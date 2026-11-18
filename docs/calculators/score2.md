# SCORE2 and SCORE2-OP

**Source.** SCORE2 working group and ESC Cardiovascular risk collaboration. SCORE2 risk prediction
algorithms: new models to estimate 10-year risk of cardiovascular disease in Europe. Eur Heart J
2021;42:2439-2454. https://pubmed.ncbi.nlm.nih.gov/34120177/

SCORE2-OP working group and ESC Cardiovascular risk collaboration. SCORE2-OP risk prediction
algorithms: estimating incident cardiovascular event risk in older persons in four geographical risk
regions. Eur Heart J 2021;42:2455-2467. https://pubmed.ncbi.nlm.nih.gov/34120185/

Who it is for: the 2021 ESC Guidelines on cardiovascular disease prevention.
https://academic.oup.com/eurheartj/article/42/34/3227/6358713

**What it gives.** The 10-year risk, in %, of fatal and non-fatal cardiovascular disease. SCORE2
covers ages 40 to 69. SCORE2-OP covers age 70 and over (we take it to 89, the oldest age of its
charts). A profile gets one of the two, never both.

**Formulas.** Total and HDL cholesterol in mmol/L, systolic pressure in mmHg, age in whole years.

```text
SCORE2:     cage = (age - 60)/5, csbp = (sbp - 120)/20, ctc = tc - 6, chdl = (hdl - 1.3)/0.5
SCORE2-OP:  cage = age - 73,     csbp = sbp - 150,      ctc = tc - 6, chdl = hdl - 1.4
x          = the sex's coefficients x (cage, smoking, csbp, ctc, chdl, and each of them x cage)
risk       = 1 - S0^exp(x)                (SCORE2-OP: 1 - S0^exp(x - mean x))
region     = 1 - exp(-exp(scale1 + scale2 x ln(-ln(1 - risk))))
```

The coefficients are in `server/crates/ownhealth-rules/src/calc/score2.rs`. The diabetes term is
always 0 here, because a person with diabetes recorded is refused first.

**Region.** The profile's country picks the ESC risk region, and the region picks scale1 and scale2.
`data/calculators/score2_regions.yaml` lists the four regions, their countries and their numbers.
Spain, France and the United Kingdom are in the low-risk region, for example, and Poland is in the
high-risk region. With no country recorded the result is refused (`country_not_recorded`). With a
country in no region, such as US, it is refused too (`region_not_covered`). We never guess a region.

**Inputs we read.**

- Total and HDL cholesterol from one draw and one day, the newest by that day. A value in mg/dL is
  divided by 38.67. For the marked-factor rule, LDL-C and total cholesterol are each the newest by
  their own day too, and every value from that day counts. A value printed as above a bound (">5.2")
  is judged by that bound, and one printed as above the limit itself (">8") is over the limit.
- Systolic pressure: the newest office reading (LOINC 8480-6) in the 12 months up to the day. If
  there is none, the mean of the last 30 days of wearable daily means, shown with its window and its
  wearable source (the preferred one, else the one with the most days in those 30). The result
  then carries the limitation `sbp_from_wearable`.
- Smoking status: the newest answer to LOINC 72166-2 (SNOMED CT). Only "smokes now" counts as a
  smoker. The app records it by hand on the lipid page. A newest answer with no known code is
  refused (`smoking_status_not_coded`) with its words and source; an older answer never stands in.
- The recorded conditions, matched by `data/calculators/conditions.yaml`. An exclusion such as
  "riesgo de", "screening" or "family history of" holds only in its own clause and only before the
  disease, so "Riesgo de diabetes" and "Screening for chronic kidney disease" are not that disease,
  while "diabetes con riesgo de complicaciones", "Diabetes mellitus tipo 2: riesgo de hipoglucemia",
  "Type 2 diabetes: family history of stroke" and "Type 2 diabetes (no family history)" keep their
  diagnosis. A qualifier such as
  "gestational", "insipidus" or a bare "family history" excludes wherever it stands in the clause,
  so "Diabetes mellitus (gestational)", "Family history: diabetes" and "Stroke (family history)" are
  not that disease.

Each input line names its source. A condition that is not on record reads as "no diabetes recorded",
never as "no diabetes".

**Validity.** Each rule is a refusal with its reason. None of them carries a value.

- Age 40 to 69 for SCORE2, 70 to 89 for SCORE2-OP (`age_out_of_range`).
- Sex at birth and birth date recorded (`sex_not_recorded`, `birth_date_not_recorded`).
- SCORE2 is for apparently healthy people. It is refused when any of these is recorded:
  established cardiovascular disease, diabetes, chronic kidney disease or familial
  hypercholesterolaemia (`outside_score2_scope`), or a computed eGFR below 60 (`egfr_below_60`).
- A markedly raised single risk factor puts a person outside SCORE2: total cholesterol above
  8 mmol/L (310 mg/dL), LDL-C above 4.9 mmol/L (190 mg/dL), or systolic pressure of 180 mmHg or
  more (`marked_risk_factor`).
- A systolic pressure of zero or below is no reading, so the risk is refused (`out_of_range`).
- A missing cholesterol, systolic pressure or smoking status gives `not_assessed` and names it
  (`missing_input`).
- A caller without read:records or read:vitals gets `not_assessed` (`scope_missing`) with no input
  line: the result reads conditions, medicines and vital signs. Without read:wearables the wearable
  mean is not read.

**Precision.** 1 decimal.

**Worked example.** A man aged 50 in the low-risk region who smokes, with systolic pressure
140 mmHg, total cholesterol 6.3 mmol/L and HDL 1.4 mmol/L: 6.3%. This is the paper's own example.

**Code.** `server/crates/ownhealth-rules/src/calc/score2.rs` and `cardio.rs`. Vectors:
`tests/fixtures/calc/score2.json` and `score2_op.json`, from the papers' supplementary examples as
the R package RiskScorescvd 0.3.1 checks them.
