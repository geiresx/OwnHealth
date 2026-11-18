# Calculators

OwnHealth runs a few published formulas on the lab values a person has recorded. Each result shows its
formula version, its citation and the values it used, each with its source. When a result cannot be
computed, it says why. This is information, not diagnosis.

| Calculator | Page | Inputs (one draw) | Validity |
|---|---|---|---|
| eGFR, CKD-EPI 2021 creatinine | [ckd-epi-2021.md](ckd-epi-2021.md) | creatinine | adults (18 and over), sex at birth recorded |
| eGFR, CKD-EPI 2021 creatinine-cystatin C | [ckd-epi-2021.md](ckd-epi-2021.md) | creatinine, cystatin C | as above |
| FIB-4 | [fib-4.md](fib-4.md) | AST, ALT, platelets | age 35 and over |
| HOMA-IR | [homa-ir.md](homa-ir.md) | glucose, insulin | same draw; says when fasting is not recorded |
| SCORE2, SCORE2-OP | [score2.md](score2.md) | total and HDL cholesterol; systolic pressure, smoking status, country | 40 to 69, and 70 to 89; apparently healthy people |
| PREVENT (total CVD, ASCVD) | [prevent.md](prevent.md) | total and HDL cholesterol; systolic pressure, smoking status, eGFR, diabetes, medicines | 30 to 79, no cardiovascular disease; the published input ranges |

Two more pages explain how the rule pack judges change over time in one marker. They are rules, not
calculators, and their flags enter the state body:

- [rcv.md](rcv.md): the reference change value, its EuBIVAS CVs with their DOIs, and why a change of
  lab or method is not judged.
- [baseline.md](baseline.md): the person's usual level from 3 to 5 earlier results.

## How a result is made

- **Computed on read.** Nothing is stored. Each read builds the document `ownhealth.calculators/v1`
  for one profile and one `as_of` day. Its `calculators_hash` is the sha256 of the same canonical JSON
  that `state_hash` uses. The document never enters the hashed state body.
- **One draw.** All inputs of one result come from one draw: the lab report (`report_key`), else the
  document and the day, else the day. A document alone is not a draw, because a FHIR bundle or an
  Apple Health export with no DiagnosticReport holds values from many days. Inputs with different
  days are refused (`inputs_not_same_draw`). A formula runs once for each draw that holds all its
  inputs, so the kidney card can show eGFR over time. The cardiovascular risk calculators are the
  exception: they give one result on the day, from the newest lipid draw and the facts beside it.
- **Units.** `data/calculators/units.yaml` lists each unit a formula accepts and the number the
  printed value is divided by, with its citation. A unit with no row is refused (`unit_unsupported`).
  No factor is guessed.
- **Arithmetic.** Decimal arithmetic (`rust_decimal`, with exp and ln), never floating point. The value
  is rounded to the precision in `data/calculators/calculators.yaml`.
- **Refusals.** A result that fails a validity rule has status `refused`. A missing input gives
  `not_assessed`. Neither carries a value. The reason codes are `missing_input`,
  `inputs_not_same_draw`, `not_adult`, `age_below_35`, `sex_not_recorded`,
  `birth_date_not_recorded`, `unit_unsupported`, `not_a_number` (a value printed as `< 0.3`, or text)
  and `out_of_range`. The cardiovascular calculators add `age_out_of_range`, `country_not_recorded`,
  `region_not_covered`, `outside_score2_scope`, `outside_prevent_scope`, `egfr_below_60`,
  `marked_risk_factor` and `input_out_of_range`.

## Where to read it

- REST: `GET /api/profiles/{p}/calculators?as_of=&lang=`, with scope `read:labs`.
- MCP: the resource `ownhealth://profile/{id}/calculators`, with scope `read:labs`. There is no tool.
  A token in `minimal` identity mode reads every date relative to `as_of`, as in the tool answers.
- UI: the Calculators card on a biomarker page lists every calculator that reads that marker. The
  Related markers card of a kidney or liver marker also shows the computed eGFR or FIB-4.

## How the numbers are checked

`server/crates/ownhealth-rules/tests/fixtures/calc/*.json` holds vectors. Each file names its source:
the R package kidney.epi 1.4.0 for CKD-EPI 2021, the paper's formula for FIB-4 and HOMA-IR, the
papers' examples through RiskScorescvd 0.3.1 for SCORE2 and SCORE2-OP, and the supplement and preventr
0.12.2 for PREVENT (`tests/cardiovascular.rs`). Every
unit the units file accepts has at least one vector. `tests/calculators.rs` checks every vector to 2
decimals, and a control proves that one wrong expected value fails the suite.
