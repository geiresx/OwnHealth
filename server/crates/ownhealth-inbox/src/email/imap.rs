//! Lane A: the IMAP side of email-in (SPIKES-v0.2.md b). One poll: connect (implicit TLS on 993,
//! STARTTLS on 143, or plain TCP to a loopback or private address only), log in with the source's
//! sealed password, list the folder, fetch each message whole with `BODY.PEEK[]` (so it stays unseen
//! until it is handled), ingest it in its own transaction, then flag the handled ones `\Seen` and move
//! them to the done folder (`MOVE`, or `COPY` + `\Deleted` + `EXPUNGE`). A message that fails stays in
//! the folder and is tried again on the next poll; one handled before is recognised by its Message-ID.
//!
//! Errors carry the server's answer and never the password. IDLE is not used: the poll interval
//! (60 s by default) already meets the two-minute target.

use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_imap::{Client, Session};
use futures::TryStreamExt;
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

use super::{MAX_MESSAGE_BYTES, MailboxConfig, MessageOutcome, hold_too_large, ingest_message};

/// Each IMAP step (connect, a command, a fetch) gets this long.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// At most this many messages per poll; the rest wait for the next one.
pub const MAX_MESSAGES_PER_POLL: usize = 200;

/// The stream under the IMAP client, whichever TLS mode it uses.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug> Io for T {}
type Stream = Box<dyn Io>;

fn imap_err(what: &str, e: impl Display) -> JobError {
    JobError::typed("ImapError", format!("{what}: {e}"))
}

async fn step<T, E: Display>(what: &str, f: impl Future<Output = Result<T, E>>) -> Result<T, JobError> {
    match tokio::time::timeout(STEP_TIMEOUT, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(imap_err(what, e)),
        Err(_) => Err(imap_err(what, "timed out")),
    }
}

fn private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private())
        }
    }
}

async fn tls_connector(cfg: &MailboxConfig) -> Result<TlsConnector, JobError> {
    let mut roots = RootCertStore::empty();
    match &cfg.ca_file {
        Some(path) => {
            let pem = tokio::fs::read(path)
                .await
                .map_err(|e| JobError::permanent(format!("the CA file could not be read: {e}")))?;
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let cert = cert.map_err(|e| JobError::permanent(format!("the CA file is not PEM: {e}")))?;
                roots
                    .add(cert)
                    .map_err(|e| JobError::permanent(format!("the CA file holds a bad certificate: {e}")))?;
            }
            if roots.is_empty() {
                return Err(JobError::permanent("the CA file holds no certificate"));
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| imap_err("tls", e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn wrap_tls(cfg: &MailboxConfig, tcp: TcpStream) -> Result<Stream, JobError> {
    let name = ServerName::try_from(cfg.host.clone())
        .map_err(|_| JobError::permanent("the mailbox host is not a valid name for TLS"))?;
    let tls = step("tls handshake", tls_connector(cfg).await?.connect(name, tcp)).await?;
    Ok(Box::new(tls))
}

async fn greeting<T: Io>(client: &mut Client<T>) -> Result<(), JobError> {
    match step("greeting", client.read_response()).await? {
        Some(_) => Ok(()),
        None => Err(imap_err("greeting", "the server closed the connection")),
    }
}

/// Open a logged-in session for a mailbox.
pub async fn connect(cfg: &MailboxConfig, password: &SecretString) -> Result<Session<Stream>, JobError> {
    let addrs: Vec<_> = step("resolve", tokio::net::lookup_host((cfg.host.as_str(), cfg.port)))
        .await?
        .collect();
    if addrs.is_empty() {
        return Err(imap_err("resolve", "no address"));
    }
    if cfg.tls == "none" && !addrs.iter().all(|a| private_or_loopback(a.ip())) {
        return Err(JobError::permanent(
            "tls none is only allowed for a mailbox on this machine or the local network",
        ));
    }
    let tcp = step("connect", TcpStream::connect(&addrs[..])).await?;
    let client: Client<Stream> = match cfg.tls.as_str() {
        "implicit" => {
            let mut c = Client::new(wrap_tls(cfg, tcp).await?);
            greeting(&mut c).await?;
            c
        }
        "starttls" => {
            let mut c = Client::new(tcp);
            greeting(&mut c).await?;
            step("starttls", c.run_command_and_check_ok("STARTTLS", None)).await?;
            // No greeting after STARTTLS (RFC 3501 6.2.1).
            Client::new(wrap_tls(cfg, c.into_inner()).await?)
        }
        _ => {
            let mut c = Client::new(Box::new(tcp) as Stream);
            greeting(&mut c).await?;
            c
        }
    };
    // async-imap quotes both strings; the error carries the server's answer, never the password.
    let login = client.login(&cfg.username, password.expose());
    match tokio::time::timeout(STEP_TIMEOUT, login).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err((e, _))) => Err(imap_err("login", e)),
        Err(_) => Err(imap_err("login", "timed out")),
    }
}

async fn mailbox_password(ctx: &JobContext, source: &Source) -> Result<SecretString, JobError> {
    let secret_id = source
        .secret_id
        .ok_or_else(|| JobError::permanent("the mailbox has no password; enter it again"))?;
    let sb = SecretBox::from_settings(&ctx.settings).map_err(JobError::from)?;
    let mut conn = ctx.pool.acquire().await?;
    Ok(secrets::read_secret(&mut conn, &sb, source.household_id, secret_id).await?)
}

/// One `source.poll` of an email source.
pub async fn poll(ctx: &JobContext, source: &Source) -> Result<RunOutcome, JobError> {
    let cfg = MailboxConfig::from_source(source)?;
    let password = mailbox_password(ctx, source).await?;
    let mut session = connect(&cfg, &password).await?;
    drop(password);
    let result = poll_session(ctx, source, &cfg, &mut session).await;
    let _ = tokio::time::timeout(STEP_TIMEOUT, session.logout()).await;
    result
}

fn count(run: &mut RunOutcome, m: &MessageOutcome) {
    if m.quarantine_id.is_some() {
        run.quarantined += 1;
    } else if m.already_seen || (m.document_ids.is_empty() && m.duplicates > 0) {
        run.duplicate += 1;
    } else if !m.document_ids.is_empty() {
        run.new += 1;
    }
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

async fn fetch_one(session: &mut Session<Stream>, uid: u32, query: &str) -> Result<Option<Vec<u8>>, JobError> {
    let fetched: Vec<_> = step("fetch", async {
        session.uid_fetch(uid.to_string(), query).await?.try_collect().await
    })
    .await?;
    Ok(fetched
        .iter()
        .find(|f| f.uid == Some(uid))
        .and_then(|f| f.body().or_else(|| f.header()))
        .map(<[u8]>::to_vec))
}

async fn handle(
    ctx: &JobContext,
    source: &Source,
    sb: Option<&SecretBox>,
    session: &mut Session<Stream>,
    uid: u32,
    size: usize,
) -> Result<MessageOutcome, JobError> {
    let too_large = size > MAX_MESSAGE_BYTES;
    let query = if too_large {
        "(UID BODY.PEEK[HEADER])"
    } else {
        "(UID BODY.PEEK[])"
    };
    let raw = fetch_one(session, uid, query)
        .await?
        .ok_or_else(|| imap_err("fetch", "the server sent no message"))?;
    let mut tx = ctx.pool.begin().await?;
    let out = if too_large {
        hold_too_large(&mut tx, &ctx.blobs, source, &raw).await?
    } else {
        ingest_message(&mut tx, &ctx.blobs, sb, source, &raw, None).await?
    };
    tx.commit().await?;
    Ok(out)
}

async fn poll_session(
    ctx: &JobContext,
    source: &Source,
    cfg: &MailboxConfig,
    session: &mut Session<Stream>,
) -> Result<RunOutcome, JobError> {
    let mut run = RunOutcome::default();
    let caps = step("capability", session.capabilities()).await?;
    let (has_move, has_uidplus) = (caps.has_str("MOVE"), caps.has_str("UIDPLUS"));
    step("select", session.select(&cfg.folder)).await?;
    // UNDELETED: a message we copied and flagged on a server without UIDPLUS stays until the server
    // expunges it, and must not be read again
    let mut uids: Vec<u32> = step("search", session.uid_search("UNDELETED"))
        .await?
        .into_iter()
        .collect();
    uids.sort_unstable();
    uids.truncate(MAX_MESSAGES_PER_POLL);
    if uids.is_empty() {
        return Ok(run);
    }
    let sizes: HashMap<u32, usize> = step("fetch sizes", async {
        let set = uid_set(&uids);
        let v: Vec<_> = session.uid_fetch(set, "(UID RFC822.SIZE)").await?.try_collect().await?;
        Ok::<_, async_imap::error::Error>(v.iter().filter_map(|f| Some((f.uid?, f.size? as usize))).collect())
    })
    .await?;
    let sb = SecretBox::from_settings(&ctx.settings).ok();
    let mut handled = Vec::new();
    for uid in uids {
        run.seen += 1;
        let size = sizes.get(&uid).copied().unwrap_or(0);
        match handle(ctx, source, sb.as_ref(), session, uid, size).await {
            Ok(m) => {
                count(&mut run, &m);
                run.problems.extend(super::problems(&m));
                handled.push(uid);
            }
            Err(e) => run.problems.push(RunProblem {
                code: "message_failed".into(),
                message: e.text(),
            }),
        }
    }
    if let Err(e) = move_done(session, cfg, &handled, has_move, has_uidplus).await {
        run.problems.push(RunProblem {
            code: "move_failed".into(),
            message: e.text(),
        });
    }
    Ok(run)
}

/// Flag the handled messages `\Seen` and move them to the done folder (created when missing).
async fn move_done(
    session: &mut Session<Stream>,
    cfg: &MailboxConfig,
    uids: &[u32],
    has_move: bool,
    has_uidplus: bool,
) -> Result<(), JobError> {
    if uids.is_empty() {
        return Ok(());
    }
    // An existing folder answers NO; the move below says whether the folder is really usable.
    let _ = step("create", session.create(&cfg.done_folder)).await;
    let set = uid_set(uids);
    step("store", async {
        session
            .uid_store(&set, "+FLAGS (\\Seen)")
            .await?
            .try_collect::<Vec<_>>()
            .await
    })
    .await?;
    if has_move {
        step("move", session.uid_mv(&set, &cfg.done_folder)).await?;
    } else {
        step("copy", session.uid_copy(&set, &cfg.done_folder)).await?;
        step("delete", async {
            session
                .uid_store(&set, "+FLAGS (\\Deleted)")
                .await?
                .try_collect::<Vec<_>>()
                .await
        })
        .await?;
        // plain EXPUNGE would also remove what another client flagged \Deleted in a shared folder:
        // only UID EXPUNGE of our own messages, else leave them for the server's own expunge
        if has_uidplus {
            step("expunge", async {
                session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await
            })
            .await?;
        }
    }
    Ok(())
}
