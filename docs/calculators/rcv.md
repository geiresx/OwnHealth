# Reference change value (RCV)

**What it answers.** Two results of one marker differ. Is the difference larger than the normal
day-to-day variation of the person and of the lab method? The RCV is the smallest change, in percent,
that is unlikely (5% two-sided) to come from that variation alone.

**Formula.**

```text
RCV % = 1.96 x √2 x √(CVi² + CVa²)
```

CVi is the within-person biological variation and CVa is the analytical variation of the method, both
in percent. 1.96 is the two-sided 95% z value. √2 is there because both results carry the variation.

**Source of the numbers.** Every CV comes from the European Biological Variation Study (EuBIVAS). The
values are the ones in Table 2 of the summary report: Carobene A et al. The European Biological
Variation Study (EuBIVAS): a summary report. Clin Chem Lab Med 2022;60:505-517.
https://doi.org/10.1515/cclm-2021-0370. Each rule also names the primary EuBIVAS paper for its
marker in `primary`.

| Marker | Rule | CVi % | CVa % | RCV % | Primary paper |
|---|---|---|---|---|---|
| LDL cholesterol | `rcv.ldl_c` | 8.46 | 1.77 | 24.0 | https://doi.org/10.1373/clinchem.2018.288415 |
| hs-CRP | `rcv.crp_hs` | 29.4 | 11.1 | 87.1 | https://doi.org/10.1373/clinchem.2019.304618 |
| Creatinine (enzymatic) | `rcv.creatinine` | 4.4 | 1.1 | 12.6 | https://doi.org/10.1373/clinchem.2017.275115 |
| ALT | `rcv.alt` | 9.3 | 6.7 | 31.8 | https://doi.org/10.1373/clinchem.2016.269811 |
| TSH | `rcv.tsh` | 18.9 | 1.46 | 52.5 | https://doi.org/10.1515/cclm-2020-1885 |

Notes on the table:

- **TSH.** The summary table gives CVi 18.9%. The abstract of the primary paper gives 17.7%. We use
  the summary table, the same source as every other row.
- **Creatinine.** The values are for the enzymatic method. The alkaline picrate (Jaffe) method gave CVi
  4.7% and CVa 4.4%. A Jaffe result and an enzymatic result are not compared, see below. The LOINC
  codes we ship do not tell the two methods apart, so a creatinine result with no method recorded is
  judged with the enzymatic values. An install that knows its lab uses Jaffe can override the rule.
- **HbA1c and ferritin** have no RCV in pack 0.2.0. We found no EuBIVAS estimate for them that we may
  ship. An install can add a rule in its override directory (`OWNHEALTH_RULES_OVERRIDE_DIR`). The pack
  version then ends in `+local.<sha8>`, so the change is visible in every answer.

**Only like with like.** A change is judged only between two results with the same method and the same
lab (the provider name of the source document). The method is the one stored with the result, else the
one the shipped LOINC table gives its code (calculated or direct LDL, the eGFR equation: CKD-EPI,
CKD-EPI 2021 or MDRD). Two codes that differ only in the unit name the same method: creatinine in mg/dL
(LOINC 2160-0) and in µmol/L (14682-9) compare through their canonical value. The comparison ignores
case and extra spaces. A missing lab name or method counts as one more value: two results that both
have no lab name count as the same lab, even when they came from two labs, and two results that both
have no method count as the same method. A result with a lab name and one without differ. When the last
two results differ in method or lab:

- the rule pack raises no RCV flag;
- the REST trend gives `exceeds_rcv: null`, `method_change: true` and a sentence in `limitations`;
- the MCP tool `get_trend` marks the point with `method_change: true` and adds a limitation.

A trend rule also uses only the points with the same method and lab as the last point.

**A rule must cite its paper.** The pack refuses to load an `rcv` or `baseline` rule whose `cite` is
not a DOI link (`https://doi.org/...`).

**Where to read it.**

- REST: `GET /api/profiles/{p}/trend?code=...` returns `stats.rcv_pct`, `stats.exceeds_rcv`,
  `stats.cv_i`, `stats.cv_a`, `stats.rcv_cite` and `stats.method_change`, and `method_change` on
  each point.
- MCP: `get_trend` returns `method_change` on a raw point that changed method or lab.
- UI: the "Is the last change real?" card of a biomarker page shows the RCV, the two CVs and a link to
  their source.

**Code.** `server/crates/ownhealth-rules/src/engine.rs` (`rcv_percent`, `rcv`, `method_key`). Tests:
`server/crates/ownhealth-rules/tests/rule_pack_020.rs`.
