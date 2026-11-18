# Guideline files: how to add a country

The Due screen lists the screenings and vaccines that a country's guideline lists for a person's age
and sex. Each country is one YAML file in [data/prevention/](../data/prevention). You can add your
country's schedule without writing Rust. You write the file, and a maintainer adds the code lines
and the test if you ask in the pull request.

This is information, not diagnosis. A row says what the guideline lists and links to it. It never
tells the person what to do.

## The files

| File | What it holds |
|---|---|
| `pack.yaml` | The country list (`countries`, ISO 3166-1 alpha-2 code to file), the days ahead that read as upcoming (90), and the answers a person can give about themselves (`answer_keys`) |
| `<cc>.yaml` | One country: its header and its rows, for example [es.yaml](../data/prevention/es.yaml) and [us.yaml](../data/prevention/us.yaml) |
| `evidence.yaml` | What counts as done for a screening: names, SNOMED CT and LOINC codes |
| `vaccines.tsv` | What counts as a dose of each vaccine: CVX codes and names in English and Spanish |
| `templates.yaml` | The sentences that the Due screen shows beside a row, in English and Spanish |

The loader (`server/crates/ownhealth-rules/src/prevention.rs`) checks every file. It refuses a file
that breaks a rule on this page, and the tests fail with the reason.

## Write in our own words

- Read the guideline. Then write the row from what it lists: the ages, the sex, the interval, the
  number of doses and the test or vaccine that counts. Never copy the guideline's sentences or tables
  into a file, a comment or a template.
- The row's `name` is our own short name, in English and Spanish, for example
  `{en: "Colorectal cancer screening", es: "Cribado de cáncer colorrectal"}`.
- Never write advice. The wording lint (`tests/wording_all.rs`, the CI step "Wording lint") refuses
  imperative phrases such as "you should", "take" or "stop" in every template.
- Each row links to the page it comes from, in `cite`. The link starts with `https://`.

## Licences of the sources

Read the terms of a source before you add it, and add a row to this table in the same pull request.

| Source | File | Terms | What the file does about them |
|---|---|---|---|
| PAPPS 2024 cancer prevention update (semFYC), in Atención Primaria | `es.yaml` | Open access under CC BY-NC-ND (Europe PMC, PMC11705588) | Rows hold only ages, intervals and the tests that count, in our own words. No text of the article is in the repo. |
| Calendario de vacunación a lo largo de toda la vida 2026 (Consejo Interterritorial del SNS, Ministerio de Sanidad) | `es.yaml` | Reuse is allowed, commercial or not, when the source and the date of its last update are named and the content keeps its meaning: https://www.sanidad.gob.es/avisoLegal/home.htm | `source` and `cite` name the calendar, and `calendar_updated` holds its last update date. |
| USPSTF recommendations | `us.yaml` | AHRQ allows reuse of the unchanged work. An adaptation must say that AHRQ and HHS do not endorse it, and it may not be redistributed for a fee: https://www.uspreventiveservicestaskforce.org/uspstf/recommendation-topics/copyright-notice | `disclaimer` in English and Spanish, which the Due screen shows above the list. |
| CDC adult immunization schedule, and CVX codes | `us.yaml`, `vaccines.tsv` | Most CDC material is in the public domain. It may not suggest that the CDC endorses a product: https://www.cdc.gov/other/agencymaterials.html | The same `disclaimer` names the CDC. |

A source whose terms forbid this use, or whose terms you cannot find, does not go in. Say so in the
pull request and we look for another source.

## A country file

```yaml
# <Country>: our own reading of <guideline> and of <vaccine calendar>. Rows are for adults.
version: "prevention-xx 0.1.0"          # bump on every change
reviewer: "OwnHealth maintainers (not clinicians)"
reviewed: 2026-11-11                     # the day the reviewer last checked every row
disclaimer:                              # when the source's terms ask for one
  en: "..."
  es: "..."
rows:
  - id: xx.cancer.colorectal
    kind: every_n_months
    area: screening
    ages: [50, 74]
    months: 24
    evidence: [fit, colonoscopy]
    interval_for: {colonoscopy: 120}
    source: <guideline short name and year>
    cite: https://...
    name: {en: "Colorectal cancer screening", es: "Cribado de cáncer colorrectal"}
```

### Header fields

| Field | Required | Meaning |
|---|---|---|
| `version` | yes | `"prevention-<cc> <semver>"`. Bump it on every change. The Due screen and its hash name it. |
| `reviewer` | yes | The reviewer of record, see below. The loader refuses a file without it. |
| `reviewed` | yes | `YYYY-MM-DD`, the day the reviewer last checked every row against the source. The loader refuses a file without it. |
| `disclaimer` | when the terms ask | `{en, es}`, shown above the country's list. |

### Row fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | `<cc>.<area>.<name>`, unique across every country file. Never reuse an id for another row. |
| `kind` | yes | `once`, `every_n_months`, `series` or `seasonal` (below). |
| `area` | yes | `screening` or `vaccine`. |
| `ages` | yes | `[from, to]` in whole years. `to` is inclusive, and `null` means no end. |
| `sex` | no | `female` or `male`, when the row is for one sex at birth. |
| `evidence` | yes | The groups that count as done: ids from `evidence.yaml` or `vaccines.tsv`. |
| `source` | yes | The guideline's short name and year, for example `USPSTF 2024 (B)`. |
| `cite` | yes | `https://` link to the page that lists the row. |
| `name` | yes | `{en, es}`, our own short name. |
| `needs` | no | `{answer_key: "yes" \| "no"}`: the row applies only with that answer. |
| `unless` | no | `{answer_key: "yes" \| "no"}`: the row does not apply with that answer. |
| `count_from` | no | An age before which a dose or test does not count for this row. |
| `months`, `interval_for`, `doses`, `gap_days`, `season_start` | by kind | See the next section. |

A new test or vaccine needs a group in `evidence.yaml` or a line in `vaccines.tsv` first. A new
answer (`needs`, `unless`) needs a key in `pack.yaml` `answer_keys` and its question in English and
Spanish in `templates.yaml` `answers`.

## Intervals and series

| `kind` | Extra fields | When it is next due |
|---|---|---|
| `once` | none | At the start age, until one counted test or dose is on record. |
| `every_n_months` | `months`, and `interval_for: {group: months}` to give one evidence group another interval | `months` after the latest counted test, never before the start age. Example: colorectal screening with `months: 24` and `interval_for: {colonoscopy: 120}` is next due 24 months after a stool test, or 120 months after a colonoscopy. |
| `series` | `doses`, `gap_days` | A dose counts only when it comes `gap_days` or more after the previous counted dose. The next dose is due `gap_days` after the last one, until `doses` doses are on record. |
| `seasonal` | `season_start` (`MM-DD`) | One dose each season. A season runs 6 months from `season_start`. Off season with no dose, the row shows only when the next season starts within 90 days. |

Each item has one status:

- `overdue`: the interval or the gap after a counted test or dose has passed.
- `due`: never done, and the start age has passed.
- `upcoming`: due within 90 days.
- `up_to_date`: done, and next due more than 90 days ahead.

Past the end age the row lists no next date. Before the start age a row shows only
within 90 days of it, or when the record already holds a matching test or dose.

## not_assessed

When the profile lacks what a row needs, the item says why and shows no date. The status is
`not_assessed` with one `reason_code`:

| `reason_code` | When |
|---|---|
| `no_birth_date` | The profile has no birth date. |
| `no_sex` | The row is for one sex and the profile has no sex at birth. |
| `no_answer` | The row has `needs`, and the person has not given that answer. |

Two more codes are on the whole list, not on one item. Then only the lab gaps of the rule pack show:

| `reason_code` | When |
|---|---|
| `country_not_covered` | No file covers the profile's country. |
| `no_country` | The profile has no country. |

Never put a default in a row to avoid one of these. A row that cannot be checked says so.

## Lab-type screening lives in the rule pack

The rule pack already checks the screenings that are lab tests, such as hepatitis C antibody,
Lp(a), lipids and HbA1c. They are in [data/rules/pack/screening.yaml](../data/rules/pack/screening.yaml)
and in the recency rules of the other pack files. Each one has that one home. A country file names
the pack rule by id and gives its own source and link:

```yaml
  - id: us.infect.hcv
    pack_rule: infect.hcv.once_adult
    source: USPSTF 2020 (B)
    cite: https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/hepatitis-c-screening
```

The loader refuses an evidence group that covers an analyte the pack already checks, and a
`pack_rule` that is not a `never_measured` or `recency` rule. Never edit the rule pack in a country
pull request. Its hash is pinned, and a pack change is its own release step.

## The reviewer of record

Every country file names one reviewer of record in `reviewer`, and
[.github/CODEOWNERS](../.github/CODEOWNERS) names the same person for the file. GitHub asks that
person to review every pull request that changes the file. The reviewer checks each row against the
source, then sets `reviewed` to that day.

Until a clinician volunteers for a country, the reviewer of record is the maintainer, and the file
says `OwnHealth maintainers (not clinicians)`. A clinician who wants to review a country's file can
open an issue. We then put their name in `reviewer` and their GitHub account in CODEOWNERS.

## Add a country, step by step

1. Write the file `data/prevention/<cc>.yaml`, with `<cc>` the lower-case country code. Copy
   `es.yaml` or `us.yaml`, and replace every row.
2. List it in `data/prevention/pack.yaml` under `countries` (`XX: xx.yaml`).
3. Name the reviewer with a line `/data/prevention/<cc>.yaml @<account>` in `.github/CODEOWNERS`.
4. Add the file name to the `shipped!(...)` list in
   `server/crates/ownhealth-rules/src/prevention.rs`, which builds the files into the binary.
5. Add a fixture and a golden in `server/crates/ownhealth-rules/tests/prevention.rs`:
   - Add a synthetic person in the new country, like `us_profile` (Jordan Test). Give a made-up
     name, and a birth date and a sex that put several rows in their age window. Add one or two
     records that count as done. Never use a real person.
   - Add the person to `goldens_for_the_riveras_and_a_us_profile`, and assert one status and one due
     date in words, as the test does for Jordan.
   - Add the country code to the list in `the_shipped_files_load_and_every_row_is_cited`.
   - Re-record the golden, then read its diff line by line:

     ```sh
     cd server
     OWNHEALTH_RECORD_PREVENTION=1 cargo test -p ownhealth-rules --test prevention
     cargo test -p ownhealth-rules --test prevention
     cargo test -p ownhealth --test wording_all
     cd ..
     ```

   The test database is in [AGENTS.md](../AGENTS.md#dev-commands).
6. From the repository root, check the links with `bash scripts/cite-links.sh`. It lists each dead link and exits with 1 when it finds one.
7. Open the pull request ready for review. Give the source and its terms (a row in the licence
   table above), and say which steps you want the maintainer to do.

Steps 4 and 5 are Rust. If you do not write Rust, do steps 1 to 3, 6 and 7, and the maintainer
does the rest in your pull request.

## The weekly link check

[.github/workflows/cite-links.yml](../.github/workflows/cite-links.yml) runs
[scripts/cite-links.sh](../scripts/cite-links.sh) every Monday and on demand. It fetches every
`https://` link in the YAML, JSON and TSV files under `data/`. A link is dead when its host does not
resolve or refuses the connection, or when it answers 404 or 410. Then the run goes red and one issue, "Dead links in the cited data
files", lists each dead link and the files that cite it. The issue updates each week and closes when
no link is dead. A site that refuses scripts (401, 403, 429, a 5xx or a timeout) is listed apart in
the run summary and does not count as dead. The check is not a pull request gate.
