//! People and logins (`auth/household.py`): first-run owner, invites, relatives without a login,
//! claiming, deletion.
//!
//! Who manages a profile (grants, locks, sensitive switches, export, deletion): its `owner`; while
//! nobody has claimed the profile (`profile.user_id is null`: a child, an elderly parent), a
//! `guardian`. A `caregiver` reads and imports but never manages; it may only invite the person to
//! claim their own profile.

use std::collections::BTreeSet;

use chrono::NaiveDate;
use ownhealth_core::access::{
    Permission, Profile, ProfileIn, create_household, create_profile, get_profile, majority, set_genome_switches,
    set_grant,
};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::blobs::sha256_hex_to_bytes;
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue, register_handler};
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::challenges::{self, Challenge, Issue};
use crate::{AuthError, AuthResult, sha256, truncate_chars};

pub const INVITE_TTL_DAYS: i64 = 7;
/// Advisory lock id for first-run setup.
const SETUP_LOCK: i64 = 0x0E_0600;

pub fn can_manage(perm: &Permission) -> bool {
    perm.role == "owner" || (perm.role == "guardian" && perm.profile.user_id.is_none())
}

pub fn require_manage(perm: Permission) -> Result<Permission> {
    if !can_manage(&perm) {
        return Err(Error::access_denied(
            "forbidden",
            format!("The role {} cannot manage this profile.", perm.role),
        ));
    }
    Ok(perm)
}

/// The first word of the name, ASCII-folded, as a slug (`slugify`).
pub fn slugify(name: &str) -> String {
    let ascii: String = name.nfkd().filter(char::is_ascii).collect::<String>().to_lowercase();
    let first = ascii.split_whitespace().next().unwrap_or("person");
    let mut slug = String::new();
    let mut dash = false;
    for c in first.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !slug.is_empty() {
                slug.push('-');
            }
            dash = false;
            slug.push(c);
        } else {
            dash = true;
        }
    }
    // Python strips the dashes, then cuts at 40 (a dash at the cut stays)
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() || slug == "me" {
        "person".into()
    } else {
        slug
    }
}

/// `^[a-z0-9][a-z0-9-]{0,39}$`
fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub async fn unique_slug(conn: &mut PgConnection, household_id: Uuid, wanted: &str) -> Result<String> {
    let base = if !is_slug(wanted) || wanted == "me" {
        slugify(wanted)
    } else {
        wanted.to_string()
    };
    let taken: BTreeSet<String> = sqlx::query_scalar::<_, String>("select slug from profile where household_id = $1")
        .bind(household_id)
        .fetch_all(conn)
        .await?
        .into_iter()
        .collect();
    let (mut slug, mut n) = (base.clone(), 2);
    while taken.contains(&slug) {
        let suffix = format!("-{n}");
        let keep: String = base.chars().take(40 - suffix.len()).collect();
        slug = format!("{keep}{suffix}");
        n += 1;
    }
    Ok(slug)
}

pub async fn users_exist(conn: &mut PgConnection) -> Result<bool> {
    Ok(sqlx::query_scalar("select exists (select 1 from app_user)")
        .fetch_one(conn)
        .await?)
}

fn locale_or_es(locale: &str) -> &str {
    if matches!(locale, "es" | "en") { locale } else { "es" }
}

// ------------------------------------------------------------------ first run ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub household_id: Uuid,
    pub user_id: Uuid,
    pub profile_id: Uuid,
    pub slug: String,
}

#[derive(Debug, Clone, Default)]
pub struct OwnerIn<'a> {
    pub user_id: Option<Uuid>,
    pub household_name: &'a str,
    pub display_name: &'a str,
    pub locale: &'a str,
    pub oidc_subject: Option<&'a str>,
    pub birth_date: Option<NaiveDate>,
    pub sex_at_birth: Option<&'a str>,
    pub country: Option<&'a str>,
}

/// Create the household, its first login and that person's own profile, as owner.
///
/// Only while the instance has no user at all; two racing setups serialise on an advisory lock.
/// The lock lasts until the caller's transaction ends (a savepoint here inside one), or until the
/// owner exists when `conn` is in autocommit (a transaction of its own here).
pub async fn setup_owner(conn: &mut PgConnection, o: &OwnerIn<'_>) -> AuthResult<Owner> {
    let mut tx = conn.begin().await?;
    let owner = setup_owner_locked(&mut tx, o).await?;
    tx.commit().await?;
    Ok(owner)
}

async fn setup_owner_locked(conn: &mut PgConnection, o: &OwnerIn<'_>) -> AuthResult<Owner> {
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(SETUP_LOCK)
        .execute(&mut *conn)
        .await?;
    if users_exist(&mut *conn).await? {
        return Err(AuthError::refused(
            "already_set_up",
            "This instance already has an owner. Log in instead.",
        ));
    }
    let display_name = truncate_chars(o.display_name.trim(), 80);
    if display_name.is_empty() {
        return Err(AuthError::refused("bad_input", "A name is needed."));
    }
    let hh_name = truncate_chars(o.household_name.trim(), 80);
    let h = create_household(&mut *conn, if hh_name.is_empty() { display_name } else { hh_name }).await?;
    let uid = o.user_id.unwrap_or_else(Uuid::new_v4);
    sqlx::query(
        "insert into app_user (id, household_id, display_name, locale, oidc_subject) values ($1, $2, $3, $4, $5)",
    )
    .bind(uid)
    .bind(h)
    .bind(display_name)
    .bind(locale_or_es(o.locale))
    .bind(o.oidc_subject)
    .execute(&mut *conn)
    .await?;
    let slug = unique_slug(&mut *conn, h, display_name).await?;
    let pid = create_profile(
        &mut *conn,
        &ProfileIn {
            household_id: h,
            slug: slug.clone(),
            display_name: display_name.into(),
            relationship: Some("self".into()),
            birth_date: o.birth_date,
            sex_at_birth: o.sex_at_birth.map(str::to_string),
            country: o.country.map(str::to_string),
            user_id: Some(uid),
            unit_pref: None,
        },
    )
    .await?;
    set_grant(&mut *conn, uid, pid, "owner", None, None, None).await?;
    Ok(Owner {
        household_id: h,
        user_id: uid,
        profile_id: pid,
        slug,
    })
}

// -------------------------------------------------------------------- invites ----

/// Create a login that has nothing yet (grants come separately) and a 7-day invite code.
///
/// With `claim_profile_id` the invited person becomes that profile's owner when they enrol.
pub async fn invite_member(
    conn: &mut PgConnection,
    household_id: Uuid,
    invited_by: Uuid,
    display_name: &str,
    locale: &str,
    claim_profile_id: Option<Uuid>,
) -> AuthResult<(Uuid, String)> {
    let display_name = truncate_chars(display_name.trim(), 80);
    if display_name.is_empty() {
        return Err(AuthError::refused("bad_input", "A name is needed."));
    }
    if let Some(pid) = claim_profile_id {
        let claimed: Option<Option<Uuid>> =
            sqlx::query_scalar("select user_id from profile where id = $1 and household_id = $2")
                .bind(pid)
                .bind(household_id)
                .fetch_optional(&mut *conn)
                .await?;
        if !matches!(claimed, Some(None)) {
            return Err(AuthError::refused(
                "already_claimed",
                "That profile already belongs to a login.",
            ));
        }
    }
    let uid: Uuid = sqlx::query_scalar(
        "insert into app_user (household_id, display_name, locale) values ($1, $2, $3) returning id",
    )
    .bind(household_id)
    .bind(display_name)
    .bind(locale_or_es(locale))
    .fetch_one(&mut *conn)
    .await?;
    let code = crate::token_urlsafe(24);
    let mut data = Map::new();
    data.insert("invited_by".into(), json!(invited_by.to_string()));
    data.insert(
        "claim_profile_id".into(),
        json!(claim_profile_id.map(|p| p.to_string())),
    );
    challenges::issue(
        conn,
        "invite",
        &sha256(&code),
        Issue {
            user_id: Some(uid),
            data,
            ttl_seconds: INVITE_TTL_DAYS * 86_400,
            ..Issue::default()
        },
    )
    .await?;
    Ok((uid, code))
}

pub async fn peek_invite(conn: &mut PgConnection, code: &str) -> AuthResult<Option<Challenge>> {
    challenges::peek(conn, &sha256(code), &["invite"]).await
}

fn invite_invalid(message: &str) -> AuthError {
    AuthError::refused("invite_invalid", message)
}

/// Consume the invite; claim its profile if it names one. Returns the invited user's id.
pub async fn redeem_invite(conn: &mut PgConnection, code: &str) -> AuthResult<Uuid> {
    let ch = match challenges::consume(&mut *conn, &sha256(code), &["invite"]).await {
        Ok(ch) => ch,
        Err(AuthError::Refused { .. }) => {
            return Err(invite_invalid("This invite link expired or was already used."));
        }
        Err(e) => return Err(e),
    };
    let user_id = ch
        .user_id
        .ok_or_else(|| invite_invalid("This invite link expired or was already used."))?;
    if let Some(claim) = ch.text("claim_profile_id") {
        let parse = |s: Option<&str>| s.and_then(|v| Uuid::parse_str(v).ok());
        let (Some(claim), Some(inviter)) = (parse(Some(claim)), parse(ch.text("invited_by"))) else {
            return Err(invite_invalid("This invite link expired or was already used."));
        };
        if !claim_invite_allowed(&mut *conn, claim, inviter).await? {
            return Err(invite_invalid(
                "Someone else manages this profile now. Ask them for a new invite.",
            ));
        }
        claim_profile(conn, claim, user_id, Some(inviter), ownhealth_core::access::today()).await?;
    }
    Ok(user_id)
}

/// A claim invite counts when its sender manages the profile (owner, or guardian of an unclaimed
/// one), or when nobody does. A caregiver's own invite must not turn a login they hold into the owner
/// of a profile its guardians manage.
pub async fn claim_invite_allowed(conn: &mut PgConnection, profile_id: Uuid, inviter: Uuid) -> Result<bool> {
    let ok: Option<bool> = sqlx::query_scalar(
        "select exists (select 1 from profile_grant where profile_id = $1 and user_id = $2 \
         and role in ('owner', 'guardian') and (expires_at is null or expires_at > now())) \
         or not exists (select 1 from profile_grant where profile_id = $1 \
         and role in ('owner', 'guardian') and (expires_at is null or expires_at > now()))",
    )
    .bind(profile_id)
    .bind(inviter)
    .fetch_one(conn)
    .await?;
    Ok(ok.unwrap_or(false))
}

/// A child's profile whose person has reached the household's age of majority on `on`: she takes it
/// over when she claims it. A relative with no birth date, or who is not the household's child (an
/// elderly parent), is not.
pub fn at_majority(profile: &Profile, majority: i32, on: NaiveDate) -> bool {
    profile.relationship.as_deref() == Some("child") && profile.birth_date.is_some() && !profile.is_minor(on, majority)
}

/// The profiles among `perms` that wait for their person to take them over: managed by the caller as
/// guardian, nobody's login yet, and past the age of majority. `invited` is true while a claim invite
/// for it is still open, so nobody sends a second one.
pub async fn transfer_pending(conn: &mut PgConnection, perms: &[Permission], on: NaiveDate) -> Result<Vec<Value>> {
    let Some(first) = perms.first() else {
        return Ok(Vec::new());
    };
    let aom = majority(&mut *conn, first.profile.household_id).await?;
    let mut out = Vec::new();
    for p in perms
        .iter()
        .filter(|p| p.role == "guardian" && p.profile.user_id.is_none() && at_majority(&p.profile, aom, on))
    {
        let invited: bool = sqlx::query_scalar(
            "select exists (select 1 from auth_challenge where purpose = 'invite' and used_at is null \
             and expires_at > now() and data->>'claim_profile_id' = $1)",
        )
        .bind(p.profile.id.to_string())
        .fetch_one(&mut *conn)
        .await?;
        out.push(json!({"profile": p.profile.slug, "invited": invited}));
    }
    Ok(out)
}

/// Who had access to the profiles the caller owns until she took them over at majority, and has none
/// now: one entry per person, from the claim's `grant_change` audit rows (never a capped page of them).
pub async fn former_access(conn: &mut PgConnection, perms: &[Permission], user_id: Uuid) -> Result<Vec<Value>> {
    let owned: Vec<Uuid> = perms
        .iter()
        .filter(|p| p.role == "owner" && p.profile.user_id == Some(user_id))
        .map(|p| p.profile.id)
        .collect();
    if owned.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "select slug, who, args_summary, ts from ( \
           select distinct on (e.profile_id, e.resource_ids[1]) p.slug, e.resource_ids[1] as who, \
             e.args_summary, e.ts, e.id \
           from audit_event e join profile p on p.id = e.profile_id \
           where e.profile_id = any($1) and e.action = 'grant_change' and e.args_summary like 'majority;%' \
           and e.resource_ids[1] is not null \
           and not exists (select 1 from profile_grant g where g.profile_id = e.profile_id \
             and g.user_id::text = e.resource_ids[1]) \
           order by e.profile_id, e.resource_ids[1], e.id desc \
         ) latest order by id desc",
    )
    .bind(&owned)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(slug, user, args, ts)| {
            let field = |k: &str| {
                args.split(';')
                    .find_map(|part| part.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
                    .unwrap_or("")
                    .to_string()
            };
            let categories: Vec<String> = field("categories")
                .split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect();
            let expires = field("expires");
            json!({
                "profile": slug,
                "user_id": user,
                "role": field("role"),
                "categories": categories,
                "expires_at": if expires.is_empty() { Value::Null } else { json!(expires) },
                "ended_at": ownhealth_core::pyjson::pydantic_datetime_utc(&ts),
            })
        })
        .collect())
}

/// The person takes over their own profile: it is linked to their login and they become owner.
///
/// A child past the age of majority on `on` takes it over whole: the guardians' grants end, each
/// written to the audit log as a `grant_change` of hers so she sees who had access and can give it
/// back, and both sensitive genome switches start off until she turns them on. Otherwise (a minor,
/// an adult relative) guardian and caregiver grants stay until the new owner changes them.
pub async fn claim_profile(
    conn: &mut PgConnection,
    profile_id: Uuid,
    user_id: Uuid,
    granted_by: Option<Uuid>,
    on: NaiveDate,
) -> AuthResult<()> {
    let n = sqlx::query("update profile set user_id = $1 where id = $2 and user_id is null")
        .bind(user_id)
        .bind(profile_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(AuthError::refused(
            "already_claimed",
            "That profile already belongs to a login.",
        ));
    }
    set_grant(conn, user_id, profile_id, "owner", None, granted_by, None).await?;
    let Some(profile) = get_profile(&mut *conn, profile_id).await? else {
        return Ok(());
    };
    if !at_majority(&profile, majority(&mut *conn, profile.household_id).await?, on) {
        return Ok(());
    }
    // A grant that had already expired gave no access: it goes without a word, and only live ones are
    // listed as hers to give back, each with the end date it had.
    type Ended = (Uuid, Vec<String>, Option<chrono::DateTime<chrono::Utc>>, bool);
    let ended: Vec<Ended> = sqlx::query_as(
        "delete from profile_grant where profile_id = $1 and role = 'guardian' and user_id <> $2 \
         returning user_id, categories, expires_at, (expires_at is null or expires_at > now())",
    )
    .bind(profile_id)
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;
    set_genome_switches(&mut *conn, profile_id, Some(false), Some(false)).await?;
    // her results feed no relative's hints until she shares them herself (migration 1111)
    sqlx::query("delete from family_share where source_profile_id = $1")
        .bind(profile_id)
        .execute(&mut *conn)
        .await?;
    let event = |action: &str, args: String| AuditEvent {
        actor_id: Some(user_id),
        tool: Some("claim_profile".into()),
        profile_id: Some(profile_id),
        args_summary: Some(args),
        ..AuditEvent::new("user", "ui", action)
    };
    write_audit(
        &mut *conn,
        &event("consent_change", "ui=False;agents=False;majority".into()),
    )
    .await?;
    for (guardian, mut categories, expires_at, live) in ended {
        if !live {
            continue;
        }
        categories.sort();
        let mut args = format!("majority;role=guardian;categories={}", categories.join(","));
        if let Some(at) = expires_at {
            args.push_str(&format!(
                ";expires={}",
                ownhealth_core::pyjson::pydantic_datetime_utc(&at)
            ));
        }
        write_audit(
            &mut *conn,
            &AuditEvent {
                resource_type: Some("user".into()),
                resource_ids: Some(vec![guardian.to_string()]),
                ..event("grant_change", args)
            },
        )
        .await?;
    }
    Ok(())
}

/// The household's founder: the login whose own profile is the household's `self`. Only they change
/// household settings.
pub async fn is_household_owner(conn: &mut PgConnection, household_id: Uuid, user_id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from profile where household_id = $1 and user_id = $2 and relationship = 'self')",
    )
    .bind(household_id)
    .bind(user_id)
    .fetch_one(conn)
    .await?)
}

/// The ages a household may set as its age of majority.
pub const MAJORITY_RANGE: std::ops::RangeInclusive<i32> = 14..=21;

/// Store the household's age of majority.
pub async fn set_majority(conn: &mut PgConnection, household_id: Uuid, years: i32) -> Result<()> {
    sqlx::query(
        "insert into household_setting (household_id, key, value) values ($1, 'age_of_majority', $2) \
         on conflict (household_id, key) do update set value = excluded.value",
    )
    .bind(household_id)
    .bind(json!(years))
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct RelativeIn<'a> {
    pub household_id: Uuid,
    pub created_by: Uuid,
    pub display_name: &'a str,
    pub role: &'a str,
    pub relationship: Option<&'a str>,
    pub birth_date: Option<NaiveDate>,
    pub sex_at_birth: Option<&'a str>,
    pub country: Option<&'a str>,
    pub slug: Option<&'a str>,
}

/// A profile without a login (a child, a parent), managed by its creator as guardian.
///
/// Only guardian: a caregiver cannot manage, so a profile with only a caregiver grant would have
/// nobody to grant access, delete it or let its creator close their account.
pub async fn add_relative(conn: &mut PgConnection, r: &RelativeIn<'_>) -> AuthResult<(Uuid, String)> {
    if r.role != "guardian" {
        return Err(AuthError::refused(
            "bad_input",
            "The creator of a relative's profile is its guardian.",
        ));
    }
    let display_name = truncate_chars(r.display_name.trim(), 80);
    if display_name.is_empty() {
        return Err(AuthError::refused("bad_input", "A name is needed."));
    }
    let wanted = r.slug.filter(|s| !s.is_empty()).unwrap_or(display_name);
    let s = unique_slug(&mut *conn, r.household_id, wanted).await?;
    let pid = create_profile(
        &mut *conn,
        &ProfileIn {
            household_id: r.household_id,
            slug: s.clone(),
            display_name: display_name.into(),
            relationship: r.relationship.map(str::to_string),
            birth_date: r.birth_date,
            sex_at_birth: r.sex_at_birth.map(str::to_string),
            country: r.country.map(str::to_string),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    set_grant(conn, r.created_by, pid, r.role, None, Some(r.created_by), None).await?;
    Ok((pid, s))
}

// ------------------------------------------------------------------- deletion ----

/// Every column that names a blob by its sha256, with the condition that picks the rows profile `$1`
/// owns. A blob is collected from those rows and deleted only when no row of any of these columns
/// still names it. `tests/blob_columns_mirror.rs` fails when the schema has a blob column missing here
/// and in [`HOUSEHOLD_BLOB_COLUMNS`].
pub const BLOB_COLUMNS: &[(&str, &[&str], &str)] = &[
    ("document", &["blob_sha256"], "profile_id = $1"),
    (
        "document_page",
        &["words_blob", "image_blob"],
        "document_id in (select id from document where profile_id = $1)",
    ),
    (
        "genome_dataset",
        &[
            "facts_blob",
            "vcf_blob",
            "tbi_blob",
            "coverage_blob",
            "gvcf_blob",
            "gvcf_tbi_blob",
            "sites_blob",
            "sites_tbi_blob",
        ],
        "profile_id = $1",
    ),
    (
        "ocr_run",
        &["words_blob"],
        "document_id in (select id from document where profile_id = $1)",
    ),
    // The raw bytes an inbox handed us: a push or export of the profile's own source (Health Auto
    // Export, Gadgetbridge), and what a household inbox (a shared mailbox, Paperless, the scanner
    // folder) filed to the profile: an item the inbox routed to it (a mail with nothing to import keeps
    // its .eml and no document), the item of one of its documents, the whole .eml of a mail released to
    // it, a structured file of its household's inboxes whose document the importer made (only for an item
    // that names no profile: rows older than migration 1249; an item that names someone else is theirs),
    // and a file in the profile's folder of a scanner folder.
    (
        "source_item",
        &["blob_sha256"],
        "profile_id = $1 \
         or source_id in (select id from source where profile_id = $1) \
         or document_id in (select id from document where profile_id = $1) \
         or quarantine_id in (select id from quarantine_item where released_document_ids \
            && array(select id from document where profile_id = $1)) \
         or (document_id is null and profile_id is null \
            and blob_sha256 in (select blob_sha256 from document where profile_id = $1) \
            and source_id in (select s.id from source s join profile p on p.household_id = s.household_id \
            where p.id = $1 and s.profile_id is null)) \
         or (source_id in (select s.id from source s join profile p on p.household_id = s.household_id \
            where p.id = $1 and s.kind = 'folder') \
            and starts_with(external_id, (select slug from profile where id = $1) || '/'))",
    ),
    // mail held on a profile's own mailbox, and mail a person released into the profile (its documents,
    // or its item when nothing in it became a document)
    (
        "quarantine_item",
        &["raw_blob"],
        "source_id in (select id from source where profile_id = $1) \
         or released_document_ids && array(select id from document where profile_id = $1) \
         or id in (select quarantine_id from source_item where profile_id = $1 and quarantine_id is not null)",
    ),
];

/// The condition of `table` in [`BLOB_COLUMNS`].
fn owned(table: &str) -> &'static str {
    BLOB_COLUMNS
        .iter()
        .find(|(t, _, _)| *t == table)
        .map_or("false", |(_, _, owned)| owned)
}

/// The job that retries removing the files a profile delete could not remove at once.
pub const UNLINK_JOB: &str = "blobs.unlink";
/// Blob columns that never belong to one profile, so a profile delete leaves them alone. None yet.
pub const HOUSEHOLD_BLOB_COLUMNS: &[(&str, &str)] = &[];
const KEEP: [&str; 3] = ["profile", "profile_grant", "audit_event"];

/// Every table with a profile_id column (other lanes add their own), minus the ones we keep.
async fn profile_tables(conn: &mut PgConnection) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "select c.table_name::text from information_schema.columns c join information_schema.tables t \
         on t.table_schema = c.table_schema and t.table_name = c.table_name \
         where c.table_schema = current_schema() and c.column_name = 'profile_id' \
         and t.table_type = 'BASE TABLE' order by c.table_name",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().filter(|t| !KEEP.contains(&t.as_str())).collect())
}

async fn profile_blobs(conn: &mut PgConnection, profile_id: Uuid) -> Result<BTreeSet<Vec<u8>>> {
    let mut shas = BTreeSet::new();
    for (table, cols, owned) in BLOB_COLUMNS {
        let sql = format!("select unnest(array[{}]) from {table} where {owned}", cols.join(", "));
        let rows: Vec<Option<Vec<u8>>> = sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?;
        shas.extend(rows.into_iter().flatten());
    }
    Ok(shas)
}

async fn still_referenced(conn: &mut PgConnection, sha: &[u8]) -> Result<bool> {
    for (table, cols, _) in BLOB_COLUMNS {
        let cond: Vec<String> = cols.iter().map(|c| format!("{c} = $1")).collect();
        let sql = format!("select 1 from {table} where {} limit 1", cond.join(" or "));
        let hit: Option<i32> = sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(sha)
            .fetch_optional(&mut *conn)
            .await?;
        if hit.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What `delete_profile` removed: rows per table (only tables with rows) and the blob hashes nobody
/// references any more, which the caller deletes after the transaction commits, with the profile's
/// folders of the folder inboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub household_id: Uuid,
    pub rows: Vec<(String, u64)>,
    pub blobs: Vec<String>,
    /// The queued retry of `blobs`, for the caller to trim after its own unlinks.
    pub unlink_job: Option<i64>,
    /// The household's folder inboxes, read before the delete: their folders may hold the profile's files.
    pub folders: Vec<FolderInbox>,
}

/// A folder inbox's settings, and whether it was the deleted profile's own (its root files were the
/// profile's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInbox {
    pub config: Value,
    pub own: bool,
}

impl Deleted {
    pub fn rows_json(&self) -> Value {
        Value::Object(self.rows.iter().map(|(t, n)| (t.clone(), json!(n))).collect())
    }

    pub fn total_rows(&self) -> u64 {
        self.rows.iter().map(|(_, n)| n).sum()
    }
}

/// Hard delete every row of the profile (security-privacy.md 12).
///
/// The audit log keeps its events (ids only, no payloads). Tokens drop the profile; a token left with
/// no profile is deleted.
pub async fn delete_profile(conn: &mut PgConnection, profile_id: Uuid) -> AuthResult<Deleted> {
    let candidates = profile_blobs(&mut *conn, profile_id).await?;
    // The credentials the profile's inboxes keep (a mailbox password, a Nightscout token) and the PDF
    // passwords of the sender rules that file mail to it, as removing the inbox does: the source and
    // sender rows go with the profile, and `secret` would keep the sealed values with nothing naming them.
    let secrets: Vec<Uuid> = sqlx::query_scalar(
        "select secret_id from source where profile_id = $1 and secret_id is not null union \
         select pr.secret_id from pdf_password_rule pr join sender_rule r on r.id = pr.sender_rule_id \
         where r.profile_id = $1 or r.source_id in (select id from source where profile_id = $1)",
    )
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    // A household inbox stays, and so do its items, but the bytes it filed to the profile are the
    // profile's. A scanner folder item names the person's short name and file name, and the file itself
    // goes, so the item goes. A mailbox or Paperless item keeps only its message or document id, so the
    // next poll does not import it again into another profile. This runs before the documents and the
    // released mail go, since the conditions read them. The held mail is picked first: a message released
    // with nothing to import is found through its item, which the update below detaches.
    let mail: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select id from quarantine_item where {}",
        owned("quarantine_item")
    )))
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    let items = owned("source_item");
    sqlx::query(AssertSqlSafe(format!(
        "delete from source_item where source_id in (select id from source where kind = 'folder') and ({items})"
    )))
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    // The item forgets the profile too, or the delete of every profile_id row below would take it.
    sqlx::query(AssertSqlSafe(format!(
        "update source_item set blob_sha256 = null, profile_id = null \
         where (blob_sha256 is not null or profile_id is not null) and ({items})"
    )))
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    // Mail held on the profile's own mailbox (`quarantine_item.source_id` is set null when the source
    // goes, which would keep the message and its raw blob), and mail released into the profile.
    let held = sqlx::query("delete from quarantine_item where id = any($1)")
        .bind(&mail)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    // A Paperless tag rule that files to the profile goes; the household inbox keeps its other rules. The
    // settings route takes any id form `Uuid::parse_str` reads (hyphens or not, braces, a URN), so the
    // value is compared without those; a value that is no id at all stays and never stops the delete.
    sqlx::query(
        "update source set config = jsonb_set(config, '{profile_by_tag}', coalesce((select jsonb_object_agg(k, v) \
         from jsonb_each(config->'profile_by_tag') e(k, v) \
         where regexp_replace(lower(v #>> '{}'), '^urn:uuid:|[{}-]', '', 'g') is distinct from $1), '{}'::jsonb)) \
         where jsonb_typeof(config->'profile_by_tag') = 'object' \
         and exists (select 1 from jsonb_each(config->'profile_by_tag') e(k, v) \
         where regexp_replace(lower(v #>> '{}'), '^urn:uuid:|[{}-]', '', 'g') = $1)",
    )
    .bind(profile_id.simple().to_string())
    .execute(&mut *conn)
    .await?;
    let household_id: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(profile_id)
        .fetch_one(&mut *conn)
        .await?;
    // The folder inboxes whose folders hold the profile's files: the caller removes them after the commit.
    let folders: Vec<(Value, bool)> = sqlx::query_as(
        "select s.config, coalesce(s.profile_id = $1, false) from source s join profile p \
         on p.household_id = s.household_id where p.id = $1 and s.kind = 'folder' order by s.created_at",
    )
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    // candidate_row -> observation -> src -> candidate_row is a cycle: cut it first.
    sqlx::query(
        "update candidate_row set observation_id = null where observation_id in \
         (select id from observation where profile_id = $1) or extraction_run_id in \
         (select r.id from extraction_run r join document d on d.id = r.document_id where d.profile_id = $1)",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let mut todo = profile_tables(&mut *conn).await?;
    let mut counts: Vec<(String, u64)> = vec![("quarantine_item".into(), held)];
    while !todo.is_empty() {
        let mut failed = Vec::new();
        for table in &todo {
            let mut sp = (*conn).begin().await?;
            let res = sqlx::query(AssertSqlSafe(format!("delete from \"{table}\" where profile_id = $1")))
                .bind(profile_id)
                .execute(&mut *sp)
                .await;
            match res {
                Ok(r) => {
                    sp.commit().await?;
                    counts.push((table.clone(), r.rows_affected()));
                }
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => {
                    // a foreign key from a table not deleted yet
                    sp.rollback().await?;
                    failed.push(table.clone());
                }
                Err(e) => return Err(e.into()),
            }
        }
        if failed.len() == todo.len() {
            let list: Vec<String> = failed.iter().map(|t| format!("'{t}'")).collect();
            return Err(AuthError::refused(
                "delete_blocked",
                format!("Rows in [{}] are still referenced.", list.join(", ")),
            ));
        }
        todo = failed;
    }
    sqlx::query(
        "update api_token set profile_ids = array_remove(profile_ids, $1), \
         default_profile = case when default_profile = $1 then (array_remove(profile_ids, $1))[1] \
         else default_profile end where $1 = any(profile_ids) or default_profile = $1",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let tokens = sqlx::query("delete from api_token where cardinality(profile_ids) = 0")
        .execute(&mut *conn)
        .await?
        .rows_affected();
    counts.push(("api_token".into(), tokens));
    // OAuth consents drop the profile the same way, so a refresh never reissues it; a consent left with
    // no profile is revoked with whatever it issued.
    sqlx::query(
        "update oauth_consent set profile_ids = array_remove(profile_ids, $1), \
         default_profile = case when default_profile = $1 then (array_remove(profile_ids, $1))[1] \
         else default_profile end where $1 = any(profile_ids) or default_profile = $1",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    let emptied: Vec<Uuid> = sqlx::query_scalar(
        "update oauth_consent set revoked_at = coalesce(revoked_at, now()) \
         where cardinality(profile_ids) = 0 returning id",
    )
    .fetch_all(&mut *conn)
    .await?;
    if !emptied.is_empty() {
        sqlx::query("update api_token set revoked_at = now() where consent_id = any($1) and revoked_at is null")
            .bind(&emptied)
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "update oauth_refresh_token set revoked_at = now() where consent_id = any($1) and revoked_at is null",
        )
        .bind(&emptied)
        .execute(&mut *conn)
        .await?;
    }
    let sealed = sqlx::query("delete from secret where id = any($1)")
        .bind(&secrets)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    counts.push(("secret".into(), sealed));
    sqlx::query("delete from profile where id = $1")
        .bind(profile_id)
        .execute(&mut *conn)
        .await?;
    let mut orphans = Vec::new();
    for sha in candidates {
        if !still_referenced(&mut *conn, &sha).await? {
            orphans.push(hex::encode(sha));
        }
    }
    orphans.sort();
    // The retry of a file the caller cannot remove after the commit (a busy or read-only disk), queued
    // here so it commits with the rows: a crash between the commit and the unlinks leaves the job.
    // The caller trims it to the files that stuck, or drops it when none did.
    let unlink_job = if orphans.is_empty() {
        None
    } else {
        enqueue(
            &mut *conn,
            UNLINK_JOB,
            // the household, so a retry that runs out of attempts shows in its review queue
            &json!({"household_id": household_id, "blobs": orphans}),
            EnqueueOptions {
                run_after: Some(chrono::Utc::now() + chrono::Duration::seconds(60)),
                ..EnqueueOptions::default()
            },
        )
        .await?
    };
    Ok(Deleted {
        household_id,
        unlink_job,
        rows: counts.into_iter().filter(|(_, n)| *n > 0).collect(),
        blobs: orphans,
        folders: folders
            .into_iter()
            .map(|(config, own)| FolderInbox { config, own })
            .collect(),
    })
}

async fn unlink_job(ctx: JobContext, job: Job) -> HandlerResult {
    let shas: Vec<String> = job
        .payload
        .get("blobs")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let mut conn = ctx.pool.acquire().await?;
    let (mut removed, mut failed) = (0i32, 0usize);
    for sha in &shas {
        let Ok(bytes) = sha256_hex_to_bytes(sha) else {
            continue;
        };
        if still_referenced(&mut conn, &bytes).await? {
            continue;
        }
        match ctx.blobs.delete(sha) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, "blob delete failed again");
                failed += 1;
            }
        }
    }
    if removed > 0 {
        write_audit(
            &mut conn,
            &AuditEvent {
                resource_type: Some("blob".into()),
                rows_out: Some(removed),
                args_summary: Some(format!("blobs={removed}")),
                ..AuditEvent::new("system", "job", "purge")
            },
        )
        .await?;
    }
    if failed > 0 {
        return Err(JobError::retry(format!("{failed} files could not be removed yet")));
    }
    Ok(Some(json!({"removed": removed})))
}

pub fn register() {
    register_handler(UNLINK_JOB, |ctx, job| Box::pin(unlink_job(ctx, job)));
}

/// Slugs of profiles nobody else could manage once this login closes (no other owner or guardian;
/// the user's own profile becomes unclaimed, so a guardian could take over).
pub async fn orphaned_profiles(
    conn: &mut PgConnection,
    user_id: Uuid,
    except_profile: Option<Uuid>,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select p.slug from profile_grant g join profile p on p.id = g.profile_id where g.user_id = $1 \
         and p.id is distinct from $2 and not exists \
         (select 1 from profile_grant o where o.profile_id = g.profile_id and o.user_id <> $1 \
         and o.role in ('owner', 'guardian') and (o.expires_at is null or o.expires_at > now())) \
         order by p.slug",
    )
    .bind(user_id)
    .bind(except_profile)
    .fetch_all(conn)
    .await?)
}

/// Close a login. Reviewed facts keep pointing at the user id (clinical rows are immutable), so the
/// user row stays, disabled and renamed; every way in (passkeys, sessions, tokens, codes, OIDC link,
/// password, grants) is removed and their own profile becomes unclaimed.
///
/// Refused while a profile would be left with nobody to manage it: delete it or grant someone first.
pub async fn delete_account(conn: &mut PgConnection, user_id: Uuid) -> AuthResult<Value> {
    let orphaned = orphaned_profiles(&mut *conn, user_id, None).await?;
    if !orphaned.is_empty() {
        return Err(last_manager(orphaned));
    }
    for sql in [
        "delete from passkey where user_id = $1",
        "delete from recovery_code where user_id = $1",
        "delete from api_token where user_id = $1",
        "delete from profile_grant where user_id = $1",
        "delete from auth_challenge where user_id = $1",
        "update user_session set revoked_at = now() where user_id = $1 and revoked_at is null",
        "update profile set user_id = null where user_id = $1",
        "update app_user set disabled_at = now(), display_name = 'Deleted user', oidc_subject = null, \
         password_hash = null where id = $1",
    ] {
        sqlx::query(sql).bind(user_id).execute(&mut *conn).await?;
    }
    Ok(json!({"user_id": user_id, "disabled": true}))
}

/// 409 `last_manager` with the stranded profiles.
pub fn last_manager(profiles: Vec<String>) -> AuthError {
    AuthError::refused(
        "last_manager",
        "Nobody else can reach these profiles. Delete them or grant someone access first.",
    )
    .with("profiles", json!(profiles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Lucía Rivera"), "lucia");
        assert_eq!(slugify("Mateo Rivera"), "mateo");
        assert_eq!(slugify("  "), "person");
        assert_eq!(slugify("Me"), "person");
        assert_eq!(slugify("José-María Ortega"), "jose-maria");
        assert_eq!(slugify("O'Brien"), "o-brien");
        assert_eq!(slugify("_x_"), "x");
        assert_eq!(slugify("Ñandú"), "nandu");
        assert!(is_slug("carmen") && !is_slug("Carmen") && !is_slug("-x") && !is_slug(""));
    }
}
