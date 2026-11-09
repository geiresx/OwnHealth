//! Lane A: email-in (ingestion.md 3.2, security-privacy.md 4.2 #7, SPIKES-v0.2.md b).
//!
//! A dedicated mailbox, polled every `source.poll_seconds` (default [`DEFAULT_POLL_SECONDS`]) over
//! IMAP (async-imap on tokio, rustls; [`imap`]). Each new message (by Message-ID,
//! `dedup::seen_item`) is stored whole as a blob first. Then [`route_message`] decides: an allowlisted
//! sender whose domain passed DKIM, SPF or DMARC in an `Authentication-Results` header stamped by the
//! trusted authserv-id (or a rule with `trust_unauthenticated`) goes to that rule's profile;
//! everything else is held in quarantine (`core::quarantine::hold`), never parsed further. Attachments
//! of allowed mail go through `core::arrive::arrive` (a locked PDF is unlocked first with that
//! sender's passwords, the copy stored with `derived_from`). Processed mail moves to the done folder
//! (`OwnHealth/Done`), flagged `\Seen`.
//!
//! The text body is not made a document of its own (ingestion.md 3.2 says it is): it stays inside
//! the stored message blob, where nothing parses it. A text document would have no importer and the
//! PDF importer would pick it up on a profile move.
//!
//! Source `config` (kind `email`): `{host, port, tls: "implicit"|"starttls"|"none", username,
//! folder?: "INBOX", done_folder?: "OwnHealth/Done", trusted_authserv_id?: string, ca_file?: path}`;
//! the password is the source's secret (purpose `imap_password`). `tls: "none"` is refused unless the
//! host is a loopback or private address.

pub mod imap;
pub mod pdf_password;

use chrono::{DateTime, Utc};
use mail_parser::MessageParser;
use ownhealth_core::arrive::{Arrival, Arrived, arrive, store_bytes};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::dedup::{record_item, seen_item};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::quarantine::{self, QuarantineIn, clean_header};
use ownhealth_core::review::{ReviewItemIn, create_item};
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use self::pdf_password::Unlock;

/// Limits of ingestion.md 3.2.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = 50;
/// A message larger than this is not downloaded: its header is held in quarantine as `too_large`.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// One minute, so a lab result reaches the review queue in under two (ROADMAP v0.2 exit criterion:
/// the poll, at most one scheduler tick of 30 s, and the import). ingestion.md says 5 minutes.
pub const DEFAULT_POLL_SECONDS: i32 = 60;
pub const DEFAULT_DONE_FOLDER: &str = "OwnHealth/Done";
pub const CHANNEL: &str = "email";
/// The review reason for a locked PDF none of the sender's passwords opened.
pub const ENCRYPTED_UNKNOWN_PASSWORD: &str = "encrypted_unknown_password";

/// MIME types that are processed (ingestion.md 3.2). `application/octet-stream` is added because lab
/// portals send PDFs with it; `arrive` sniffs the bytes, so only kinds we import are imported.
const ALLOWED_TYPES: [&str; 6] = [
    "application/pdf",
    "application/zip",
    "application/fhir+json",
    "application/json",
    "text/xml",
    "application/octet-stream",
];

/// `source.config` of an email source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxConfig {
    pub host: String,
    pub port: u16,
    /// implicit | starttls | none
    pub tls: String,
    pub username: String,
    #[serde(default = "inbox")]
    pub folder: String,
    #[serde(default = "done_folder")]
    pub done_folder: String,
    /// The authserv-id of the receiving server (RFC 8601); other Authentication-Results are ignored.
    #[serde(default)]
    pub trusted_authserv_id: Option<String>,
    /// A PEM file with the CA that signed a home server's certificate; the public roots otherwise.
    #[serde(default)]
    pub ca_file: Option<String>,
}

impl MailboxConfig {
    pub fn from_source(source: &Source) -> Result<MailboxConfig> {
        let cfg: MailboxConfig = serde_json::from_value(source.config.clone())
            .map_err(|e| Error::Invalid(format!("the mailbox settings are incomplete: {e}")))?;
        cfg.check()?;
        Ok(cfg)
    }

    pub fn check(&self) -> Result<()> {
        if !matches!(self.tls.as_str(), "implicit" | "starttls" | "none") {
            return Err(Error::Invalid("tls must be implicit, starttls or none".into()));
        }
        if self.host.trim().is_empty() || self.username.trim().is_empty() || self.port == 0 {
            return Err(Error::Invalid("host, port and username are required".into()));
        }
        if self.folder == self.done_folder {
            return Err(Error::Invalid(
                "the done folder must differ from the folder read".into(),
            ));
        }
        Ok(())
    }
}

fn inbox() -> String {
    "INBOX".into()
}

fn done_folder() -> String {
    DEFAULT_DONE_FOLDER.into()
}

/// One allowlist row (`sender_rule`).
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SenderRule {
    pub id: Uuid,
    pub source_id: Uuid,
    /// `resultados@labnorte.example` or the domain `labnorte.example`, lower case.
    pub pattern: String,
    pub profile_id: Uuid,
    pub doc_type_hint: Option<String>,
    pub trust_unauthenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SenderRuleIn {
    pub pattern: String,
    pub profile_id: Uuid,
    #[serde(default)]
    pub doc_type_hint: Option<String>,
    #[serde(default)]
    pub trust_unauthenticated: bool,
}

/// A stored PDF password, as the UI lists it: never the value.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct PdfPassword {
    pub id: Uuid,
    pub sender_rule_id: Uuid,
    pub label: Option<String>,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

/// What routing needs from a message, read from its headers only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageFacts {
    /// The From address, lower case, when the message has exactly one.
    pub from: Option<String>,
    /// More than one From address (or From header): never routed, held as `sender_not_allowed`.
    pub multiple_from: bool,
    pub message_id: Option<String>,
    /// Untrusted text.
    pub subject: Option<String>,
    /// Every `Authentication-Results` header value, verbatim.
    pub authentication_results: Vec<String>,
    /// `(filename, mime, size)`; names are untrusted text.
    pub attachments: Vec<(Option<String>, String, usize)>,
}

impl MessageFacts {
    /// What the quarantine row shows as the sender: the one From address, else the first one.
    fn display_sender(&self, first_from: Option<&str>) -> Option<String> {
        self.from.clone().or_else(|| first_from.map(str::to_lowercase))
    }
}

/// Where a message goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Routing {
    Allowed {
        rule: SenderRule,
    },
    /// A `core::quarantine::REASONS` code.
    Quarantine {
        reason: &'static str,
    },
}

/// What one message became.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MessageOutcome {
    pub quarantine_id: Option<Uuid>,
    /// Documents this message created (a locked original and its unlocked copy are both here).
    pub document_ids: Vec<Uuid>,
    /// Attachments already held as documents of the profile, and a message seen before.
    pub duplicates: i32,
    /// `attachment <n>: <code>` for each part not imported; never a file name or content.
    pub skipped: Vec<String>,
    /// The message was handled before (same Message-ID on this mailbox): nothing was done.
    pub already_seen: bool,
}

// ----------------------------------------------------------------- parsing ----

/// One MIME part to import.
struct Part {
    filename: Option<String>,
    mime: String,
    size: usize,
    inline_image: bool,
    nested_message: bool,
    bytes: Vec<u8>,
}

struct Parsed {
    facts: MessageFacts,
    first_from: Option<String>,
    parts: Vec<Part>,
}

fn parse(raw: &[u8], with_bytes: bool) -> Result<Parsed> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Invalid("not an email message".into()))?;
    let mut froms: Vec<String> = Vec::new();
    for list in msg.headers().all_from() {
        for mb in list.mailboxes() {
            if let Some(a) = mb.address() {
                froms.push(a.trim().to_lowercase());
            }
        }
    }
    let auth: Vec<String> = msg
        .headers()
        .all("Authentication-Results")
        .map(|h| unfold(&String::from_utf8_lossy(h.raw_value())))
        .collect();
    let mut parts = Vec::new();
    for a in msg.attachments() {
        let mime = a.content_type().map_or_else(
            || "application/octet-stream".to_string(),
            |c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")).to_lowercase(),
        );
        let inline_image = a.is_inline() && mime.starts_with("image/") && a.content_id().is_some();
        parts.push(Part {
            filename: a.attachment_name().map(|n| clean_header(n, 255)),
            size: a.decoded_len(),
            inline_image,
            nested_message: a.is_message(),
            bytes: if with_bytes {
                a.decoded().into_owned()
            } else {
                Vec::new()
            },
            mime,
        });
    }
    let facts = MessageFacts {
        from: (froms.len() == 1).then(|| froms[0].clone()),
        multiple_from: froms.len() > 1,
        message_id: msg.message_id().map(|m| clean_header(m.trim(), 998)),
        subject: msg.subject().map(|s| clean_header(s, quarantine::SUBJECT_MAX_CHARS)),
        authentication_results: auth,
        attachments: parts
            .iter()
            .map(|p| (p.filename.clone(), p.mime.clone(), p.size))
            .collect(),
    };
    Ok(Parsed {
        facts,
        first_from: froms.into_iter().next(),
        parts,
    })
}

fn unfold(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Read the routing facts from a raw message (RFC 5322 bytes).
pub fn message_facts(raw: &[u8]) -> Result<MessageFacts> {
    Ok(parse(raw, false)?.facts)
}

// ----------------------------------------------------------------- routing ----

fn domain_of(address: &str) -> Option<&str> {
    address.rsplit_once('@').map(|(_, d)| d).filter(|d| !d.is_empty())
}

/// Relaxed alignment: the authenticated domain is the From domain or a parent of it (with a dot, so
/// never a bare top-level domain).
fn aligned(authenticated: &str, from_domain: &str) -> bool {
    let a = authenticated.trim().trim_end_matches('.').to_lowercase();
    a.contains('.') && (a == from_domain || from_domain.ends_with(&format!(".{a}")))
}

/// RFC 5322 comments `( ... )` out, nested ones too, quoted strings kept.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for c in s.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                quoted = !quoted;
                out.push(c);
            }
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthVerdict {
    /// The trusted server saw DKIM, SPF or DMARC pass for the From domain.
    Pass,
    /// The trusted server saw a failure for the From domain and no pass.
    Fail,
    /// No header from the trusted server says anything about this domain.
    None,
}

/// Read the `Authentication-Results` header (RFC 8601) the trusted server stamped: only the topmost one
/// with its authserv-id, which is the copy the receiving server prepended. A lower copy with the same
/// id came with the message (a forgery the server did not strip) and is never read.
fn auth_verdict(headers: &[String], trusted: &str, from_domain: &str) -> AuthVerdict {
    let mut verdict = AuthVerdict::None;
    let trusted_header = headers.iter().map(|h| strip_comments(h)).find(|h| {
        let authserv = h
            .split(';')
            .next()
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        authserv.eq_ignore_ascii_case(trusted.trim())
    });
    if let Some(h) = trusted_header {
        for section in h.split(';').skip(1) {
            let mut words = section.split_whitespace();
            let Some((method, result)) = words.next().and_then(|w| w.split_once('=')) else {
                continue;
            };
            let method = method.split('/').next().unwrap_or("").to_ascii_lowercase();
            let result = result.to_ascii_lowercase();
            let props: Vec<(String, String)> = words
                .filter_map(|w| w.split_once('='))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim_matches('"').to_lowercase()))
                .collect();
            let prop = |names: &[&str]| {
                props
                    .iter()
                    .find(|(k, _)| names.contains(&k.as_str()))
                    .map(|(_, v)| v.clone())
            };
            let domain = match method.as_str() {
                "dkim" => prop(&["header.d"]).or_else(|| prop(&["header.i"]).map(|i| domain_or_self(&i))),
                "spf" => prop(&["smtp.mailfrom"]).map(|m| domain_or_self(&m)),
                "dmarc" => prop(&["header.from"]).map(|m| domain_or_self(&m)),
                _ => continue,
            };
            let Some(domain) = domain else { continue };
            if !aligned(&domain, from_domain) {
                continue;
            }
            match result.as_str() {
                "pass" => return AuthVerdict::Pass,
                "fail" | "softfail" | "permerror" | "policy" => verdict = AuthVerdict::Fail,
                _ => {}
            }
        }
    }
    verdict
}

fn domain_or_self(v: &str) -> String {
    domain_of(v).unwrap_or(v).to_string()
}

fn find_rule<'r>(from: &str, rules: &'r [SenderRule]) -> Option<&'r SenderRule> {
    let domain = domain_of(from)?;
    rules
        .iter()
        .find(|r| r.pattern == from)
        .or_else(|| rules.iter().find(|r| !r.pattern.contains('@') && r.pattern == domain))
}

/// The allowlist decision (pure). Sender match: an address rule beats a domain rule.
pub fn route_message(facts: &MessageFacts, rules: &[SenderRule], trusted_authserv_id: Option<&str>) -> Routing {
    let quarantine = |reason| Routing::Quarantine { reason };
    let Some(from) = facts.from.as_deref().filter(|_| !facts.multiple_from) else {
        return quarantine("sender_not_allowed");
    };
    let Some(rule) = find_rule(from, rules) else {
        return quarantine("sender_not_allowed");
    };
    let from_domain = domain_of(from).unwrap_or("");
    let verdict = trusted_authserv_id
        .filter(|t| !t.trim().is_empty())
        .map_or(AuthVerdict::None, |t| {
            auth_verdict(&facts.authentication_results, t, from_domain)
        });
    match verdict {
        AuthVerdict::Pass => Routing::Allowed { rule: rule.clone() },
        // The receiving server said this mail is forged: never trusted, whatever the rule says.
        AuthVerdict::Fail => quarantine("auth_failed"),
        AuthVerdict::None if rule.trust_unauthenticated => Routing::Allowed { rule: rule.clone() },
        AuthVerdict::None => quarantine("auth_failed"),
    }
}

// --------------------------------------------------------------- ingesting ----

fn external_id(facts: &MessageFacts, raw: &[u8]) -> String {
    match facts.message_id.as_deref().filter(|m| !m.is_empty()) {
        Some(m) => format!("mid:{m}"),
        None => format!("sha:{}", hex::encode(Sha256::digest(raw))),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("email worker task failed: {e}")))?
}

/// The passwords of one sender rule, in order. Without the secrets key none can be read.
async fn passwords_for(
    conn: &mut PgConnection,
    sb: Option<&SecretBox>,
    household_id: Uuid,
    rule_id: Uuid,
) -> Result<Vec<SecretString>> {
    let Some(sb) = sb else { return Ok(Vec::new()) };
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select secret_id from pdf_password_rule where sender_rule_id = $1 order by position, created_at",
    )
    .bind(rule_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(secrets::read_secret(&mut *conn, sb, household_id, id).await?);
    }
    Ok(out)
}

struct Target<'a> {
    source: &'a Source,
    profile_id: Uuid,
    rule: Option<&'a SenderRule>,
    message_id: Option<&'a str>,
}

/// Count what `arrive` did with one attachment.
fn tally(out: &mut MessageOutcome, n: usize, arrived: Arrived) {
    match arrived {
        Arrived::Queued { document_id, .. } => out.document_ids.extend(document_id),
        Arrived::Duplicate { .. } | Arrived::AlreadySeen { .. } => out.duplicates += 1,
        Arrived::Unsupported { .. } => out.skipped.push(format!("attachment {n}: unsupported_type")),
    }
}

async fn arrive_bytes(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    t: &Target<'_>,
    filename: Option<&str>,
    bytes: Vec<u8>,
    derived_from: Option<Uuid>,
) -> Result<Arrived> {
    let (sha, size) = store_bytes(blobs, bytes).await?;
    arrive(
        conn,
        blobs,
        &Arrival {
            source_id: Some(t.source.id),
            source_ref: t.message_id,
            filename,
            derived_from,
            ..Arrival::new(t.source.household_id, t.profile_id, CHANNEL, &sha, size as i64)
        },
    )
    .await
}

/// A locked PDF: keep the original as a document of its own (never imported), then import the
/// unlocked copy with `derived_from`, or file `encrypted_document` when no password opened it.
async fn arrive_locked(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    sb: Option<&SecretBox>,
    t: &Target<'_>,
    n: usize,
    part: Part,
    out: &mut MessageOutcome,
) -> Result<()> {
    let passwords = match t.rule {
        Some(rule) => passwords_for(&mut *conn, sb, t.source.household_id, rule.id).await?,
        None => Vec::new(),
    };
    let tried = passwords.len();
    let locked = part.bytes;
    let (unlock, locked) = blocking(move || Ok((pdf_password::unlock(&locked, &passwords)?, locked))).await?;
    let (sha, size) = store_bytes(blobs, locked).await?;
    let original = create_document(
        &mut *conn,
        &NewDocument {
            filename: part.filename.as_deref(),
            channel: CHANNEL,
            source_ref: t.message_id,
            source_id: Some(t.source.id),
            ..NewDocument::upload(
                t.source.household_id,
                Some(t.profile_id),
                &sha,
                "application/pdf",
                size as i64,
            )
        },
    )
    .await?;
    if !original.created && !original.reactivated {
        out.duplicates += 1;
        return Ok(());
    }
    sqlx::query("update document set flags = flags || '{\"encrypted\": true}'::jsonb where id = $1")
        .bind(original.id)
        .execute(&mut *conn)
        .await?;
    out.document_ids.push(original.id);
    match unlock {
        Unlock::Unlocked { pdf, .. } => {
            let arrived = arrive_bytes(conn, blobs, t, part.filename.as_deref(), pdf, Some(original.id)).await?;
            tally(out, n, arrived);
        }
        Unlock::NoPasswordWorked | Unlock::NotEncrypted => {
            create_item(
                conn,
                &ReviewItemIn {
                    kind: "encrypted_document".into(),
                    household_id: Some(t.source.household_id),
                    profile_id: Some(t.profile_id),
                    payload: json!({
                        "document_id": original.id.to_string(),
                        "source_id": t.source.id.to_string(),
                        "sender_rule_id": t.rule.map(|r| r.id.to_string()),
                        "passwords_tried": tried,
                    }),
                    reasons: vec![ENCRYPTED_UNKNOWN_PASSWORD.into()],
                    document_id: Some(original.id),
                    ref_table: Some("document".into()),
                    ref_id: Some(original.id),
                    ..ReviewItemIn::default()
                },
            )
            .await?;
            out.skipped
                .push(format!("attachment {n}: {ENCRYPTED_UNKNOWN_PASSWORD}"));
        }
    }
    Ok(())
}

async fn import_parts(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    sb: Option<&SecretBox>,
    t: &Target<'_>,
    parts: Vec<Part>,
    out: &mut MessageOutcome,
) -> Result<()> {
    let total = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        let n = i + 1;
        if i >= MAX_ATTACHMENTS {
            out.skipped
                .push(format!("attachments {n}-{total}: too_many_attachments"));
            break;
        }
        let code = if part.nested_message {
            Some("attached_message")
        } else if part.inline_image {
            Some("inline_image")
        } else if part.size > MAX_ATTACHMENT_BYTES || part.bytes.len() > MAX_ATTACHMENT_BYTES {
            Some("too_large")
        } else if !(part.mime.starts_with("image/") || ALLOWED_TYPES.contains(&part.mime.as_str())) {
            Some("unsupported_type")
        } else {
            None
        };
        if let Some(code) = code {
            out.skipped.push(format!("attachment {n}: {code}"));
            continue;
        }
        if part.bytes.starts_with(b"%PDF-") {
            let probe = part.bytes.clone();
            if blocking(move || Ok(pdf_password::is_encrypted(&probe))).await? {
                arrive_locked(&mut *conn, blobs, sb, t, n, part, out).await?;
                continue;
            }
        }
        let arrived = arrive_bytes(&mut *conn, blobs, t, part.filename.as_deref(), part.bytes, None).await?;
        tally(out, n, arrived);
    }
    Ok(())
}

/// One raw message from a mailbox: store, route, quarantine or arrive. `released_to` skips the
/// routing for a message a person released into that profile. Runs in the caller's transaction.
pub async fn ingest_message(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    sb: Option<&SecretBox>,
    source: &Source,
    raw: &[u8],
    released_to: Option<Uuid>,
) -> Result<MessageOutcome> {
    if source.kind != "email" {
        return Err(Error::Invalid("not an email source".into()));
    }
    let trusted = source
        .config
        .get("trusted_authserv_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let owned = raw.to_vec();
    let (parsed, owned) = blocking(move || {
        // Headers only until the routing allows the message: quarantined mail is never decoded.
        let p = parse(&owned, false)?;
        Ok((p, owned))
    })
    .await?;
    let ext = external_id(&parsed.facts, &owned);
    let mut out = MessageOutcome::default();
    if released_to.is_none() && seen_item(&mut *conn, source.id, &ext).await?.is_some() {
        out.already_seen = true;
        out.duplicates = 1;
        return Ok(out);
    }
    let (raw_sha, _) = store_bytes(blobs, owned.clone()).await?;
    let rules = list_rules(&mut *conn, source.id).await?;
    let (rule, profile_id) = match released_to {
        Some(p) => (
            parsed.facts.from.as_deref().and_then(|f| find_rule(f, &rules)).cloned(),
            p,
        ),
        None => match route_message(&parsed.facts, &rules, trusted.as_deref()) {
            Routing::Allowed { rule } => {
                let p = rule.profile_id;
                (Some(rule), p)
            }
            Routing::Quarantine { reason } => {
                out.quarantine_id = Some(hold(&mut *conn, source, &parsed, &raw_sha, reason).await?);
                record_item(&mut *conn, source.id, &ext, Some(&raw_sha), None, out.quarantine_id).await?;
                return Ok(out);
            }
        },
    };
    let parts = blocking(move || Ok(parse(&owned, true)?.parts)).await?;
    let target = Target {
        source,
        profile_id,
        rule: rule.as_ref(),
        message_id: parsed.facts.message_id.as_deref(),
    };
    import_parts(&mut *conn, blobs, sb, &target, parts, &mut out).await?;
    record_item(
        &mut *conn,
        source.id,
        &ext,
        Some(&raw_sha),
        out.document_ids.first().copied(),
        None,
    )
    .await?;
    Ok(out)
}

async fn hold(conn: &mut PgConnection, source: &Source, parsed: &Parsed, raw_sha: &str, reason: &str) -> Result<Uuid> {
    let attachments: Vec<Value> = parsed
        .facts
        .attachments
        .iter()
        .map(|(f, m, s)| json!({"filename": f, "mime": m, "size": s}))
        .collect();
    let sender = parsed.facts.display_sender(parsed.first_from.as_deref());
    quarantine::hold(
        conn,
        &QuarantineIn {
            household_id: source.household_id,
            source_id: Some(source.id),
            raw_sha256: raw_sha,
            message_id: parsed.facts.message_id.as_deref(),
            sender: sender.as_deref(),
            subject: parsed.facts.subject.as_deref(),
            attachments: Value::Array(attachments),
            reason,
        },
    )
    .await
}

/// Hold a message too large to download: only its header is stored.
pub async fn hold_too_large(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    source: &Source,
    header: &[u8],
) -> Result<MessageOutcome> {
    let owned = header.to_vec();
    let (parsed, owned) = blocking(move || Ok((parse(&owned, false)?, owned))).await?;
    let ext = external_id(&parsed.facts, &owned);
    let mut out = MessageOutcome::default();
    if seen_item(&mut *conn, source.id, &ext).await?.is_some() {
        out.already_seen = true;
        out.duplicates = 1;
        return Ok(out);
    }
    let (sha, _) = store_bytes(blobs, owned).await?;
    out.quarantine_id = Some(hold(&mut *conn, source, &parsed, &sha, "too_large").await?);
    record_item(&mut *conn, source.id, &ext, Some(&sha), None, out.quarantine_id).await?;
    Ok(out)
}

/// The `email` poller: log in, fetch the folder's mail, ingest each message, move it to the done folder.
pub async fn poll_mailbox(ctx: JobContext, source: Source) -> std::result::Result<RunOutcome, JobError> {
    imap::poll(&ctx, &source).await
}

/// Release a held message into `profile_id`; with `allow_sender`, also add its sender to the
/// allowlist for that profile. Returns what the message produced (its documents, and the parts it
/// skipped). Needs a person (the route checks the role and step-up).
pub async fn release(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    sb: Option<&SecretBox>,
    quarantine_id: Uuid,
    user_id: Uuid,
    profile_id: Uuid,
    allow_sender: bool,
) -> Result<MessageOutcome> {
    let item = quarantine::lock_held(&mut *conn, quarantine_id).await?;
    if item.reason == "too_large" {
        // only the header was downloaded: releasing it would import nothing and lose the message
        return Err(Error::Review(
            "This message was too large to download, so only its header was kept. It is in the mailbox's \
             done folder: save the attachment from there and upload it by hand."
                .into(),
        ));
    }
    let household: Option<Uuid> = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(profile_id)
        .fetch_optional(&mut *conn)
        .await?;
    if household != Some(item.household_id) {
        return Err(Error::Invalid("the profile is not in this message's household".into()));
    }
    let source_id = item
        .source_id
        .ok_or_else(|| Error::Review("The mailbox this message came from was removed.".into()))?;
    let source = ownhealth_core::sources::get_source(&mut *conn, source_id)
        .await?
        .ok_or_else(|| Error::Review("The mailbox this message came from was removed.".into()))?;
    if allow_sender {
        // without a trusted authserv-id nothing authenticates, so a rule that requires it would hold
        // this sender's next message again
        let trusted = source
            .config
            .get("trusted_authserv_id")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty());
        if !trusted {
            return Err(Error::Invalid(
                "this mailbox has no trusted receiving server (authserv-id), so the sender's next message \
                 would be held again: set it in the mailbox settings, or add the sender with \
                 'trust unauthenticated mail'"
                    .into(),
            ));
        }
        let sender = item
            .sender
            .as_deref()
            .ok_or_else(|| Error::Invalid("this message has no sender to allow".into()))?;
        insert_rule(
            &mut *conn,
            source.id,
            &SenderRuleIn {
                pattern: sender.to_string(),
                profile_id,
                doc_type_hint: None,
                trust_unauthenticated: false,
            },
            Some(user_id),
        )
        .await?;
    }
    let sha = item.raw_sha256_hex();
    let b = blobs.clone();
    let raw = blocking(move || b.get_bytes(&sha).map_err(Error::from)).await?;
    let out = ingest_message(&mut *conn, blobs, sb, &source, &raw, Some(profile_id)).await?;
    quarantine::mark_released(&mut *conn, quarantine_id, user_id, &out.document_ids).await?;
    Ok(out)
}

/// What a person should see about a handled message that did not import everything: one problem per
/// skipped part (never a file name), and one when nothing came of an allowed message at all. A part
/// the message embeds in its text (`inline_image`, a logo) is not reported on its own, and a locked
/// PDF already has its review item.
pub fn problems(m: &MessageOutcome) -> Vec<RunProblem> {
    if m.quarantine_id.is_some() || m.already_seen {
        return Vec::new();
    }
    let mut out: Vec<RunProblem> = m
        .skipped
        .iter()
        .filter(|s| !s.ends_with(": inline_image") && !s.ends_with(ENCRYPTED_UNKNOWN_PASSWORD))
        .map(|s| RunProblem {
            code: "attachment_skipped".into(),
            message: s.clone(),
        })
        .collect();
    if m.document_ids.is_empty() && m.duplicates == 0 {
        out.push(RunProblem {
            code: "no_importable_attachment".into(),
            message: "an allowed message brought no file that could be imported".into(),
        });
    }
    out
}

// ------------------------------------------------------------------- rules ----

const RULE_COLS: &str = "id, source_id, pattern, profile_id, doc_type_hint, trust_unauthenticated";

pub async fn list_rules(conn: &mut PgConnection, source_id: Uuid) -> Result<Vec<SenderRule>> {
    let sql = format!("select {RULE_COLS} from sender_rule where source_id = $1 order by pattern");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(source_id)
        .fetch_all(conn)
        .await?)
}

pub async fn get_rule(conn: &mut PgConnection, rule_id: Uuid) -> Result<Option<SenderRule>> {
    let sql = format!("select {RULE_COLS} from sender_rule where id = $1");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(rule_id)
        .fetch_optional(conn)
        .await?)
}

/// `resultados@labnorte.example` or `labnorte.example`, trimmed and lower case; the same shape the
/// `sender_rule.pattern` check accepts.
pub fn normalize_pattern(pattern: &str) -> Result<String> {
    let p = pattern.trim().trim_start_matches('@').to_lowercase();
    let (local, domain) = match p.rsplit_once('@') {
        Some((l, d)) => (Some(l), d),
        None => (None, p.as_str()),
    };
    let bad = |s: &str| s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '@' || c.is_control());
    let domain_ok = !bad(domain)
        && domain
            .split_once('.')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.starts_with('.'));
    if local.is_some_and(bad) || !domain_ok || p.len() > 320 {
        return Err(Error::Invalid(
            "a sender is an address (name@lab.example) or a domain (lab.example)".into(),
        ));
    }
    Ok(p)
}

/// Insert a rule; an existing one for the same pattern is left as it is. Returns the rule id (new or
/// existing) and whether it is new.
async fn insert_rule(
    conn: &mut PgConnection,
    source_id: Uuid,
    rule: &SenderRuleIn,
    user_id: Option<Uuid>,
) -> Result<(Uuid, bool)> {
    let pattern = normalize_pattern(&rule.pattern)?;
    let same_household: Option<bool> = sqlx::query_scalar(
        "select p.household_id = s.household_id from source s, profile p where s.id = $1 and p.id = $2",
    )
    .bind(source_id)
    .bind(rule.profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    if same_household != Some(true) {
        return Err(Error::Invalid("the profile is not in this mailbox's household".into()));
    }
    let id: Option<Uuid> = sqlx::query_scalar(
        "insert into sender_rule (source_id, pattern, profile_id, doc_type_hint, trust_unauthenticated, created_by) \
         values ($1, $2, $3, $4, $5, $6) on conflict (source_id, pattern) do nothing returning id",
    )
    .bind(source_id)
    .bind(&pattern)
    .bind(rule.profile_id)
    .bind(rule.doc_type_hint.as_deref().map(|h| clean_header(h, 64)))
    .bind(rule.trust_unauthenticated)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    match id {
        Some(id) => Ok((id, true)),
        None => Ok((
            sqlx::query_scalar("select id from sender_rule where source_id = $1 and pattern = $2")
                .bind(source_id)
                .bind(&pattern)
                .fetch_one(conn)
                .await?,
            false,
        )),
    }
}

/// Add a sender to a mailbox's allowlist. A sender already listed is a conflict.
pub async fn add_rule(conn: &mut PgConnection, source_id: Uuid, rule: &SenderRuleIn, user_id: Uuid) -> Result<Uuid> {
    match insert_rule(conn, source_id, rule, Some(user_id)).await? {
        (id, true) => Ok(id),
        (_, false) => Err(Error::Review("This sender is already on the list.".into())),
    }
}

/// Delete a rule and the PDF passwords stored for it.
pub async fn delete_rule(conn: &mut PgConnection, rule_id: Uuid) -> Result<bool> {
    sqlx::query("delete from secret where id in (select secret_id from pdf_password_rule where sender_rule_id = $1)")
        .bind(rule_id)
        .execute(&mut *conn)
        .await?;
    let n = sqlx::query("delete from sender_rule where id = $1")
        .bind(rule_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

pub async fn list_pdf_passwords(conn: &mut PgConnection, sender_rule_id: Uuid) -> Result<Vec<PdfPassword>> {
    Ok(sqlx::query_as(
        "select r.id, r.sender_rule_id, s.label, r.position, r.created_at from pdf_password_rule r \
         join secret s on s.id = r.secret_id where r.sender_rule_id = $1 order by r.position, r.created_at",
    )
    .bind(sender_rule_id)
    .fetch_all(conn)
    .await?)
}

/// Store a PDF password for one sender rule (sealed, purpose `pdf_password`). Returns the rule row id.
pub async fn add_pdf_password(
    conn: &mut PgConnection,
    sb: &SecretBox,
    sender_rule_id: Uuid,
    password: &SecretString,
    label: Option<&str>,
) -> Result<Uuid> {
    let len = password.expose().chars().count();
    if len == 0 || len > 256 {
        return Err(Error::Invalid("a PDF password has 1 to 256 characters".into()));
    }
    let household: Option<Uuid> = sqlx::query_scalar(
        "select s.household_id from sender_rule r join source s on s.id = r.source_id where r.id = $1",
    )
    .bind(sender_rule_id)
    .fetch_optional(&mut *conn)
    .await?;
    let household = household.ok_or_else(|| Error::NotFound("Unknown sender rule.".into()))?;
    let label = label.map(|l| clean_header(l, 100)).filter(|l| !l.trim().is_empty());
    let secret_id =
        secrets::store_secret(&mut *conn, sb, household, "pdf_password", label.as_deref(), password).await?;
    Ok(sqlx::query_scalar(
        "insert into pdf_password_rule (sender_rule_id, secret_id, position) values ($1, $2, \
         (select coalesce(max(position) + 1, 0) from pdf_password_rule where sender_rule_id = $1)) returning id",
    )
    .bind(sender_rule_id)
    .bind(secret_id)
    .fetch_one(conn)
    .await?)
}

/// Delete a password rule and its sealed secret.
pub async fn delete_pdf_password(conn: &mut PgConnection, rule_id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from secret where id = (select secret_id from pdf_password_rule where id = $1)")
        .bind(rule_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

// -------------------------------------------------------------------- demo ----

/// The demo mailbox (RUST-CONTRACTS 10.6): LabNorte allowed for Alex, and one message from an unknown
/// sender held in quarantine. Disabled, so the demo never tries to log in anywhere.
async fn seed_demo(conn: &mut PgConnection, ctx: &mut ownhealth_core::demo::DemoContext) -> Result<()> {
    let household = ctx.family.household_id;
    let source_id = ownhealth_core::sources::create_source(
        &mut *conn,
        &ownhealth_core::sources::NewSource {
            config: json!({"host": "imap.correo.example", "port": 993, "tls": "implicit",
                           "username": "salud.rivera@correo.example", "trusted_authserv_id": "mx.correo.example"}),
            poll_seconds: Some(DEFAULT_POLL_SECONDS),
            ..ownhealth_core::sources::NewSource::new(household, "email", "Correo de salud")
        },
    )
    .await?;
    sqlx::query("update source set enabled = false where id = $1")
        .bind(source_id)
        .execute(&mut *conn)
        .await?;
    insert_rule(
        &mut *conn,
        source_id,
        &SenderRuleIn {
            pattern: "labnorte.example".into(),
            profile_id: ctx.family.profile("alex"),
            doc_type_hint: Some("lab_report".into()),
            trust_unauthenticated: false,
        },
        None,
    )
    .await?;
    let raw = demo_message();
    let blobs = ctx.blobs.clone();
    let sha = blocking(move || blobs.put_bytes(&raw).map_err(Error::from)).await?;
    let source = ownhealth_core::sources::get_source(&mut *conn, source_id)
        .await?
        .ok_or_else(|| Error::NotFound("demo mailbox".into()))?;
    let parsed = parse(&demo_message(), false)?;
    hold(&mut *conn, &source, &parsed, &sha, "sender_not_allowed").await?;
    Ok(())
}

fn demo_message() -> Vec<u8> {
    "Message-ID: <promo-0001@ofertas-salud.example>\r\n\
         From: Ofertas Salud <promo@ofertas-salud.example>\r\n\
         To: salud.rivera@correo.example\r\n\
         Subject: Su analitica gratis: abra el adjunto\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Mensaje de ejemplo retenido en cuarentena.\r\n"
        .as_bytes()
        .to_vec()
}

pub fn register() {
    ownhealth_core::sources::register_poller("email", |ctx, source| Box::pin(poll_mailbox(ctx, source)));
    ownhealth_core::demo::register_demo_hook("email", 1010, |conn, ctx| Box::pin(seed_demo(conn, ctx)));
}

#[cfg(test)]
mod tests;
