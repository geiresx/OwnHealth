//! Ten single-use recovery codes, printed once at enrolment (`auth/recovery.py`).
//!
//! A code is 60 random bits as 12 Crockford base32 characters, shown as `XXXX-XXXX-XXXX`. The table
//! stores sha256 of the normalised code: at that entropy a slow hash adds nothing, and the unique
//! index lets the code alone identify its user. A recovery login does not count as step-up.

use ownhealth_core::Result;
use rand::RngExt;
use sqlx::PgConnection;
use uuid::Uuid;

pub const COUNT: usize = 10;
/// Crockford: no I, L, O, U.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Upper case, letters and digits only, O read as 0 and I, L as 1 (Python's `str.upper`/`isalnum`).
pub fn normalise(code: &str) -> String {
    code.chars()
        .flat_map(char::to_uppercase)
        .filter(|c| c.is_alphanumeric())
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

pub fn code_hash(code: &str) -> Vec<u8> {
    crate::sha256(&normalise(code))
}

/// Replace the user's codes with ten new ones. Returns the plaintext, to show once.
pub async fn new_codes(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<String>> {
    sqlx::query("delete from recovery_code where user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let codes: Vec<String> = {
        let mut rng = rand::rng();
        (0..COUNT)
            .map(|_| {
                let raw: String = (0..12)
                    .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
                    .collect();
                format!("{}-{}-{}", &raw[..4], &raw[4..8], &raw[8..])
            })
            .collect()
    };
    for c in &codes {
        sqlx::query("insert into recovery_code (user_id, code_sha256) values ($1, $2)")
            .bind(user_id)
            .bind(code_hash(c))
            .execute(&mut *conn)
            .await?;
    }
    Ok(codes)
}

/// Consume one code. Returns its user, or None when unknown, used, or the user is disabled.
pub async fn use_code(conn: &mut PgConnection, code: &str) -> Result<Option<Uuid>> {
    if normalise(code).chars().count() != 12 {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(
        "update recovery_code r set used_at = now() from app_user u where r.code_sha256 = $1 \
         and r.used_at is null and u.id = r.user_id and u.disabled_at is null returning r.user_id",
    )
    .bind(code_hash(code))
    .fetch_optional(conn)
    .await?)
}

pub async fn remaining(conn: &mut PgConnection, user_id: Uuid) -> Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from recovery_code where user_id = $1 and used_at is null")
            .bind(user_id)
            .fetch_one(conn)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalising_forgives_case_dashes_and_lookalikes() {
        assert_eq!(normalise("ab1o-il2z-9999"), "AB10112Z9999");
        assert_eq!(code_hash("AB10-112Z-9999"), code_hash(" ab1o il2z 9999 "));
    }
}
