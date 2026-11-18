//! SCORE2 and SCORE2-OP, the 10-year risk of fatal and non-fatal cardiovascular disease of the ESC
//! (SCORE2 working group, Eur Heart J 2021;42:2439-2454, https://pubmed.ncbi.nlm.nih.gov/34120177/;
//! SCORE2-OP working group, Eur Heart J 2021;42:2455-2467, https://pubmed.ncbi.nlm.nih.gov/34120185/).
//! Cholesterol in mmol/L, systolic pressure in mmHg, age in whole years. The result is a percentage.
//!
//! ```text
//! SCORE2 (40-69):  cage = (age-60)/5, csbp = (sbp-120)/20, ctc = tc-6, chdl = (hdl-1.3)/0.5
//!   x    = b.cage + b.smoking + b.csbp + b.diabetes + b.ctc + b.chdl + age interactions
//!   risk = 1 - S0^exp(x)
//! SCORE2-OP (70+): cage = age-73, csbp = sbp-150, ctc = tc-6, chdl = hdl-1.4
//!   risk = 1 - S0^exp(x - mean x)
//! region:          10-year risk = 1 - exp(-exp(scale1 + scale2 x ln(-ln(1 - risk))))
//! ```
//!
//! The coefficients are the papers' supplementary tables. The region scales (scale1, scale2) are
//! data, in `data/calculators/score2_regions.yaml`. SCORE2 is for people without diabetes, so the
//! diabetes term is always 0 here: a person with diabetes recorded is refused before this runs.

use rust_decimal::{Decimal, MathematicalOps};

use super::Sex;

/// The inputs of one person.
#[derive(Debug, Clone, Copy)]
pub struct Inputs {
    pub age: u32,
    pub sex: Sex,
    pub smoker: bool,
    pub sbp: Decimal,
    pub total_chol_mmol_l: Decimal,
    pub hdl_mmol_l: Decimal,
}

/// The two recalibration numbers of one region and sex.
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    pub scale1: Decimal,
    pub scale2: Decimal,
}

fn d(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn flag(b: bool) -> Decimal {
    if b { Decimal::ONE } else { Decimal::ZERO }
}

/// 1 - exp(-exp(scale1 + scale2 x ln(-ln(1 - p)))), as a percentage.
fn recalibrate(p: Decimal, s: Scale) -> Option<Decimal> {
    if p <= Decimal::ZERO || p >= Decimal::ONE {
        return None;
    }
    let inner = (-(Decimal::ONE - p).checked_ln()?).checked_ln()?;
    let z = s.scale1.checked_add(s.scale2.checked_mul(inner)?)?;
    let r = Decimal::ONE - (-z.checked_exp()?).checked_exp()?;
    r.checked_mul(Decimal::ONE_HUNDRED)
}

/// `1 - s0^exp(x)`.
fn survival(s0: Decimal, x: Decimal) -> Option<Decimal> {
    let p = x.checked_exp()?.checked_mul(s0.checked_ln()?)?.checked_exp()?;
    Some(Decimal::ONE - p)
}

/// SCORE2 (ages 40 to 69) in % with the region's scale, unrounded.
pub fn score2(i: &Inputs, scale: Scale) -> Option<Decimal> {
    // age, smoking, sbp, tchol, hdl, smoking*age, sbp*age, tchol*age, hdl*age, baseline survival
    let b: [Decimal; 10] = match i.sex {
        Sex::Male => [
            d(3742, 4),
            d(6012, 4),
            d(2777, 4),
            d(1458, 4),
            d(-2698, 4),
            d(-755, 4),
            d(-255, 4),
            d(-281, 4),
            d(426, 4),
            d(9605, 4),
        ],
        Sex::Female => [
            d(4648, 4),
            d(7744, 4),
            d(3131, 4),
            d(1002, 4),
            d(-2606, 4),
            d(-1088, 4),
            d(-277, 4),
            d(-226, 4),
            d(613, 4),
            d(9776, 4),
        ],
    };
    let age = Decimal::from(i.age).checked_sub(d(60, 0))?.checked_div(d(5, 0))?;
    let smk = flag(i.smoker);
    let sbp = i.sbp.checked_sub(d(120, 0))?.checked_div(d(20, 0))?;
    let tc = i.total_chol_mmol_l.checked_sub(d(6, 0))?;
    let hdl = i.hdl_mmol_l.checked_sub(d(13, 1))?.checked_div(d(5, 1))?;
    let x = b[0] * age
        + b[1] * smk
        + b[2] * sbp
        + b[3] * tc
        + b[4] * hdl
        + b[5] * age * smk
        + b[6] * age * sbp
        + b[7] * age * tc
        + b[8] * age * hdl;
    recalibrate(survival(b[9], x)?, scale)
}

/// SCORE2-OP (age 70 and over) in % with the region's scale, unrounded.
pub fn score2_op(i: &Inputs, scale: Scale) -> Option<Decimal> {
    // age, smoking, sbp, tchol, hdl, smoking*age, sbp*age, tchol*age, hdl*age, baseline survival, mean x
    let b: [Decimal; 11] = match i.sex {
        Sex::Male => [
            d(634, 4),
            d(3524, 4),
            d(94, 4),
            d(850, 4),
            d(-3564, 4),
            d(-247, 4),
            d(-5, 4),
            d(73, 4),
            d(91, 4),
            d(7576, 4),
            d(929, 4),
        ],
        Sex::Female => [
            d(789, 4),
            d(4921, 4),
            d(102, 4),
            d(605, 4),
            d(-3040, 4),
            d(-255, 4),
            d(-4, 4),
            d(-9, 4),
            d(154, 4),
            d(8082, 4),
            d(229, 3),
        ],
    };
    let age = Decimal::from(i.age).checked_sub(d(73, 0))?;
    let smk = flag(i.smoker);
    let sbp = i.sbp.checked_sub(d(150, 0))?;
    let tc = i.total_chol_mmol_l.checked_sub(d(6, 0))?;
    let hdl = i.hdl_mmol_l.checked_sub(d(14, 1))?;
    let x = b[0] * age
        + b[1] * smk
        + b[2] * sbp
        + b[3] * tc
        + b[4] * hdl
        + b[5] * age * smk
        + b[6] * age * sbp
        + b[7] * age * tc
        + b[8] * age * hdl;
    recalibrate(survival(b[9], x.checked_sub(b[10])?)?, scale)
}
