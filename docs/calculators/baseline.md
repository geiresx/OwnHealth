# Personal baseline (usual level)

**What it answers.** Is the last result outside this person's own usual level? A population reference
range is wide. A person's results usually stay in a much narrower band. A result can be inside the
lab's range and still be far from the person's usual level.

**Source.** Coşkun A et al. Personalized reference intervals in laboratory medicine: a new model based
on within-subject biological variation. Clin Chem 2021;67:374-384. https://doi.org/10.1093/clinchem/hvaa233

**Formula.** The rule `baseline.personal` in `data/rules/pack/pack.yaml`:

```text
set point  sp = mean of the n earlier results       (3 <= n <= 5, the most recent ones)
half width    = sp x z x √(CVa² + CVi²) x √(1 + 1/n) / 100
usual level   = sp - half width  to  sp + half width
flag when     |x - sp| > half width
```

`x` is the last result and `z` is 1.96. The CVs are the ones of the marker's RCV rule, so the same
EuBIVAS table feeds both. See [rcv.md](rcv.md) for the values and their DOIs. The `√(1 + 1/n)` term
widens the band when the set point rests on few results.

**Which results count.**

- Only earlier results with the same method and lab as the last result (see [rcv.md](rcv.md)). A result
  from another lab or method never moves the set point.
- At least 3 earlier results. With 2 or fewer there is no usual level and no flag.
- At most the 5 most recent earlier results, so the set point follows slow change and the state body
  stays small.
- Only the markers with an RCV rule: LDL cholesterol, hs-CRP, creatinine, ALT and TSH.

**Order in the rule pack.** `lab_range`, then `rcv`, then `baseline`, then `trend`. The first rule that
fires gives the marker its flag. A result outside the lab's printed range is flagged by the lab range,
and critical values stay the lab's own printed HH and LL.

**Worked example.** Carmen Ortega, creatinine at LabNorte (enzymatic): 0.80, 0.81 and 0.90 mg/dL, then
0.98 mg/dL. The set point is 0.8367 mg/dL. The half width is 0.8367 x 1.96 x √(1.1² + 4.4²) x √(1 +
1/3) / 100 = 0.0859 mg/dL. The usual level is 0.75 to 0.92 mg/dL, so 0.98 is outside it. The change
from 0.90 to 0.98 is 8.9%, inside the creatinine RCV of 12.6%. So only the baseline sees this drift.

**Where to read it.**

- State body and its render: a flag with reason `BASELINE_OUT`. The text names the usual level and the
  number of earlier results it rests on. Its sources are the earlier results and the last one.
- REST: `GET /api/profiles/{p}/trend?code=...` returns `stats.baseline` with `set_point`, `low`,
  `high`, `n`, `outside`, `rule` and `cite`, or `null` when there is no usual level yet. The band
  reads every earlier result, also the ones before the `window` the chart asks for.
- UI: the biomarker chart draws the usual level as a blue dashed band, with `n`. The "Is the last
  change real?" card shows the band and says whether the last result is inside it.

**Code.** `server/crates/ownhealth-rules/src/engine.rs` (`baseline_band`, `baseline`). Tests:
`server/crates/ownhealth-rules/tests/rule_pack_020.rs`.
