//! Short public ids. `src_` ids are the citation ids shown over MCP (`^src_[a-z0-9_]+$`).

use rand::RngExt;

/// No l, o, 0, 1.
pub const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

pub fn short(n: usize) -> String {
    let mut rng = rand::rng();
    (0..n)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

pub fn new_src_id() -> String {
    format!("src_{}", short(8))
}

/// The whole string must match `^src_[a-z0-9_]+$` (a trailing newline does not slip through).
pub fn is_src_id(value: &str) -> bool {
    value.strip_prefix("src_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// `secrets.token_hex(n)`.
pub fn token_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::fill(&mut b[..]);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_src_id_with_a_trailing_newline_is_refused() {
        assert!(is_src_id("src_abc") && !is_src_id("src_abc\n") && !is_src_id("src_") && !is_src_id("src_A"));
        let id = new_src_id();
        assert!(is_src_id(&id) && id.len() == 12);
        assert_eq!(token_hex(4).len(), 8);
    }
}
