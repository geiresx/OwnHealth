//! PREVENT, the base equations of the American Heart Association for the 10-year risk of total
//! cardiovascular disease and of atherosclerotic cardiovascular disease (Khan SS et al., Circulation
//! 2024;149:430-449, https://pubmed.ncbi.nlm.nih.gov/37947085/). Adults 30 to 79 without
//! cardiovascular disease. Cholesterol in mmol/L, systolic pressure in mmHg, eGFR in
//! mL/min/1.73 m2. The heart failure equation and the HbA1c, UACR and SDI add-ons are not here.
//!
//! ```text
//! cage = (age-55)/10, cnonhdl = (tc-hdl) - 3.5, chdl = (hdl-1.3)/0.3,
//! csbp_lt = (min(sbp,110)-110)/20, csbp_ge = (max(sbp,110)-130)/20,
//! cegfr_lt = (min(egfr,60)-60)/-15, cegfr_ge = (max(egfr,60)-90)/-15
//! x    = constant + sum of the coefficients x terms (the supplement of the paper)
//! risk = exp(x) / (1 + exp(x))
//! ```
//!
//! The coefficients are the supplement's base 10-year model, as the R package preventr 0.12.2
//! ships them.

use rust_decimal::{Decimal, MathematicalOps};

use super::Sex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// total cardiovascular disease
    Cvd,
    /// atherosclerotic cardiovascular disease
    Ascvd,
}

/// The inputs of one person.
#[derive(Debug, Clone, Copy)]
pub struct Inputs {
    pub age: u32,
    pub sex: Sex,
    pub total_chol_mmol_l: Decimal,
    pub hdl_mmol_l: Decimal,
    pub sbp: Decimal,
    pub diabetes: bool,
    pub smoker: bool,
    pub egfr: Decimal,
    pub bp_treated: bool,
    pub statin: bool,
}

fn d(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn flag(b: bool) -> Decimal {
    if b { Decimal::ONE } else { Decimal::ZERO }
}

/// age, non-HDL-C, HDL-C, SBP < 110, SBP >= 110, diabetes, smoking, eGFR < 60, eGFR >= 60,
/// antihypertensive, statin, treated SBP >= 110, treated non-HDL-C, age x non-HDL-C, age x HDL-C,
/// age x SBP >= 110, age x diabetes, age x smoking, age x eGFR < 60, constant.
fn coefficients(outcome: Outcome, sex: Sex) -> [Decimal; 20] {
    let c = |v: [i64; 20]| v.map(|m| d(m, 7));
    match (outcome, sex) {
        (Outcome::Cvd, Sex::Female) => c([
            7939329, 305239, -1606857, -2394003, 3600781, 8667604, 5360739, 6045917, 433769, 3151672, -1477655,
            -663612, 1197879, -819715, 306769, -946348, -2705700, -787150, -1637806, -33077280,
        ]),
        (Outcome::Cvd, Sex::Male) => c([
            7688528, 736174, -954431, -4347345, 3362658, 7692857, 4386871, 5378979, 164827, 2888790, -1337349, -475924,
            1502730, -517874, 191169, -1049477, -2251948, -895067, -1543702, -30311680,
        ]),
        (Outcome::Ascvd, Sex::Female) => c([
            7198830, 1176967, -1511850, -835358, 3592852, 8348585, 4831078, 4864619, 397779, 2265309, -592374, -395762,
            844423, -567839, 325692, -1035985, -2417542, -791142, -1671492, -38199750,
        ]),
        (Outcome::Ascvd, Sex::Male) => c([
            7099847, 1658663, -1144285, -2837212, 3239977, 7189597, 3956973, 3690075, 203619, 2036522, -865581,
            -322916, 1145630, -300005, 232747, -927024, -2018525, -970527, -1217081, -35006550,
        ]),
    }
}

/// The 10-year risk in %, unrounded.
pub fn risk(outcome: Outcome, i: &Inputs) -> Option<Decimal> {
    let b = coefficients(outcome, i.sex);
    let (n110, n130, n60, n90) = (d(110, 0), d(130, 0), d(60, 0), d(90, 0));
    let age = Decimal::from(i.age).checked_sub(d(55, 0))?.checked_div(d(10, 0))?;
    let nonhdl = i.total_chol_mmol_l.checked_sub(i.hdl_mmol_l)?.checked_sub(d(35, 1))?;
    let hdl = i.hdl_mmol_l.checked_sub(d(13, 1))?.checked_div(d(3, 1))?;
    let sbp_lt = i.sbp.min(n110).checked_sub(n110)?.checked_div(d(20, 0))?;
    let sbp_ge = i.sbp.max(n110).checked_sub(n130)?.checked_div(d(20, 0))?;
    let egfr_lt = i.egfr.min(n60).checked_sub(n60)?.checked_div(d(-15, 0))?;
    let egfr_ge = i.egfr.max(n60).checked_sub(n90)?.checked_div(d(-15, 0))?;
    let (dm, smk, bp, st) = (flag(i.diabetes), flag(i.smoker), flag(i.bp_treated), flag(i.statin));
    let terms = [
        age,
        nonhdl,
        hdl,
        sbp_lt,
        sbp_ge,
        dm,
        smk,
        egfr_lt,
        egfr_ge,
        bp,
        st,
        bp * sbp_ge,
        st * nonhdl,
        age * nonhdl,
        age * hdl,
        age * sbp_ge,
        age * dm,
        age * smk,
        age * egfr_lt,
        Decimal::ONE,
    ];
    let x = b
        .iter()
        .zip(terms)
        .try_fold(Decimal::ZERO, |acc, (c, t)| acc.checked_add(c.checked_mul(t)?))?;
    let e = x.checked_exp()?;
    e.checked_div(Decimal::ONE + e)?.checked_mul(Decimal::ONE_HUNDRED)
}
