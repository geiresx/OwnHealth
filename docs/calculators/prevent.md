# PREVENT

**Source.** Khan SS et al. Development and validation of the American Heart Association's PREVENT
equations. Circulation 2024;149:430-449. https://pubmed.ncbi.nlm.nih.gov/37947085/

**What it gives.** Two results for adults aged 30 to 79 without cardiovascular disease: the 10-year
risk, in %, of total cardiovascular disease (`prevent_cvd`) and of atherosclerotic cardiovascular
disease (`prevent_ascvd`). We ship the base model only. The heart failure equation and the HbA1c,
urine albumin-creatinine ratio and social deprivation add-ons are not here.

**Formula.** Cholesterol in mmol/L, systolic pressure in mmHg, eGFR in mL/min/1.73 m².

```text
cage     = (age - 55)/10
cnonhdl  = (tc - hdl) - 3.5            chdl    = (hdl - 1.3)/0.3
csbp_lt  = (min(sbp, 110) - 110)/20    csbp_ge = (max(sbp, 110) - 130)/20
cegfr_lt = (min(egfr, 60) - 60)/-15    cegfr_ge = (max(egfr, 60) - 90)/-15
x        = constant + the sex's coefficients x (the terms above, diabetes, smoking,
           antihypertensive, statin, and their interactions)
risk     = exp(x) / (1 + exp(x))
```

The coefficients are in `server/crates/ownhealth-rules/src/calc/prevent.rs`.

**Inputs we read.**

- Total and HDL cholesterol from one draw and one day, the newest by that day. A value in mg/dL is
  divided by 38.67, the same factor as PREVENT's own 0.02586.
- Systolic pressure and smoking status, as for SCORE2 (see [score2.md](score2.md)).
- eGFR: the newest CKD-EPI 2021 result of the same document, with the creatinine's source.
- Diabetes: recorded or not (`data/calculators/conditions.yaml`).
- Antihypertensive and statin use: an active medicine whose name or ingredient is in
  `data/calculators/drug_classes.yaml`. Spanish brand names count, so "Cardyl 20 mg" is a statin.

**Validity.** Each rule is a refusal with its reason. None of them carries a value.

- Age 30 to 79 (`age_out_of_range`). Sex at birth and birth date recorded.
- Established cardiovascular disease or heart failure recorded (`outside_prevent_scope`).
- The input ranges the equations were built on (`input_out_of_range`): total cholesterol 130 to
  320 mg/dL (3.36 to 8.28 mmol/L), HDL 20 to 100 mg/dL (0.52 to 2.59 mmol/L), systolic pressure
  90 to 200 mmHg, and eGFR 15 to 140.
- A missing cholesterol, systolic pressure, smoking status or eGFR gives `not_assessed` and names it.
- A smoking status with no known code (`smoking_status_not_coded`), and a caller without the scopes of
  the facts (`scope_missing`), as for SCORE2.

**Limitations.** PREVENT was built and validated in adults in the United States. A profile with
another country, or none, carries the limitation `prevent_us_cohorts`.

**Precision.** 1 decimal.

**Worked example.** A woman aged 50 with total cholesterol 200 mg/dL, HDL 45 mg/dL, systolic
pressure 160 mmHg on blood pressure medicine, diabetes, no smoking, no statin and eGFR 90: total
cardiovascular disease 14.7%, atherosclerotic cardiovascular disease 9.2%. This is the example of
the paper's supplement (Table S25).

**Code.** `server/crates/ownhealth-rules/src/calc/prevent.rs` and `cardio.rs`. Vectors:
`tests/fixtures/calc/prevent_cvd.json` and `prevent_ascvd.json`, from the supplement's example and
the published outputs of the R package preventr 0.12.2. preventr caps systolic pressure at 180, so
the cases above 180 come from the paper's equations.
