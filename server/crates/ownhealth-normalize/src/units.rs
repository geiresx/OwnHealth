//! UCUM for the units labs print: parse, map printed spellings to UCUM, dimensions, conversion
//! (`normalize/_units.py`).
//!
//! A deliberately small UCUM subset (ingestion.md 8.3): the atoms and prefixes that appear on lab
//! reports, products and quotients, integer exponents, `10*N` factors and `{annotations}`. Every
//! unit becomes (factor to base units, dimension vector). Base units: g, m, s, mol, plus four
//! dimensions UCUM treats as arbitrary and we never cross: [IU], [arb'U], eq, [pH], and [HPF].
//!
//! Traps this module exists to hold:
//! * `[IU]` and `U` are different things (U is µmol/min, an enzyme activity) and never convert.
//! * `eq` is kept apart from `mol`; only an analyte's valence converts mEq to mmol.
//! * Mass <-> substance needs the analyte's molar mass (molar table), never a guess.
//! * `%` <-> `mmol/mol` for HbA1c is the NGSP/IFCC master equation, not a factor of 10.

use std::sync::LazyLock;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Zero};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;

pub const BASES: [&str; 9] = ["g", "m", "s", "mol", "IU", "ARB", "EQ", "PH", "HPF"];
pub type Dims = [i32; 9];
pub const ZERO: Dims = [0; 9];

const G: usize = 0;
const M: usize = 1;
const S: usize = 2;
const MOL: usize = 3;
const IU: usize = 4;
const ARB: usize = 5;
const EQ: usize = 6;
const PH: usize = 7;
const HPF: usize = 8;

/// `_dim(**exps)`.
pub const fn dim(pairs: &[(usize, i32)]) -> Dims {
    let mut d = ZERO;
    let mut i = 0;
    while i < pairs.len() {
        d[pairs[i].0] = pairs[i].1;
        i += 1;
    }
    d
}

pub const D_MASS_CONC: Dims = dim(&[(G, 1), (M, -3)]);
pub const D_SUBST_CONC: Dims = dim(&[(MOL, 1), (M, -3)]);
pub const D_EQ_CONC: Dims = dim(&[(EQ, 1), (M, -3)]);
pub const D_IU_CONC: Dims = dim(&[(IU, 1), (M, -3)]);
pub const D_ARB_CONC: Dims = dim(&[(ARB, 1), (M, -3)]);
pub const D_CAT_CONC: Dims = dim(&[(MOL, 1), (S, -1), (M, -3)]);
pub const D_NUM_CONC: Dims = dim(&[(M, -3)]);
pub const D_MASS_PER_MOL: Dims = dim(&[(G, 1), (MOL, -1)]);
pub const D_MASS: Dims = dim(&[(G, 1)]);
pub const D_VOL: Dims = dim(&[(M, 3)]);
pub const D_VOL_RATE: Dims = dim(&[(M, 3), (S, -1)]);
pub const D_OSMOL: Dims = dim(&[(MOL, 1), (G, -1)]);
pub const D_PRESSURE: Dims = dim(&[(G, 1), (M, -1), (S, -2)]);
pub const D_PH: Dims = dim(&[(PH, 1)]);
pub const D_TIME: Dims = dim(&[(S, 1)]);
pub const D_SPEED: Dims = dim(&[(M, 1), (S, -1)]);
pub const D_PER_HPF: Dims = dim(&[(HPF, -1)]);
pub const D_MASS_RATE: Dims = dim(&[(G, 1), (S, -1)]);

fn ratio(n: i64, d: i64) -> BigRational {
    BigRational::new(BigInt::from(n), BigInt::from(d))
}

fn int(n: i64) -> BigRational {
    BigRational::from_integer(BigInt::from(n))
}

/// canonical atom -> (factor to base, dimension, prefixable)
fn atom_def(atom: &str) -> Option<(BigRational, Dims, bool)> {
    Some(match atom {
        "g" => (int(1), D_MASS, true),
        "m" => (int(1), dim(&[(M, 1)]), true),
        "L" => (ratio(1, 1000), D_VOL, true),
        "s" => (int(1), D_TIME, true),
        "min" => (int(60), D_TIME, false),
        "h" => (int(3600), D_TIME, false),
        "d" => (int(86400), D_TIME, false),
        "mol" => (int(1), dim(&[(MOL, 1)]), true),
        "kat" => (int(1), dim(&[(MOL, 1), (S, -1)]), true),
        "U" => (ratio(1, 60_000_000), dim(&[(MOL, 1), (S, -1)]), true), // 1 U = 1 µmol/min
        "[IU]" => (int(1), dim(&[(IU, 1)]), true),
        "[arb'U]" => (int(1), dim(&[(ARB, 1)]), true),
        "eq" => (int(1), dim(&[(EQ, 1)]), true),
        "osm" => (int(1), dim(&[(MOL, 1)]), true),
        "[pH]" => (int(1), D_PH, false),
        "[HPF]" => (int(1), dim(&[(HPF, 1)]), false),
        "%" => (ratio(1, 100), ZERO, false),
        "mm[Hg]" => (ratio(133322387415, 1000000), D_PRESSURE, false),
        _ => return None,
    })
}

fn prefix(c: char) -> Option<BigRational> {
    let p = |e: u32| BigRational::new(BigInt::one(), BigInt::from(10u8).pow(e));
    Some(match c {
        'k' => int(1000),
        'd' => p(1),
        'c' => p(2),
        'm' => p(3),
        'u' => p(6),
        'n' => p(9),
        'p' => p(12),
        'f' => p(15),
        _ => return None,
    })
}

/// Lower-case spellings seen on reports -> canonical atom. Tried whole first, then prefix + atom.
fn atom_lc(word: &str) -> Option<&'static str> {
    Some(match word {
        "g" | "gr" => "g",
        "m" => "m",
        "l" => "L",
        "s" | "seg" | "sec" | "sg" => "s",
        "min" => "min",
        "h" | "hr" | "hrs" | "hora" => "h",
        "d" | "dia" | "day" => "d",
        "mol" => "mol",
        "kat" => "kat",
        "u" => "U",
        "iu" | "ui" | "ie" | "[iu]" => "[IU]",
        "[arb'u]" => "[arb'U]",
        "eq" => "eq",
        "osm" => "osm",
        "[ph]" => "[pH]",
        "[hpf]" | "hpf" | "campo" => "[HPF]",
        "%" => "%",
        "mmhg" | "mm[hg]" => "mm[Hg]",
        _ => return None,
    })
}

/// `_PREFIXABLE_LC`: a spelling of a prefixable atom, at most 4 characters.
fn prefixable_lc(word: &str) -> Option<&'static str> {
    let atom = atom_lc(word)?;
    let (_, _, prefixable) = atom_def(atom)?;
    (prefixable && word.chars().count() <= 4).then_some(atom)
}

/// Whole printed spellings the grammar cannot read. French labs print cell counts as G/L (giga,
/// 10^9) and T/L (tera, 10^12); lower case g/L is grams, so these are read before lower-casing.
fn cased(s: &str) -> Option<&'static str> {
    Some(match s {
        "G/L" | "G/l" | "Giga/L" | "Giga/l" => "10*9/L",
        "T/L" | "T/l" | "Tera/L" | "Tera/l" => "10*12/L",
        _ => return None,
    })
}

fn synonym(low: &str) -> Option<&'static str> {
    Some(match low {
        "mg%" | "mg/100ml" => "mg/dL",
        "g%" => "g/dL",
        "miles/ul" | "mil/ul" | "x1000/ul" | "k/ul" | "thou/ul" => "10*3/uL",
        "mill/ul" | "millones/ul" | "m/ul" => "10*6/uL",
        "cel/ul" | "cél/ul" | "celulas/ul" | "células/ul" | "cells/ul" => "/uL",
        "/mm3" | "cel/mm3" => "/mm3",
        "/campo" | "cel/campo" | "xcampo" | "porcampo" | "/hpf" | "/cga" => "/[HPF]",
        "mm/1ah" | "mm/1ªh" | "mm/1h" | "mm/1ahora" | "mm/1ªhora" | "mm/hr" | "mm/hora" => "mm/h",
        "ratio" | "indice" | "índice" => "{ratio}",
        "inr" => "{INR}",
        "ph" | "unidadesdeph" => "[pH]",
        "g/24h" | "g/24horas" => "g/(24.h)",
        "mg/24h" | "mg/24horas" => "mg/(24.h)",
        "l/24h" => "L/(24.h)",
        "ml/24h" => "mL/(24.h)",
        _ => return None,
    })
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("static regex"));
    };
}

re!(TEN_POWER, r"^(?:x|\*)?10(?:\^|e|\*|exp)?(\d{1,2})/(.+)$");
re!(PER_AREA, r"/1[.,]73(?:m2|m\^2|sqm|msq)?$");
re!(CREAT_SUFFIX, r"(?i)\s*(?:de\s+)?(?:creat\w*|crea)\.?\s*$");
re!(
    TOKEN,
    r"\{[^}]*\}|mm\[Hg\]|[kdcmunpf]?\[[^\]]*\]|\d+\*[-+]?\d+|[A-Za-z%']+|[-+]?\d+|[./()]"
);
re!(INTEGER, r"^[-+]?\d+$");
re!(ATOM_PRINTED, r"^(\d+)?([a-z%'\[\]]+?)(-?\d)?$");
re!(WHITESPACE, r"\s+");

fn superscript(c: char) -> char {
    match c {
        '⁰' => '0',
        '¹' => '1',
        '²' => '2',
        '³' => '3',
        '⁴' => '4',
        '⁵' => '5',
        '⁶' => '6',
        '⁷' => '7',
        '⁸' => '8',
        '⁹' => '9',
        '⁻' => '-',
        c => c,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub ucum: String,
    /// Multiply a value in this unit to get base units.
    pub factor: BigRational,
    pub dims: Dims,
}

impl Unit {
    pub fn dimensionless(&self) -> bool {
        self.dims == ZERO
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitError(pub String);

impl std::fmt::Display for UnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnitError {}

/// Python's `repr` of a str, for messages.
pub(crate) fn py_repr(s: &str) -> String {
    crate::text::py_repr(s)
}

// ------------------------------------------------------------------ strict UCUM ----

fn pow(f: &BigRational, exp: i64) -> BigRational {
    let e = i32::try_from(exp).unwrap_or(i32::MAX);
    num_traits::Pow::pow(f, e)
}

struct Parser<'a> {
    code: &'a str,
    tokens: Vec<&'a str>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.pos).copied()
    }

    fn term(&mut self) -> Result<(BigRational, Dims), UnitError> {
        let mut f = BigRational::one();
        let mut d = ZERO;
        let mut op = ".";
        if self.peek() == Some("/") {
            op = "/";
            self.pos += 1;
        }
        while let Some(t) = self.peek()
            && t != ")"
        {
            let (cf, cd) = self.component()?;
            if op == "." {
                f *= cf;
                for i in 0..d.len() {
                    d[i] += cd[i];
                }
            } else {
                if cf.is_zero() {
                    return Err(UnitError(format!("division by zero in {}", py_repr(self.code))));
                }
                f /= cf;
                for i in 0..d.len() {
                    d[i] -= cd[i];
                }
            }
            match self.peek() {
                Some(t @ ("." | "/")) => {
                    op = if t == "." { "." } else { "/" };
                    self.pos += 1;
                }
                Some(t) if t != ")" => {
                    return Err(UnitError(format!(
                        "unexpected {} in {}",
                        py_repr(t),
                        py_repr(self.code)
                    )));
                }
                _ => {}
            }
        }
        Ok((f, d))
    }

    fn component(&mut self) -> Result<(BigRational, Dims), UnitError> {
        let Some(tok) = self.peek() else {
            return Err(UnitError(format!("cannot read UCUM {}", py_repr(self.code))));
        };
        self.pos += 1;
        if tok == "(" {
            let (f, d) = self.term()?;
            if self.peek() != Some(")") {
                return Err(UnitError(format!("unbalanced parentheses in {}", py_repr(self.code))));
            }
            self.pos += 1;
            return Ok((f, d));
        }
        if tok.starts_with('{') {
            return Ok((BigRational::one(), ZERO));
        }
        if let Some((base, exp)) = tok.split_once('*') {
            let base: BigInt = base.parse().map_err(|_| UnitError(format!("bad factor {tok}")))?;
            let exp: i64 = exp.parse().map_err(|_| UnitError(format!("bad factor {tok}")))?;
            if base.is_zero() && exp < 0 {
                return Err(UnitError(format!("bad factor {tok}")));
            }
            return Ok((pow(&BigRational::from_integer(base), exp), ZERO));
        }
        if INTEGER.is_match(tok) {
            let n: BigInt = tok
                .trim_start_matches('+')
                .parse()
                .map_err(|_| UnitError(format!("bad number {tok}")))?;
            return Ok((BigRational::from_integer(n), ZERO));
        }
        let mut exp: i64 = 1;
        if let Some(next) = self.peek()
            && INTEGER.is_match(next)
        {
            exp = next
                .trim_start_matches('+')
                .parse()
                .map_err(|_| UnitError(format!("bad exponent {next}")))?;
            self.pos += 1;
        }
        let (f, d) = atom(tok, self.code)?;
        if self.peek().is_some_and(|t| t.starts_with('{')) {
            self.pos += 1; // an annotation glued to the atom: mg{FEU}
        }
        let e = i32::try_from(exp).map_err(|_| UnitError(format!("bad exponent {exp}")))?;
        let mut out = ZERO;
        for i in 0..d.len() {
            out[i] = d[i].saturating_mul(e);
        }
        Ok((pow(&f, exp), out))
    }
}

fn atom(tok: &str, code: &str) -> Result<(BigRational, Dims), UnitError> {
    if let Some((f, d, _)) = atom_def(tok) {
        return Ok((f, d));
    }
    let mut chars = tok.chars();
    if let Some(first) = chars.next()
        && let Some(p) = prefix(first)
        && let Some((f, d, true)) = atom_def(chars.as_str())
    {
        return Ok((p * f, d));
    }
    Err(UnitError(format!("unknown unit {} in {}", py_repr(tok), py_repr(code))))
}

/// Parse a UCUM code from our subset. Annotations count as 1.
pub fn parse_ucum(code: &str) -> Result<Unit, UnitError> {
    let tokens: Vec<&str> = TOKEN.find_iter(code).map(|m| m.as_str()).collect();
    if tokens.concat() != code {
        return Err(UnitError(format!("cannot read UCUM {}", py_repr(code))));
    }
    let mut p = Parser { code, tokens, pos: 0 };
    let (f, d) = p.term()?;
    if let Some(t) = p.peek() {
        return Err(UnitError(format!("unexpected {} in {}", py_repr(t), py_repr(code))));
    }
    Ok(Unit {
        ucum: code.to_string(),
        factor: f,
        dims: d,
    })
}

pub fn dims_of(ucum: Option<&str>) -> Option<Dims> {
    match ucum {
        None | Some("") => None,
        Some(u) => parse_ucum(u).ok().map(|u| u.dims),
    }
}

pub fn dims_name(dims: Option<Dims>) -> String {
    let Some(dims) = dims else {
        return "unknown".into();
    };
    let parts: Vec<String> = BASES
        .iter()
        .zip(dims)
        .filter(|(_, e)| *e != 0)
        .map(|(b, e)| if e != 1 { format!("{b}^{e}") } else { b.to_string() })
        .collect();
    if parts.is_empty() { "1".into() } else { parts.join(".") }
}

// -------------------------------------------------------------- printed -> UCUM ----

pub fn fold_unit(printed: &str) -> String {
    let s: String = printed.chars().map(superscript).collect::<String>().nfkc().collect();
    let s = s.replace(['µ', 'μ'], "u").replace('×', "x").replace(['−', '–'], "-");
    let s = if s.contains('/') {
        CREAT_SUFFIX.replace_all(&s, "").into_owned()
    } else {
        s
    };
    WHITESPACE.replace_all(&s, "").into_owned()
}

/// The UCUM code for a unit as printed, or None when we cannot read it.
pub fn to_ucum(printed: Option<&str>) -> Option<String> {
    let s = fold_unit(printed?);
    if s.is_empty() || matches!(s.as_str(), "-" | "--" | ".") {
        return None;
    }
    if let Some(c) = cased(&s) {
        return Some(c.into());
    }
    let low = s.to_lowercase();
    if let Some(c) = synonym(&low) {
        return Some(c.into());
    }
    if let Some(m) = TEN_POWER.captures(&low) {
        let n: u32 = m[1].parse().ok()?;
        return from_printed(&m[2])
            .filter(|rest| !rest.is_empty())
            .map(|rest| format!("10*{n}/{rest}"));
    }
    if parse_ucum(&s).is_ok() {
        return Some(s); // already valid UCUM, case and all: keep it
    }
    from_printed(&low)
}

/// `re.split(r"([/.])", s)`: the pieces with the separators kept.
fn split_keep(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '/' || c == '.' {
            out.push(&s[start..i]);
            out.push(&s[i..i + 1]);
            start = i + 1;
        }
    }
    out.push(&s[start..]);
    out
}

fn from_printed(low: &str) -> Option<String> {
    let mut low = low.to_string();
    let mut area = "";
    if PER_AREA.is_match(&low) {
        low = PER_AREA.replace_all(&low, "").into_owned();
        area = "/{1.73_m2}";
    }
    let mut out = String::new();
    for (i, part) in split_keep(&low).into_iter().enumerate() {
        if part == "/" || part == "." {
            out.push_str(part);
            continue;
        }
        if part.is_empty() {
            if i == 0 {
                continue; // leading "/"
            }
            return None;
        }
        out.push_str(&atom_printed(part)?);
    }
    out.push_str(area);
    parse_ucum(&out).ok().map(|_| out)
}

fn atom_printed(part: &str) -> Option<String> {
    let m = ATOM_PRINTED.captures(part)?;
    let count = m.get(1).map(|x| x.as_str());
    let word = m.get(2).map_or("", |x| x.as_str());
    let exp = m.get(3).map_or("", |x| x.as_str());
    let (pre, atom) = match atom_lc(word) {
        Some(a) => ("", a),
        None => {
            if let Some(rest) = word.strip_prefix("mc")
                && let Some(a) = prefixable_lc(rest)
            {
                ("u", a)
            } else {
                let mut chars = word.chars();
                let first = chars.next()?;
                match (prefix(first), prefixable_lc(chars.as_str())) {
                    (Some(_), Some(a)) => (&word[..first.len_utf8()], a),
                    _ => return None,
                }
            }
        }
    };
    let text = format!("{pre}{atom}{exp}");
    Some(match count {
        Some(c) => format!("({c}.{text})"),
        None => text,
    })
}

// ------------------------------------------------------------------ conversion ----

pub const HBA1C_GROUP: &str = "hba1c";

/// NGSP/IFCC master equation: IFCC (mmol/mol) = 10.929 x (NGSP % - 2.15). https://ngsp.org/ifccngsp.asp
fn ngsp() -> (BigRational, BigRational) {
    (ratio(10929, 1000), ratio(215, 100))
}

/// Exact conversion; None when the units do not convert for this analyte.
pub fn convert_fraction(
    value: &BigRational,
    src: &str,
    dst: &str,
    alias_group: &str,
    molar_mass: Option<&BigRational>,
    valence: Option<i64>,
) -> Option<BigRational> {
    if alias_group == HBA1C_GROUP && ((src == "%" && dst == "mmol/mol") || (src == "mmol/mol" && dst == "%")) {
        let (slope, offset) = ngsp();
        return Some(if src == "%" {
            slope * (value - offset)
        } else {
            value / slope + offset
        });
    }
    let a = parse_ucum(src).ok()?;
    let b = parse_ucum(dst).ok()?;
    let mut base = value * &a.factor;
    let diff: Vec<i32> = a.dims.iter().zip(b.dims).map(|(x, y)| x - y).collect();
    let (mass, eq, mol) = (diff[G], diff[EQ], diff[MOL]);
    if diff
        .iter()
        .enumerate()
        .any(|(i, d)| *d != 0 && i != G && i != MOL && i != EQ)
        || mass + eq + mol != 0
    {
        return None;
    }
    if mass != 0 {
        let mm = molar_mass?;
        if mm.is_zero() {
            return None;
        }
        base /= pow(mm, i64::from(mass));
    }
    if eq != 0 {
        let v = valence.filter(|v| *v != 0)?;
        base /= pow(&int(v), i64::from(eq));
    }
    if b.factor.is_zero() {
        return None;
    }
    Some(base / b.factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_separators() {
        assert_eq!(split_keep("/ul"), vec!["", "/", "ul"]);
        assert_eq!(split_keep("mg/dl"), vec!["mg", "/", "dl"]);
        assert_eq!(split_keep("ml/min/"), vec!["ml", "/", "min", "/", ""]);
    }

    #[test]
    fn a_trailing_slash_parses_as_python_does() {
        assert_eq!(parse_ucum("mg/").map(|u| u.dims), Ok(D_MASS));
        assert!(parse_ucum("mg.(").is_err());
    }
}
