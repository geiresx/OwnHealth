//! Small Python semantics the extractor depends on.

/// Python `round(x, n)`: correctly rounded, ties to even on the exact binary value (Rust's
/// formatting rounds the same way).
pub fn py_round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.n$}").parse().unwrap_or(x)
}

pub fn py_round3(x: f64) -> f64 {
    py_round(x, 3)
}

/// Python `s.strip(chars)`.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// Python `s.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}

/// Python `str.isupper()`: at least one cased character and no lowercase one.
pub fn py_isupper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_lowercase() {
            return false;
        }
        cased |= c.is_uppercase();
    }
    cased
}

/// All start offsets (bytes) where `needle` occurs in `hay`, overlapping ones included.
pub fn occurrences<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    hay.char_indices()
        .map(|(i, _)| i)
        .filter(move |&i| !needle.is_empty() && hay[i..].starts_with(needle))
}

/// The character before byte offset `i` and the one at offset `j`.
pub fn around(hay: &str, i: usize, j: usize) -> (Option<char>, Option<char>) {
    (hay[..i].chars().next_back(), hay[j..].chars().next())
}

/// `(?<![a-z0-9]) needle (?![a-z0-9])` style search: `bad` tells which neighbours break a match.
pub fn has_bounded(hay: &str, needle: &str, bad: fn(char) -> bool) -> bool {
    occurrences(hay, needle).any(|i| {
        let (b, a) = around(hay, i, i + needle.len());
        !b.is_some_and(bad) && !a.is_some_and(bad)
    })
}

/// Number of non-overlapping bounded matches (Python `len(re.findall(...))`).
pub fn count_bounded(hay: &str, needle: &str, bad: fn(char) -> bool) -> usize {
    let mut n = 0;
    let mut from = 0;
    for i in occurrences(hay, needle) {
        if i < from {
            continue;
        }
        let (b, a) = around(hay, i, i + needle.len());
        if !b.is_some_and(bad) && !a.is_some_and(bad) {
            n += 1;
            from = i + needle.len();
        }
    }
    n
}

pub fn az(c: char) -> bool {
    c.is_ascii_lowercase()
}

pub fn az09(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.375, 2), 0.38);
    }

    #[test]
    fn bounded_search() {
        assert!(has_bounded("labnorte informe", "labnorte", az09));
        assert!(!has_bounded("xlabnorte", "labnorte", az09));
        assert_eq!(count_bounded("rx rx rxa", "rx", az), 2);
    }
}
