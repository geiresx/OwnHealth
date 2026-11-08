# Contributing to OwnHealth

Thank you for helping. Contributions are licensed under [AGPL-3.0-or-later](LICENSE), like the rest of the code.

## Ground rules

- **Synthetic data only.** Never put a real person's name, results, genome or lab report in an issue, a fixture, a test, a screenshot or a commit. Our test people are Alex, Sam and Lucía Rivera and Carmen Ortega; our lab is the fictional LabNorte. No real lab brands in fixtures either: a template is named after a lab, but its test slip prints a made-up lab name.
- **Information, not diagnosis.** Nothing OwnHealth writes tells a person what to do. No "you should", "take" or "stop". Status words are OK, Watch, Out, Stale and No data. Pharmacogenomics quotes the guideline and links it.
- **Every value keeps its source**: the document, page and position, or the import it came from.
- **Free text sent over MCP is marked** `untrusted_text`.
- **No chat agent inside the app.** The MCP server is the AI interface.
- The UI matches [the mockup](docs/mockup/index.html): dark theme, Spanish and English.

## Development setup

The commands are in [AGENTS.md](AGENTS.md#dev-commands). In short:

```sh
./scripts/test-db.sh                  # test Postgres 17 on 127.0.0.1:55432
bash scripts/fetch-pdfium.sh && cd server && mkdir -p static && cargo test -p <crate>
cd .. && bash contract-tests/run.sh rust
cd ui && pnpm install && pnpm run check && pnpm build
```

Each test creates its own database and drops it afterwards. The backup tests in `server/crates/ownhealth-ops/tests` need pg_dump and pg_restore 17 and restic, on PATH or in `OWNHEALTH_PG_BINDIR`. Without a local client, `OWNHEALTH_PG_BINDIR=$PWD/../scripts/pg-bin` runs them inside the test container; without restic the restic tests are skipped.

## Pull requests

- One change per pull request, with a test that fails without it.
- Conventional commits (`fix(pdf): ...`, `feat(mcp): ...`). The title says why, not what.
- Open it ready for review, never as a draft, so CI runs.
- Database changes are new numbered files in [migrations/](migrations). Never edit a migration that has shipped.
- [CONTRACTS.md](CONTRACTS.md) says which interface lives where. Change a shared signature only together with every caller.

## How to add a lab template

A template teaches the PDF importer one lab's layout. With a template, rows from that lab can be accepted without a click when every check passes. Without one, generic rules read the report and more rows go to the review queue.

1. **Create the file** `data/vendors/<country>/<lab>.yaml`. Copy [es/labnorte.yaml](data/vendors/es/labnorte.yaml), then add it to `VENDOR_FILES` in [templates.rs](server/crates/ownhealth-ingest-pdf/src/templates.rs), which builds the templates into the binary:

   ```yaml
   id: es/labnorte                 # <country>/<lab>, unique
   version: "1"                    # bump on every change
   provider_name: LabNorte
   locale: es                      # decimal comma, date order, language of the headers
   fingerprint:
     all: [labnorte]               # words that must all be on page 1
     any: [prueba, resultado, unidades, valores de referencia, fecha de toma]
   columns:                        # header words of each column, as printed
     name: [prueba]
     value: [resultado]
     unit: [unidades]
     ref_range: [valores de referencia]
     flag: [indicador]
   flag_tokens: [H, L, "*"]
   stop: [documento sintetico, validado por]   # a line starting with these ends the table
   skip: [comentario]                          # lines starting with these are not rows
   ```

   A template holds anchors only: header words and markers. Never a value, a name or a patient block.

2. **Check the fingerprint.** A template is used when its score on page 1 is 0.8 or more: every `all` word present, and enough `any` words. Pick words that appear on every report from that lab and on no other lab's.

3. **Add a synthetic slip.** Real reports never enter the repo. Add a `Layout` for the lab's layout to `layouts()` in [generate.rs](server/crates/ownhealth-ingest-pdf/tests/support/generate.rs), with a made-up lab name and the Rivera family, and include the template's fingerprint words. The generator writes the PDF and its row-level ground truth.

4. **Test it.**

   ```sh
   cd server
   cargo test -p ownhealth-ingest-pdf
   ```

   [eval.rs](server/crates/ownhealth-ingest-pdf/tests/eval.rs) scores every slip against its ground truth: no row may be dropped, and no wrong row may be accepted without review. Add a test in `server/crates/ownhealth-ingest-pdf/tests/` that imports your slip and asserts the rows were read by your template (`extractor` is `template:<id>`).

## How to add an alias

An alias maps a name as a lab prints it ("Colesterol LDL", "LDL-Cholesterin") to an analyte group (`ldl_c`). The group, together with the unit, specimen and method, picks the LOINC code. A name alone never does.

**For everyone, in the repo.** Add a row to the language file in [data/terminology/aliases/](data/terminology/aliases) (`es.tsv`, `en.tsv`, `de.tsv`, `fr.tsv`). Columns are tab-separated:

```text
alias	alias_group	method	source	reviewed
Colesterol LDL directo	ldl_c	direct	curated	2026-11-08
```

- `alias`: the name exactly as printed, accents included.
- `alias_group`: an existing group from [groups.tsv](data/terminology/groups.tsv). Never reuse a group for a different analyte.
- `method`: only when the printed name says it (`direct`, `calculated`); blank otherwise.
- `source`: `curated`. `reviewed`: today's date.

**A new analyte** also needs a row in `groups.tsv` (canonical unit, SI and conventional units, plausibility limits, English and Spanish names) and its LOINC codes in [loinc/codes.tsv](data/terminology/loinc/codes.tsv). Then rebuild the shipped LOINC subset, which needs network access:

```sh
python3 data/terminology/loinc/build_subset.py   # standard library only
```

Mass-to-molar conversion needs a cited row in [molar/molar_mass.tsv](data/terminology/molar/molar_mass.tsv).

Then run `cargo test -p ownhealth-normalize` from `server/`. [pure.rs](server/crates/ownhealth-normalize/tests/pure.rs) checks that every alias and every code points at a known group, and that each language covers the core panel. Do not bump a version by hand: the derivation version is a hash of these files, and every stored value records the version it was derived with.

**Just for your household.** In the review queue, "Add alias" on an unresolved row saves the alias for your household only and re-links the rows it matches. If it would help everyone, open a pull request with the row as above.

## Reporting a security problem

Do not open a public issue. See [SECURITY.md](SECURITY.md).
