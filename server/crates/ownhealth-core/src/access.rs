//! Households, users, profiles, grants and the one permission function (`core/access.py`).
//!
//! ```text
//! effective = token.scopes ∩ grant(user, profile).categories − profile.locks
//! ```
//!
//! The browser session is a token with every scope. A user with no grant on a profile cannot reach
//! it, whatever a token says. The owner's own grant ignores locks; locks restrict everyone else.
//! Every surface (UI, REST, MCP, export) calls `effective_permission` or `require`.

use sqlx::AssertSqlSafe;
use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};

pub const SCOPES: [&str; 10] = [
    "read:summary",
    "read:labs",
    "read:vitals",
    "read:wearables",
    "read:records",
    "read:genome",
    "read:genome_raw",
    "read:sensitive",
    "read:documents_text",
    "write:propose",
];
pub const PAT_DEFAULT_SCOPES: [&str; 3] = ["read:summary", "read:labs", "read:vitals"];
pub const ROLES: [&str; 4] = ["owner", "guardian", "caregiver", "viewer"];
pub const ACTIONS: [&str; 8] = [
    "import",
    "review",
    "write",
    "manage_grants",
    "export",
    "delete",
    "purge",
    "manage_tokens",
];

pub type Scopes = BTreeSet<String>;

pub fn scope_set<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> Scopes {
    items.into_iter().map(|s| s.as_ref().to_string()).collect()
}

pub fn all_scopes() -> Scopes {
    scope_set(SCOPES)
}

pub fn is_scope(s: &str) -> bool {
    SCOPES.contains(&s)
}

/// The categories a new grant of `role` gets.
pub fn default_categories(role: &str) -> Scopes {
    match role {
        "owner" | "guardian" => all_scopes(),
        "caregiver" => scope_set([
            "read:summary",
            "read:labs",
            "read:vitals",
            "read:records",
            "write:propose",
        ]),
        "viewer" => scope_set(["read:summary"]),
        _ => Scopes::new(),
    }
}

/// What each role may do beyond reading. Reading is decided by scopes.
pub fn can(role: &str, action: &str) -> bool {
    let allowed: &[&str] = match role {
        "owner" => &[
            "import",
            "review",
            "write",
            "manage_grants",
            "export",
            "delete",
            "purge",
        ],
        "guardian" | "caregiver" => &["import", "review", "write"],
        _ => &[],
    };
    allowed.contains(&action)
}

/// token scopes ∩ grant categories − profile locks (locks do not bind the owner).
pub fn effective_scopes(token_scopes: &Scopes, categories: &Scopes, locks: &Scopes, role: &str) -> Scopes {
    let both: Scopes = token_scopes.intersection(categories).cloned().collect();
    if role == "owner" {
        both
    } else {
        both.difference(locks).cloned().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    Session,
    Token,
    Dev,
    System,
}

/// Who is calling. A browser session has every scope; a PAT has its own scopes and profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub user_id: Option<Uuid>,
    pub household_id: Option<Uuid>,
    pub scopes: Scopes,
    pub token_id: Option<Uuid>,
    pub token_label: Option<String>,
    /// None: every profile the user has a grant on.
    pub profile_ids: Option<BTreeSet<Uuid>>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub lang: String,
    pub units: String,
    /// The session row behind a browser principal (step-up, logout).
    pub session_id: Option<Uuid>,
}

impl Principal {
    /// A browser session (or dev auth) for one user: every scope, every granted profile.
    pub fn session(user_id: Uuid, household_id: Uuid) -> Principal {
        Principal {
            kind: PrincipalKind::Session,
            user_id: Some(user_id),
            household_id: Some(household_id),
            scopes: all_scopes(),
            token_id: None,
            token_label: None,
            profile_ids: None,
            default_profile: None,
            identity_mode: "full".into(),
            lang: "es".into(),
            units: "si".into(),
            session_id: None,
        }
    }

    /// `user` | `token` | `system`, the `actor_kind` of audit rows and src rows.
    pub fn actor_kind(&self) -> &'static str {
        match self.kind {
            PrincipalKind::Token => "token",
            PrincipalKind::System => "system",
            _ => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Profile {
    pub id: Uuid,
    pub household_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub user_id: Option<Uuid>,
    pub relationship: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub sex_at_birth: Option<String>,
    pub country: Option<String>,
    pub deceased: bool,
    pub unit_pref: String,
    pub genome_sensitive_ui: bool,
    pub genome_sensitive_agents: bool,
    pub locks: Vec<String>,
}

impl Profile {
    pub fn locks(&self) -> Scopes {
        scope_set(&self.locks)
    }

    pub fn age_on(&self, on: NaiveDate) -> Option<i32> {
        let b = self.birth_date?;
        let before_birthday = (on.month(), on.day()) < (b.month(), b.day());
        Some(on.year() - b.year() - i32::from(before_birthday))
    }

    pub fn is_minor(&self, on: NaiveDate, age_of_majority: i32) -> bool {
        self.age_on(on).is_some_and(|a| a < age_of_majority)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Permission {
    pub profile: Profile,
    pub role: String,
    pub scopes: Scopes,
}

impl Permission {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    pub fn can(&self, action: &str) -> bool {
        can(&self.role, action)
    }
}

const PROFILE_COLS: &str = "id, household_id, slug, display_name, user_id, relationship, birth_date, sex_at_birth, \
                            country, deceased, unit_pref, genome_sensitive_ui, genome_sensitive_agents, locks";

pub async fn get_profile(conn: &mut PgConnection, profile_id: Uuid) -> Result<Option<Profile>> {
    let sql = format!("select {PROFILE_COLS} from profile where id = $1");
    Ok(sqlx::query_as::<_, Profile>(AssertSqlSafe(sql.as_str()))
        .bind(profile_id)
        .fetch_optional(conn)
        .await?)
}

pub async fn get_profile_by_slug(conn: &mut PgConnection, household_id: Uuid, slug: &str) -> Result<Option<Profile>> {
    let sql = format!("select {PROFILE_COLS} from profile where household_id = $1 and slug = $2");
    Ok(sqlx::query_as::<_, Profile>(AssertSqlSafe(sql.as_str()))
        .bind(household_id)
        .bind(slug)
        .fetch_optional(conn)
        .await?)
}

fn not_granted() -> Error {
    // One message for an unknown id and an ungranted one: a uuid must not confirm a profile exists
    // nor reveal its slug. (resolve_profile_ref echoes a slug only when the caller sent it.)
    Error::access_denied("profile_not_granted", "Profile not available.")
}

/// The permission `principal` has on one profile. `AccessDenied` when there is none.
pub async fn effective_permission(
    conn: &mut PgConnection,
    principal: &Principal,
    profile_id: Uuid,
) -> Result<Permission> {
    let profile = get_profile(&mut *conn, profile_id).await?;
    let (Some(profile), Some(user_id)) = (profile, principal.user_id) else {
        return Err(not_granted());
    };
    if principal
        .profile_ids
        .as_ref()
        .is_some_and(|ids| !ids.contains(&profile_id))
    {
        return Err(not_granted());
    }
    let grant: Option<(String, Vec<String>)> = sqlx::query_as(
        "select role, categories from profile_grant where user_id = $1 and profile_id = $2 \
         and (expires_at is null or expires_at > now())",
    )
    .bind(user_id)
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((role, categories)) = grant else {
        return Err(not_granted());
    };
    let scopes = effective_scopes(&principal.scopes, &scope_set(&categories), &profile.locks(), &role);
    Ok(Permission { profile, role, scopes })
}

#[derive(sqlx::FromRow)]
struct GrantedProfile {
    #[sqlx(flatten)]
    profile: Profile,
    role: String,
    categories: Vec<String>,
}

/// Every profile the principal may reach, in slug order, with role and effective scopes.
pub async fn accessible_profiles(conn: &mut PgConnection, principal: &Principal) -> Result<Vec<Permission>> {
    let Some(user_id) = principal.user_id else {
        return Ok(Vec::new());
    };
    let cols: Vec<String> = PROFILE_COLS.split(',').map(|c| format!("p.{}", c.trim())).collect();
    let sql = format!(
        "select {}, g.role, g.categories from profile_grant g join profile p on p.id = g.profile_id \
         where g.user_id = $1 and (g.expires_at is null or g.expires_at > now()) order by p.slug",
        cols.join(", ")
    );
    let rows: Vec<GrantedProfile> = sqlx::query_as(AssertSqlSafe(sql.as_str()))
        .bind(user_id)
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            principal
                .profile_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&r.profile.id))
        })
        .map(|r| {
            let scopes = effective_scopes(
                &principal.scopes,
                &scope_set(&r.categories),
                &r.profile.locks(),
                &r.role,
            );
            Permission {
                profile: r.profile,
                role: r.role,
                scopes,
            }
        })
        .collect())
}

/// A profile reference: a slug, `"me"`, a uuid, or none (the token's default profile).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProfileRef<'a> {
    Id(Uuid),
    Name(&'a str),
    Default,
}

impl<'a> From<&'a str> for ProfileRef<'a> {
    fn from(s: &'a str) -> Self {
        ProfileRef::Name(s)
    }
}

impl From<Uuid> for ProfileRef<'_> {
    fn from(id: Uuid) -> Self {
        ProfileRef::Id(id)
    }
}

impl<'a> From<Option<&'a str>> for ProfileRef<'a> {
    fn from(s: Option<&'a str>) -> Self {
        s.map_or(ProfileRef::Default, ProfileRef::Name)
    }
}

/// Turn a slug, `me`, a uuid or none (the token's default profile) into a profile id.
pub async fn resolve_profile_ref(conn: &mut PgConnection, principal: &Principal, r: ProfileRef<'_>) -> Result<Uuid> {
    let name = match r {
        ProfileRef::Id(id) => return Ok(id),
        ProfileRef::Name(n) => Some(n),
        ProfileRef::Default => None,
    };
    let allowed = accessible_profiles(conn, principal).await?;
    let slugs = || allowed.iter().map(|p| p.profile.slug.clone()).collect::<Vec<_>>();
    if matches!(name, None | Some("") | Some("me")) {
        if name.is_none()
            && let Some(d) = principal.default_profile
        {
            return Ok(d);
        }
        if let Some(own) = allowed
            .iter()
            .find(|p| p.profile.user_id.is_some() && p.profile.user_id == principal.user_id)
        {
            return Ok(own.profile.id);
        }
        if let Some(d) = principal.default_profile {
            return Ok(d);
        }
        return Err(Error::access_denied_with(
            "profile_required",
            "Name a profile.",
            slugs(),
        ));
    }
    let name = name.unwrap_or_default();
    if let Ok(id) = Uuid::parse_str(name) {
        return Ok(id);
    }
    if let Some(p) = allowed.iter().find(|p| p.profile.slug == name) {
        return Ok(p.profile.id);
    }
    Err(Error::access_denied_with(
        "profile_not_granted",
        format!("Profile '{name}' is not available."),
        slugs(),
    ))
}

/// Resolve the profile and check one scope and/or one role action. `AccessDenied` otherwise.
pub async fn require(
    conn: &mut PgConnection,
    principal: &Principal,
    profile: ProfileRef<'_>,
    scope: Option<&str>,
    action: Option<&str>,
) -> Result<Permission> {
    let profile_id = resolve_profile_ref(&mut *conn, principal, profile).await?;
    let perm = effective_permission(conn, principal, profile_id).await?;
    if let Some(scope) = scope
        && !perm.has(scope)
    {
        return Err(Error::access_denied(
            "insufficient_scope",
            format!("This needs {scope}."),
        ));
    }
    if let Some(action) = action
        && !perm.can(action)
    {
        return Err(Error::access_denied(
            "forbidden",
            format!("The role {} cannot {action} on this profile.", perm.role),
        ));
    }
    Ok(perm)
}

// ------------------------------------------------------------------- writes ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileIn {
    pub household_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub relationship: Option<String>,
    pub birth_date: Option<NaiveDate>,
    /// female | male | unknown
    pub sex_at_birth: Option<String>,
    pub country: Option<String>,
    pub user_id: Option<Uuid>,
    /// si | conventional | as_reported (default si)
    pub unit_pref: Option<String>,
}

pub async fn create_household(conn: &mut PgConnection, name: &str) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("insert into household (name) values ($1) returning id")
            .bind(name)
            .fetch_one(conn)
            .await?,
    )
}

pub async fn create_user(
    conn: &mut PgConnection,
    household_id: Uuid,
    display_name: &str,
    locale: &str,
    oidc_subject: Option<&str>,
) -> Result<Uuid> {
    // clock_timestamp, not now(): users made in one transaction keep the order they were made in
    Ok(sqlx::query_scalar(
        "insert into app_user (household_id, display_name, locale, oidc_subject, created_at) \
         values ($1, $2, $3, $4, clock_timestamp()) returning id",
    )
    .bind(household_id)
    .bind(display_name)
    .bind(locale)
    .bind(oidc_subject)
    .fetch_one(conn)
    .await?)
}

pub async fn create_profile(conn: &mut PgConnection, data: &ProfileIn) -> Result<Uuid> {
    if let Some(sex) = &data.sex_at_birth
        && !["female", "male", "unknown"].contains(&sex.as_str())
    {
        return Err(Error::Invalid(format!("sex_at_birth: unknown value {sex}")));
    }
    let unit_pref = data.unit_pref.as_deref().unwrap_or("si");
    if !["si", "conventional", "as_reported"].contains(&unit_pref) {
        return Err(Error::Invalid(format!("unit_pref: unknown value {unit_pref}")));
    }
    Ok(sqlx::query_scalar(
        "insert into profile (household_id, slug, display_name, relationship, birth_date, sex_at_birth, country, \
         user_id, unit_pref) values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning id",
    )
    .bind(data.household_id)
    .bind(&data.slug)
    .bind(&data.display_name)
    .bind(&data.relationship)
    .bind(data.birth_date)
    .bind(&data.sex_at_birth)
    .bind(&data.country)
    .bind(data.user_id)
    .bind(unit_pref)
    .fetch_one(conn)
    .await?)
}

/// Create or replace the one grant row for (user, profile). Categories default by role.
pub async fn set_grant(
    conn: &mut PgConnection,
    user_id: Uuid,
    profile_id: Uuid,
    role: &str,
    categories: Option<&Scopes>,
    granted_by: Option<Uuid>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<()> {
    if !ROLES.contains(&role) {
        return Err(Error::Invalid(format!("unknown role {role}")));
    }
    let cats: Vec<String> = categories
        .cloned()
        .unwrap_or_else(|| default_categories(role))
        .into_iter()
        .collect();
    let unknown: Vec<&String> = cats.iter().filter(|c| !is_scope(c)).collect();
    if !unknown.is_empty() {
        return Err(Error::Invalid(format!("unknown categories {unknown:?}")));
    }
    sqlx::query(
        "insert into profile_grant (user_id, profile_id, role, categories, granted_by, expires_at) \
         values ($1, $2, $3, $4, $5, $6) on conflict (user_id, profile_id) do update set \
         role = excluded.role, categories = excluded.categories, granted_by = excluded.granted_by, \
         expires_at = excluded.expires_at, granted_at = now()",
    )
    .bind(user_id)
    .bind(profile_id)
    .bind(role)
    .bind(&cats)
    .bind(granted_by)
    .bind(expires_at)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn revoke_grant(conn: &mut PgConnection, user_id: Uuid, profile_id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from profile_grant where user_id = $1 and profile_id = $2")
        .bind(user_id)
        .bind(profile_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Every grant in a household, by user name then slug (`list_grants`).
pub async fn list_grants(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let rows = sqlx::query(
        "select g.user_id, u.display_name as user_name, g.profile_id, p.slug, g.role, g.categories, g.expires_at, \
         g.granted_at from profile_grant g join profile p on p.id = g.profile_id join app_user u on u.id = g.user_id \
         where p.household_id = $1 order by u.display_name, p.slug",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;
    rows.iter().map(crate::pyjson::row_to_json).collect()
}

pub async fn set_locks(conn: &mut PgConnection, profile_id: Uuid, locks: &Scopes) -> Result<()> {
    let unknown: Vec<&String> = locks.iter().filter(|c| !is_scope(c)).collect();
    if !unknown.is_empty() {
        return Err(Error::Invalid(format!("unknown scopes {unknown:?}")));
    }
    let sorted: Vec<&String> = locks.iter().collect();
    sqlx::query("update profile set locks = $1 where id = $2")
        .bind(sorted)
        .bind(profile_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn set_genome_switches(
    conn: &mut PgConnection,
    profile_id: Uuid,
    ui: Option<bool>,
    agents: Option<bool>,
) -> Result<()> {
    sqlx::query(
        "update profile set genome_sensitive_ui = coalesce($1, genome_sensitive_ui), \
         genome_sensitive_agents = coalesce($2, genome_sensitive_agents) where id = $3",
    )
    .bind(ui)
    .bind(agents)
    .bind(profile_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// The household's `age_of_majority` setting, or `default`.
pub async fn age_of_majority(conn: &mut PgConnection, household_id: Uuid, default: i32) -> Result<i32> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("select value from household_setting where household_id = $1 and key = 'age_of_majority'")
            .bind(household_id)
            .fetch_optional(conn)
            .await?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()).unwrap_or(default),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    })
}

pub fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_scopes_formula() {
        let token = scope_set(["read:summary", "read:labs", "read:sensitive"]);
        let cats = scope_set(["read:summary", "read:labs", "read:sensitive", "read:genome"]);
        let locks = scope_set(["read:sensitive"]);
        assert_eq!(
            effective_scopes(&token, &cats, &locks, "caregiver"),
            scope_set(["read:summary", "read:labs"])
        );
        // the owner's own grant ignores locks
        assert_eq!(
            effective_scopes(&token, &cats, &locks, "owner"),
            scope_set(["read:summary", "read:labs", "read:sensitive"])
        );
        // token scopes cap everything
        assert_eq!(
            effective_scopes(&scope_set(["read:summary"]), &all_scopes(), &Scopes::new(), "owner"),
            scope_set(["read:summary"])
        );
    }

    #[test]
    fn role_actions() {
        assert!(can("owner", "manage_grants") && can("owner", "export"));
        assert!(can("guardian", "import") && !can("guardian", "export"));
        assert!(can("caregiver", "review") && !can("caregiver", "manage_grants"));
        assert!(!can("viewer", "import") && !can("viewer", "write"));
        assert!(!can("owner", "manage_tokens"));
    }
}
