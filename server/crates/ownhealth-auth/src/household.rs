//! People and logins (`auth/household.py`): first-run owner, invites, relatives without a login,
//! claiming, deletion.
//!
//! Who manages a profile (grants, locks, sensitive switches, export, deletion): its `owner`; while
//! nobody has claimed the profile (`profile.user_id is null`: a child, an elderly parent), a
//! `guardian`. A `caregiver` reads and imports but never manages; it may only invite the person to
//! claim their own profile.

use std::collections::BTreeSet;

use chrono::NaiveDate;
use ownhealth_core::access::{Permission, ProfileIn, create_household, create_profile, set_grant};
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
pub async fn setup_owner(conn: &mut PgConnection, o: &OwnerIn<'_>) -> AuthResult<Owner> {
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
        claim_profile(conn, claim, user_id, Some(inviter)).await?;
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

/// The person takes over their own profile: it is linked to their login and they become owner.
/// Existing guardian and caregiver grants stay until the new owner changes them.
pub async fn claim_profile(
    conn: &mut PgConnection,
    profile_id: Uuid,
    user_id: Uuid,
    granted_by: Option<Uuid>,
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

/// Tables that hold blob hashes, and their columns: a blob is deleted only when no row still names it.
pub const BLOB_COLUMNS: [(&str, &[&str]); 3] = [
    ("document", &["blob_sha256"]),
    ("document_page", &["words_blob", "image_blob"]),
    (
        "genome_dataset",
        &["facts_blob", "vcf_blob", "tbi_blob", "coverage_blob"],
    ),
];
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
    for sql in [
        "select blob_sha256, null::bytea, null::bytea, null::bytea from document where profile_id = $1",
        "select words_blob, image_blob, null::bytea, null::bytea from document_page dp \
         join document d on d.id = dp.document_id where d.profile_id = $1",
        "select facts_blob, vcf_blob, tbi_blob, coverage_blob from genome_dataset where profile_id = $1",
    ] {
        type Four = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);
        let rows: Vec<Four> = sqlx::query_as(sql).bind(profile_id).fetch_all(&mut *conn).await?;
        for (a, b, c, d) in rows {
            shas.extend([a, b, c, d].into_iter().flatten());
        }
    }
    Ok(shas)
}

async fn still_referenced(conn: &mut PgConnection, sha: &[u8]) -> Result<bool> {
    for (table, cols) in BLOB_COLUMNS {
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
/// references any more, which the caller deletes after the transaction commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub rows: Vec<(String, u64)>,
    pub blobs: Vec<String>,
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
    let mut counts: Vec<(String, u64)> = Vec::new();
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
    Ok(Deleted {
        rows: counts.into_iter().filter(|(_, n)| *n > 0).collect(),
        blobs: orphans,
    })
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
         and o.role in ('owner', 'guardian')) order by p.slug",
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
