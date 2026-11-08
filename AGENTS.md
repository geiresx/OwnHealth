# AGENTS.md

OwnHealth is a self-hosted personal health record: a web app that keeps a person's (and a family's) labs, records, wearables, genome and imaging in one place, and an MCP server that lets an outside agent read it. Pre-alpha: v0.1 is being built; the plan lives in `docs/`, and [server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md) maps the crates and their interfaces.

## Rules for agents

- The app has no built-in chat agent. The MCP server is the AI interface; keep it that way.
- Never commit real health data, names, genomes or lab reports. Fixtures are synthetic: Alex, Sam and Lucía Rivera and Carmen Ortega, and the fictional lab LabNorte. No real lab brands.
- Every stored value keeps its provenance: source document, page and position.
- Dark theme by default; Spanish and English from the first screen.
- `docs/mockup/index.html` is the visual spec for the UI; the real UI must match it (palette, type, cards, chips, sparklines, layout, copy, es/en).
- Information, not diagnosis: no imperative medical advice. Status words are OK, Watch, Out, Stale and No data.
- Free text sent over MCP is wrapped as `untrusted_text`.
- Database changes are new numbered SQL files in `migrations/`. Never edit an applied migration.
- Conventional commits. Every PR runs CI and is never opened as a draft.
- License: AGPL-3.0-or-later. Every package manifest uses that SPDX id.

## Dev commands

Server (Rust), from `server/`:

```sh
../scripts/test-db.sh                 # shared test Postgres 17 on 127.0.0.1:55432 (idempotent)
bash ../scripts/fetch-pdfium.sh       # the pinned PDFium for this machine, once
mkdir -p static                       # the UI build is embedded only in release builds
cargo test -p <crate>                 # one crate; each test creates and drops its own database
cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p ownhealth -- migrate     # apply migrations to OWNHEALTH_DATABASE_URL
cargo run -p ownhealth -- demo-seed   # seed the synthetic Rivera family (--force to add another)
OWNHEALTH_DEV_AUTH=1 OWNHEALTH_DEMO=1 cargo run -p ownhealth -- serve   # http://127.0.0.1:8080, MCP at /mcp
```

The backup tests need pg_dump and pg_restore 17 and restic. Without a local PostgreSQL client, set
`OWNHEALTH_PG_BINDIR=$PWD/../scripts/pg-bin` to run them inside the test container. The Python MCP
client contract (`ownhealth-mcp/tests/python_client.rs`) needs `uv` on PATH.

Contract suite, from the repo root: `bash contract-tests/run.sh rust`. Its goldens were recorded from 0.1.1.

The crate map is in [server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md). Migrations live in `migrations/`, the
shipped reference files (LOINC subset, aliases, rule pack, CPIC snapshot, vendor templates, genome lists) in `data/`.

UI (SvelteKit, pnpm), from `ui/`:

```sh
pnpm install
pnpm dev                        # proxies /api and /mcp to OWNHEALTH_BACKEND (default 127.0.0.1:8080)
pnpm run check                  # svelte-check, warnings fail
pnpm build                      # writes server/static, which the release binary embeds
```

Container: `cp .env.example .env`, fill it in, `docker compose up -d`. The image is `drumsergio/ownhealth`; Postgres is pinned to a full 17.x version.
