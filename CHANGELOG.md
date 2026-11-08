# Changelog

## 0.2.0 (2026-11-08)

The backend is now Rust: one binary with the web UI built in, in place of the Python 3.13 app. Same features, same data. Upgrade in place by changing the image tag to `drumsergio/ownhealth:0.2.0`; there are no new migrations, so a 0.1.1 database is already up to date, and passkeys, passwords, sessions and access tokens keep working. The image went from 586 MB to 348 MB unpacked (129 MB to 79 MB to download, arm64).

- **Same answers.** The REST API, the MCP tools, their errors and scopes, the cookies, the blob layout and the `state_hash` of every profile are the ones 0.1.1 gave. A black-box suite of 79 tests with answers recorded from 0.1.1 passes on both releases.
- **Stack.** axum and tokio, sqlx for Postgres, rmcp (the official Rust MCP SDK), PDFium through pdfium-render for lab PDFs, noodles for the VCF and its tabix index, webauthn-rs for passkeys.
- **Re-imports.** A lab PDF read by 0.2.0 records `pdfium` as its extraction engine, so forcing a re-import of a document 0.1.1 read adds a new extraction run. Its rows are the same.
- **Uploads.** A form field other than a file is refused past 1 MiB, and a form past 1000 fields or 1000 files, as 0.1.1 did.
- **Logs.** One line per request with method, route, status and latency, never the query string. Upload leftovers in `/data/blobs/tmp` that a killed process left behind are removed after an hour.
- **Homelab backups.** The restic repository and its S3 keys move from `.env` to `restic.env`, which only the `admin` service reads, so the app container no longer holds the keys to the backups. An install that keeps them in `.env` still works; move the lines to take them away from the app ([deploy/README.md](deploy/README.md#homelab-setup)).

## 0.1.1 (2026-11-08)

Fixes from a full CodeRabbit review of 0.1.0, plus places where the code disagreed with the design docs. No migrations; an existing 0.1.0 install upgrades in place.

- **Security.** Adding a passkey or linking an OIDC provider needs a fresh login; the OIDC callback only counts in the browser that started it; a caregiver can no longer take over a managed profile with a claim invite; document search no longer matches or quotes sensitive pages without `read:sensitive`; document bytes are sent `no-store`; a fact can no longer create a source on another profile.
- **Genome.** Lookups no longer reveal that a withheld finding sits at a position; minors are also shielded from carrier-panel genes; the importer recomputes fact ids, caps confidence per assay, checks imputed sites and keeps sensitive facts inside their gene spans; drug guidance comes from the newest CPIC version.
- **Drug check and status.** CYP2D6 phenoconversion bands follow CPIC's open bounds, so no activity score is left without a phenotype; medication matching uses whole words; `get_health_status` stays under 2 KB with long notes; `check_drug` accepts its own paging cursor.
- **Data.** Duplicate detection considers method, comparator and source; the received UCUM unit is kept like the received LOINC code; trend statistics never mix units.
- **Robustness and UI.** Oversized limits and windows answer 422 instead of 500; a failed export leaves no file behind; browser reads are audited as `ui`; a new relative's profile starts with a guardian (profiles created on 0.1.0 may still have none); UI fixes across the review queue, genome, settings and charts.

## 0.1.0 (2026-11-08)

The first release you can run: `docker compose up` with one app image and Postgres 17.

- **Imports.** Lab PDFs with a text layer (rows read with page and position, coded to LOINC, units converted, checked against the printed range, auto-accepted only when every check passes, else the review queue). Apple Health `export.zip`, FHIR R4 and IPS bundles, manual entry, and OwnGenome `genome-facts.json` with its VCF. Scans and photos wait as pending documents for an agent to read over MCP.
- **Status.** One card per body system (OK, Watch, Out, Stale, No data) from a cited YAML rule pack, with a reproducible `state_hash`. Drug check with CPIC level A quotes, current medications, relevant labs and phenoconversion.
- **MCP** at `/mcp`: `get_health_status`, `search_observations`, `get_trend`, `get_genome_findings`, `lookup_variant`, `check_drug`, `get_source`, `propose`. Hashed scoped PATs, scope-filtered `tools/list`, `untrusted_text` on free text, agent writes only into the review queue, rate limits, audit log.
- **Security.** Passkeys with recovery codes, step-up, household roles, full export (FHIR NDJSON plus originals) from the UI only.
- **UI.** Dark theme, Spanish and English, built to match the mockup.

Known limits: no curated rsID index yet, so rsID lookups outside your own facts file answer `rsid_unknown`; lab coverage is the synthetic eval set; no OCR (v0.2). Measured exit criteria: [docs/v0.1-exit-criteria.md](docs/v0.1-exit-criteria.md).
