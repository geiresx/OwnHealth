# CONTRACTS.md

The behaviour OwnHealth keeps from one release to the next: the REST routes, the migrations, the MCP
tools and the UI rules. 0.2.0 moved the server from Python to Rust without changing any of it. The crate
map, the Rust names of the shared interfaces and the formats that must stay byte for byte (hashes,
canonical JSON, cookies, blob layout) are in [server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md). The
design behind each item lives in `docs/design/`; this file only fixes names, owners and shapes. Change
a shape here only in the same commit that changes the code, and say so in the PR.

Sections 1 to 4 and 6 (lanes, file ownership, dependency rules, service interfaces, demo hooks) are in
[server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md) sections 1 to 4 and 6. The numbering below stays
so older references keep pointing at the right section.

## 5. REST API

All routes are under `/api`, JSON, and resolve `{p}` as a profile slug, `me` or a uuid through
`core::access::require`. Errors are `{"detail": {"code": ..., "message": ..., ...}}`: 401 `unauthenticated`,
403 `forbidden` with the missing scope or action, 404 `profile_not_granted` (with `allowed` slugs) or
`unknown`, 409 for lifecycle and review conflicts, 415 `unsupported_upload`. A router module is
`server/crates/ownhealth-api/src/routes/<file>.rs` with a `router()` that `routes.rs` merges.

| Owner | File (`routes/`) | Route | Notes |
|---|---|---|---|
| foundation | core.rs | `GET /me` | user, household, locale, dev flag |
| foundation | core.rs | `GET /profiles` | `ProfileSummary[]` the caller can read, with role and scopes |
| foundation | core.rs | `GET /sources/{src_id}` | mcp.md 5.7 shape; `unknown` 404 when not visible |
| foundation | review.rs | `GET /review?status=&profile=&kind=&document_id=` | |
| foundation | review.rs | `GET /review/{id}` ; `POST /review/{id}/accept` `{edits?}` ; `/reject` `{reason}` ; `/reopen` ; `/dismiss` `{reason?}` | action `review` |
| foundation | review.rs | `POST /review/bulk-accept` `{document_id, min_row_conf}` | only items with no reasons |
| foundation | imports.rs | `POST /imports` multipart `profile`, `files[]` | returns `{items: ImportItem[]}`; action `import` |
| foundation | imports.rs | `GET /jobs?ids=1,2` | |
| — | app.rs | `GET /healthz`, `GET /readyz` (outside `/api`) | |
| A | observations.rs | `GET /profiles/{p}/observations?q=&loinc=&alias_group=&category=&from=&to=&units=&lang=` | `{items: ObservationOut[]}` |
| A | observations.rs | `GET /profiles/{p}/trend?code=&window=P3Y&units=&lang=` | `Trend` (mcp.md 5.3 shape) |
| A | observations.rs | `POST /aliases` | `{profile, alias, alias_group, lang?, loinc?, unit?, specimen?}`, action `review`; re-derives the household |
| B | documents.rs | `GET /profiles/{p}/documents?q=&type=` ; `GET /documents/{id}` | |
| B | documents.rs | `GET /documents/{id}/file` ; `/pages/{n}.png` ; `/pages/{n}/words` | words need `read:documents_text` for tokens |
| B | documents.rs | `POST /documents/{id}/assign` `{profile}` ; `POST /documents/{id}/retract` `{reason}` | |
| C | records.rs | `GET`/`POST /profiles/{p}/records/{kind}` ; `POST .../{kind}/{id}/correct` ; `.../{id}/retract` | |
| C | records.rs | `POST /profiles/{p}/manual-observations` | |
| C | wearables.rs | `GET /profiles/{p}/wearables/summary` ; `GET /profiles/{p}/wearables/{metric}?window=` | |
| D | genome.rs | `GET /profiles/{p}/genome` ; `GET /profiles/{p}/genome/variant?rsid=` ; `POST /profiles/{p}/genome/consent` | consent needs step-up |
| E | status.rs | `GET /profiles/{p}/status?as_of=` ; `GET /profiles/{p}/dashboard?lang=&as_of=` ; `GET /rules` | |
| E | drug_check.rs | `GET /profiles/{p}/drug-check?drug=&lang=` | |
| F | mcp_usage.rs | `GET /tokens/{id}/usage` | `/.well-known/oauth-protected-resource` lands in v0.2 |
| G | auth.rs | passkey register/login, OIDC, `POST /auth/logout`, `POST /auth/step-up` | |
| G | tokens.rs | `GET /tokens` ; `POST /tokens` (returns plaintext once) ; `DELETE /tokens/{id}` | |
| G | audit.rs | `GET /audit?profile=&limit=&before=` | |
| G | family.rs | `GET /household` ; `PUT /grants` ; `DELETE /grants?user_id=&profile=` ; `PUT /profiles/{p}/locks` ; `PUT /profiles/{p}/sensitive` ; `POST /household/profiles` ; `POST /household/invites` ; `DELETE /profiles/{p}?confirm=<slug>` ; `DELETE /account?confirm=<name>&delete_profile=` | |
| G | export.rs | `POST /profiles/{p}/export` `{fmt}` | step-up; answers the zip itself |
| G | auth.rs | `/auth/state`, `/auth/setup/{begin,finish,password}`, `/auth/login/{begin,finish}`, `/auth/recovery`, `/auth/password/login`, `/auth/step-up/{begin,finish}`, `/auth/passkeys` (GET, `/begin`, `/finish`, `DELETE /{id}`), `/auth/recovery-codes`, `/auth/invite/{code}`, `/auth/enroll/{begin,finish,password}`, `/auth/oidc/{login,callback}` | 403 `step_up_required`, 403 `csrf_failed` |
| H | timeline.rs | `GET /profiles/{p}/timeline?types=&from=&to=&lang=` | |
| J | app-level | `GET /metrics` (Prometheus, outside `/api`) | |

The TypeScript shapes for every route are in `ui/src/lib/api/types.ts`; the client functions are in
`ui/src/lib/api/{core,health,manage}.ts`. A server change that alters a response shape updates the type in
the same PR.

## 7. Migrations

Plain numbered SQL in `migrations/NNNN_name.sql` (embedded in the binary), applied in number order,
forward-only, each file once, its checksum recorded in `schema_migrations`. An edited applied file or
an applied version the code does not know stops startup. Each lane numbers inside its range so two lanes
never collide:

| Range | Owner |
|---|---|
| 0001-0099 | foundation (0001_foundation.sql: the whole v0.1 schema) |
| 0100-0149 | A |
| 0150-0199 | B |
| 0200-0299 | C |
| 0300-0399 | D |
| 0400-0499 | E |
| 0500-0599 | F |
| 0600-0699 | G |
| 0700-0799 | J |
| 1000-1079 | v0.2 Inboxes: foundation 1000-1009, then lanes A-G in steps of ten (server/RUST-CONTRACTS.md 10.3) |

H and I have no migrations. Every new phase numbers after the highest shipped file: the migrator applies
missing files in number order, so a lower number would run before files it depends on. Prefer using the 0001 tables; add columns only when the design needs them.
Lifecycle rule for every clinical table: only `status`, `status_reason` and `superseded_at` change after
insert (a trigger enforces it), and `audit_event` is append-only.

## 8. MCP tools

The server is `ownhealth_mcp::router(rt)` (rmcp), streamable HTTP at `/mcp`, stateless, JSON
responses; protocol 2026-07-28 with 2025-11-25 accepted. Full schemas and examples: `docs/design/mcp.md` section 5; the table below is the contract
summary every lane builds against.

| Tool | Scope | Input (beyond `profile`) | Output `data` | Budget | Backed by |
|---|---|---|---|---|---|
| `get_health_status` | `read:summary` (+ domain scopes widen sections) | `as_of` | `rules_version`, `state_hash`, `flags[]`, `medications[]`, `supplements[]`, `active_conditions[]`, `never_measured[]`, `stale[]`, `pending[]`, `corrections[]`, `notes[]`, `data_freshness{}` | < 2 KB | E `build_state` + `render` |
| `search_observations` | `read:labs` / `read:vitals` / `read:wearables` by category | `query` or `loinc`, `category`, `from`, `to`, `flagged_only`, `group_by`, `detail`, `cursor`, `as_of` | `results[]` `{name, code, value, unit, canonical, range, flag, date, lab, draw_id, src}`, `resolved` | 4k, max 15k | core `list_observations`, A |
| `get_trend` | as above | `code`, `window` (`P\d+[DMY]`/`all`), `resolution`, `as_of` | `name, codes, canonical_unit, resolution, coverage_pct, points[], stats, annotations[]` | 2k, max 6k | A, C rollups |
| `get_genome_findings` | `read:genome` (+ `read:sensitive` and the agents switch for sensitive) | `category`, `gene`, `min_evidence`, `detail`, `cursor`, `as_of` | `assay`, `findings[]` (each with `call_state`) ; `not_assessed[]` ; `db_versions` | 4k, max 15k | D `list_findings` |
| `lookup_variant` | `read:genome_raw` | one of `rsid`, `hgvs`, `region`; `cursor` | `query`, `variant`, `calls[]` (`call_state` always), `annotations`, `cited_by_findings`; `quota` | ~500 per position | D `lookup_variant` |
| `check_drug` | any of `read:genome`, `read:records`, `read:labs` (missing parts in `withheld.by_scope`) | `drug` | `drug`, `pgx[]`, `phenoconversion[]`, `current_medications_relevant[]`, `relevant_labs[]`, `guidance[]`, `not_assessed[]` | 2k, max 4k | E `check_drug`, D `pgx_for_drug` |
| `get_source` | scope of the underlying fact; text needs `read:documents_text` | `src_id`, `include_text` | `src_id, status, document, location, printed, derived, extraction, crop_url, text` | 800, 4k with text | core `resolve_src`, B `page_words` |
| `propose` | `write:propose` + read on the profile | `kind` (`observations`, `note`, `finding_status`, `medication`), `payload`, `based_on[]` | `proposal_id, status: queued_for_review, kind, rows[]/..., review_url` | 1k | core `create_item` (kind `agent_proposal`), A, B `ground_value` |

Rules every tool follows (mcp.md 3, 9):

- Envelope fields: `profile, as_of, generated_at, lang, identity_mode, data, evidence, limitations,
  not_assessed, withheld{sensitive, by_scope, minor_rule}, db_versions, next_cursor,
  budget{tokens_est, truncated}, notice`, plus a compact markdown text block with the same facts.
- `tools/list` shows only tools whose scope the token holds. All tools: `openWorldHint: false`; all but
  `propose` are `readOnlyHint: true, idempotentHint: true`.
- Every fact carries `src`. Ids are stable and resolve through `get_source`.
- Free text is `{"trust": "untrusted_text", "origin": <pdf_text_layer|ocr|email_subject|fhir_note|
  agent_note|person_note>, "text": ...}` via `core::text::untrusted`.
- Tool errors are `isError: true` with `code`, `message`, `fix` (`profile_not_granted`, `scope_missing`,
  `rate_limited`, `egress_budget_exhausted`, `genome_lookup_cap`, `propose_cap`, `bad_input`).
- Limits per token (mcp.md 9.4): 60 calls/min burst 20; 2 000 facts and 2 MB per day; 200 lookups per
  day; 50 proposals per day, 200 rows per call. Counters live in Postgres.
- Every call writes one `audit_event` (`via = "mcp"`, `tool`, codes and ids only, free-text args
  hashed with `hash_arg`).
- `propose` never auto-accepts; failed rows are queued and marked, never dropped.

Resources (`cacheScope: private`, audited like tools): `ownhealth://profiles`,
`ownhealth://profile/{id}/status`, `ownhealth://profile/{id}/status.md`, `ownhealth://source/{src_id}`,
`ownhealth://rules/{version}`, `ownhealth://profile/{id}/access-log` (owner or guardian).
Prompts: `prepare-doctor-visit`, `check-before-new-drug`, `what-to-measure-next`.

## 9. UI

`docs/mockup/index.html` is the visual spec: the palette, type, cards, chips, sparklines, layout and
copy are ported into `ui/src/lib/styles/app.css` and the shared components. Pages use those classes and
components; a page that needs a new shared look adds it to a component, not a one-off style.

- Shared components (`#lib/components`): `Card`, `StatusChip` (status from the API, never computed),
  `PhaseChip`, `SourceChip` (every value shows its src id and opens `GET /sources/{id}`), `Sparkline`,
  `TrendChart` (hand-written SVG), `PageHeader`, `StubPage`.
- i18n: `t(key)` from `#lib/i18n/index.svelte`, es and en, every key in both (`_esComplete` check).
- State: `app.profile` (current slug) from `#lib/state/app.svelte`; pages read it and refetch.
- Imports use `#lib/...` (SvelteKit 3 subpath imports, `package.json` `imports`).
- `pnpm run check` (svelte-check, warnings fail) and `pnpm build` (into `server/static`)
  must pass.
