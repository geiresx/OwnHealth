# RUST-CONTRACTS.md

The working agreement between the lanes of the Rust port (OwnHealth 0.2.0). The Python backend 0.1.1
(tag `v0.1.1`; removed from the tree in 0.2.0) is the reference: the Rust server behaves the same from
the outside. Same Postgres
schema and migrations, same environment variables, REST paths and JSON, cookies, MCP tools, schemas,
scopes, envelopes and error codes, the same hashes for tokens, passwords, recovery codes and sessions,
the same blob layout, job kinds, `state_hash` bytes and audit rows. [CONTRACTS.md](../CONTRACTS.md)
holds the behaviour; this file maps it onto crates and fixes the Rust names.

The foundation shipped the shared core and every interface another lane calls as a typed stub; the
lanes filled them in and the integration wired them together, so no stub and no `NotImplemented` error is
left. No `todo!()` and no panics in request paths. Change a signature here only
in the same commit that changes the code, and say so in the PR.

## 1. Lanes and crates

| Lane | Crate | Area | Python it ports |
|---|---|---|---|
| foundation | `ownhealth-core` | config, migrations, db, blobs, jobs, audit, ids, canonical JSON, access, fact store, review queue, documents, demo family | `config.py`, `runtime.py`, `blobs.py`, `db/`, `jobs/__init__.py`, `core/`, `state/canonical.py`, `ingest/__init__.py`, `demo/__init__.py`, `demo/family.py`, `demo/pdf.py`, `mcp/envelope.untrusted` |
| A | `ownhealth-normalize` | value parsing, analyte resolution, UCUM, conversions, ranges, flags, aliases, rederive | `normalize/`, `jobs/kinds/normalize.py` |
| B | `ownhealth-ingest-pdf` | lab PDFs: text layer (pdfium), tables, vendor templates, page renders, grounding | `ingest/pdf/`, `ingest/classify.py`, `jobs/kinds/pdf.py` |
| C | `ownhealth-ingest-structured` | FHIR/IPS bundles, Apple Health, manual entry, retraction, wearables | `ingest/fhir/`, `ingest/apple_health/`, `ingest/manual.py`, `ingest/retract.py`, `wearables/`, `jobs/kinds/{fhir,apple_health}.py`, `demo/hooks/apple_fhir.py` |
| D | `ownhealth-genome` | genome-facts import, findings, gating, tabix lookups (noodles) | `genome/`, `jobs/kinds/genome.py`, `demo/hooks/genome.py` |
| E | `ownhealth-rules` | rule pack, state body, render, status words, dashboard, drug check | `state/`, `rules/`, `jobs/kinds/state.py` |
| F | `ownhealth-mcp` | `/mcp`: the 8 tools, PAT guard, envelope, budgets, limits, resources, prompts | `mcp/` |
| G | `ownhealth-auth` | passkeys, OIDC, passwords, recovery, sessions, step-up, PATs, household management | `auth/` |
| H | `ownhealth-ops` | backup, restore, verify-restore, the profile export; the base demo seed | `ops/`, `export/`, `demo/base.py` |
| I | `ownhealth-api` | `/api`: error mapping, extractors, the foundation routes, timeline | `api/_deps.py`, `api/core.py`, `api/review.py`, `api/imports.py`, `api/timeline.py`, `auth/deps.py` |
| J | `contract-tests/` | black-box HTTP and MCP tests both backends must pass | `tests/` (as behaviour, not code) |
| foundation | `ownhealth` (bin) | CLI, app assembly, UI serving, probes, startup and shutdown | `cli.py`, `app.py` |

Dependency direction (no cycles): `core` <- `normalize` <- `ingest-pdf`, `ingest-structured`, `rules`
(`rules` also uses `genome`); `auth` uses only `core`; `mcp` uses `auth`, `genome`, `ingest-pdf`,
`ingest-structured`, `normalize`, `rules`; `api` uses every lane crate (`mcp` for `mcp.gate` and
`mcp.limits`, as `api/core.py` and `api/mcp_usage.py` did); the binary uses all. `ingest-pdf` calls
lane C's `retract_document_facts` directly (`ingest-structured` depends only on `core` and `normalize`). Python's `random.Random` is `core::pyrandom`, shared by the demo seed and the
synthetic fixtures. A lane that needs
something from a crate it may not depend on asks the foundation to move it into `core` (as was done
with `fhir::received_ucum`/`loinc_of`, `text::untrusted`, `ingest::ImportSummary` and the demo family).

## 2. File ownership

A lane edits only what it owns. **foundation** files are shared: change them in a small separate commit
that touches nothing else, and keep existing signatures working (additive changes are fine).

```text
server/
  Cargo.toml, Cargo.lock, rustfmt.toml    foundation. Every dependency a lane needs is already declared
                                          and resolved (in [workspace.dependencies] and in the lane's own
                                          Cargo.toml) so parallel lanes never fight over Cargo.lock.
                                          Adding one: one line, sorted, and say so in the PR.
  crates/ownhealth-core/                  foundation
  crates/ownhealth-normalize/             A
  crates/ownhealth-ingest-pdf/            B   (examples/spike_pdfium.rs stays as the spike)
  crates/ownhealth-ingest-structured/     C
  crates/ownhealth-genome/                D
  crates/ownhealth-rules/                 E
  crates/ownhealth-mcp/                   F   (examples/spike_rmcp.rs stays as the spike)
  crates/ownhealth-auth/                  G   (csrf.rs and principal.rs are foundation: extend, keep signatures)
  crates/ownhealth-ops/                   H
  crates/ownhealth-api/src/lib.rs, error.rs, extract.rs, routes.rs   I (foundation-grade: keep signatures)
  crates/ownhealth-api/src/routes/<area>.rs   the lane of the Python router file (section 5)
  crates/ownhealth-api/tests/<area>*.rs   that area's lane
  crates/ownhealth/                       foundation (H may add CLI wiring for its own commands)
  static/                                 build output of ui/ (pnpm build), never edited, not committed
  vendor/pdfium/                          scripts/fetch-pdfium.sh output, not committed
  SPIKES.md, RUST-CONTRACTS.md            foundation
contract-tests/                           J (run.sh is foundation-grade: keep its arguments)
scripts/fetch-pdfium.sh, scripts/test-db.sh   foundation
migrations/                               numbered SQL, ranges as in CONTRACTS.md section 7
data/                                     the shipped reference files below
```

Data files are the ones 0.1.1 shipped, byte for byte, moved out of the Python tree in 0.2.0. They are
embedded at build time with `rust-embed` or `include_str!` relative to the crate, as `core::migrate`
embeds the migrations:

| Files | Lane |
|---|---|
| `migrations/*.sql` | all (embedded by `core::migrate`) |
| `data/terminology/` (LOINC subset, aliases, molar masses) | A |
| `data/vendors/<country>/<provider>.yaml` | B |
| `data/rules/pack/*.yaml`, `data/rules/cpic/snapshot.json` | E |
| `data/genome/` (`sensitive_genes.tsv`, `carrier_genes.tsv`), `data/genome-facts/genome-facts.v1.json` | D |
| `fixtures/<lane>/` (synthetic only) | each lane's tests |

## 3. Rules every lane follows

The Python rules (CONTRACTS.md section 3) hold, with these Rust names:

- Every fact enters the store through `core::review::route_fact` / `submit_candidate` (imports) or
  `core::observations::commit_observation` / `core::records::add_record` with `accepted_by = "person"`.
  No lane writes `observation` or the record tables with raw SQL.
- Every fact has provenance: `core::provenance::SrcIn` / `SrcRef`.
- Every read a person or agent makes writes one `core::audit::write_audit` row in the same transaction.
- Every permission check goes through `core::access::require` / `effective_permission`.
- Status words come only from lane E. Free text going out over MCP is `core::text::untrusted(text, origin)`.
- Domain functions take `&mut sqlx::PgConnection` (a `Transaction` derefs to it); the caller owns the
  transaction. Routes open one per request and commit before answering.
- Errors: `ownhealth_core::Error` (thiserror). `anyhow` only in binaries, tests and examples. No
  `unwrap`/`expect` outside tests. `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt`.
- Synthetic data only: the Rivera family, Carmen Ortega, LabNorte. Information, never diagnosis.

## 4. Interfaces

### 4.1 Foundation (implemented, tested)

```rust
// config
Settings::from_env() -> Result<Settings>            // OWNHEALTH_* and the *_FILE twins; Debug hides secrets
Settings::from(&dyn Env) -> Result<Settings>        // tests pass a HashMap
settings.blobs_path(), settings.sqlx_url()          // DEV_SECRET_KEY, QUEUES
// runtime: the axum state of every route, cloned into MCP and the workers
Runtime { settings: Arc<Settings>, pool: PgPool, blobs: BlobStore }
Runtime::new(settings) / Runtime::with_pool(settings, pool) / rt.job_context()
// migrate: forward-only, checksum-locked, Python's schema_migrations bookkeeping and advisory lock
migrate::embedded() -> Vec<Migration>; migrate(pool, &[Migration]) -> Vec<i32>; migrate_embedded(pool)
migrate::pending(conn, &[Migration]) -> Vec<i32>
// blobs: blobs/sha256/ab/cd/<hash>, mode 0440, tmp + rename
BlobStore::new(root) .put_bytes(&[u8]) -> String .put_reader / .put_async / .put_file -> (String, u64)
        .open / .get_bytes / .path / .exists / .size / .delete
// canonical: byte-identical to Python (tests/parity_fixtures.rs checks a Python-made fixture)
canonical::canonical_json(&Value) -> Result<Vec<u8>>   // floats refused
canonical::state_hash(&Value) -> Result<String>        // "sha256:<hex>" of the body without state_hash
// pyjson: Python's printing of Decimal, float4, datetimes; row_to_json for SELECT * rows
// ids
ids::new_src_id(), ids::is_src_id(s), ids::token_hex(n), ids::short(n)
// access
require(conn, &Principal, ProfileRef, scope: Option<&str>, action: Option<&str>) -> Result<Permission>
effective_permission(conn, &Principal, profile_id) -> Result<Permission>   // .scopes .role .has() .can()
accessible_profiles(conn, &Principal) -> Result<Vec<Permission>>
resolve_profile_ref(conn, &Principal, ProfileRef) -> Result<Uuid>          // Id | Name(slug or "me") | Default
effective_scopes(token, categories, locks, role) -> Scopes
create_household / create_user / create_profile(&ProfileIn) / set_grant / revoke_grant / list_grants
set_locks / set_genome_switches / age_of_majority / get_profile / get_profile_by_slug
Error::AccessDenied { code, message, allowed }    // 404 profile_not_granted, else 403
// provenance
SrcIn::new(kind) / SrcIn::manual(user) / .validate(); SrcRef::{Id(String), New(SrcIn)}
create_src, ensure_src(conn, &SrcRef, profile_id), resolve_src(conn, id) -> Option<ResolvedSrc>, region_label
// observations
commit_observation(conn, &ObservationIn, &SrcRef, CommitOptions { accepted_by, status, reviewed_by, supersedes })
supersede_observation(conn, old, &new, &src, accepted_by, reason, reviewed_by) -> Committed { id, src_id }
retract_observation / confirm_observation / get_observation / observation_history
list_observations(conn, pid, &ObservationQuery) / latest_per_group(conn, pid, as_of, category)
ObservationIn::from_json(Value)   // pydantic's validation; Observation serializes as FastAPI did
// records (condition | medication | allergy | immunization | procedure)
RecordIn::from_json(kind, Value); add_record(conn, &RecordIn, &SrcRef, RecordOptions)
supersede_record / retract_record / get_record / record_history / list_records / active_medications
// acceptance
auto_accept(row_conf, &[Check], allowance, identity, threshold) -> Decision { auto, reasons, why_not }
Check::pass(name) / Check::fail(name, reason); REASON_CODES, SOURCE_ALLOWANCES
// review
route_fact(conn, RouteFact::new(&obs, &src, &checks, row_conf, allowance, identity) { threshold, provisional, ... })
create_extraction_run(conn, document_id, extractor, version, config_hash, engine, engine_version) -> Uuid
submit_candidate(conn, run_id, &CandidateIn, allowance, identity, threshold, provisional) -> RouteResult
create_item(conn, &ReviewItemIn) -> Uuid; get_item; list_items(conn, &ItemQuery); count_open
accept(conn, item, user, edits) -> Vec<Uuid>; reject(reason); reopen; dismiss; bulk_accept
register_accept_handler(kind, AcceptHandler)       // section 6
// documents
sniff(filename, head, blob_path) -> &'static str; create_document(conn, &NewDocument) -> DocumentRef
dispatch_upload(conn, &BlobStore, household, profile, &[StoredFile], actor) -> Value   // {"items": [...]}
sensitive_document_sql(doc) -> String; document_is_sensitive(conn, id) -> bool
// audit
write_audit(conn, &AuditEvent) -> i64; hash_arg(value) -> "h:<12 hex>"; list_audit(conn, profiles, actor, limit, before)
// jobs
enqueue(conn, kind, &payload, EnqueueOptions { queue, dedupe_key, priority, run_after, max_attempts }) -> Option<i64>
register_handler(kind, Handler); claim / complete / fail / reap_expired / run_one / drain / start_workers
JobError::retry(msg) / ::permanent(msg) / ::typed(type_name, msg)    // last_error = "<Type>: <msg>"[:500]
// ingest, text, demo
ImportSummary { document_id, accepted, queued, skipped, reasons }; .count(&RouteResult)
text::untrusted(Option<&str>, origin) -> Value      // final shape: {"trust": "untrusted_text", origin, text}
demo::create_rivera_family(conn, name) -> Family; demo::seed_demo(pool, settings, blobs, force) -> bool
demo_pdf::lab_report_pdf(&LabReport) -> (Vec<u8>, Vec<Vec<BBox>>)   // byte-identical to demo/pdf.py
testing::TestDb::new() / ::empty() / .settings(dir)  // feature "testing": a throwaway migrated database
```

`ownhealth-auth` (foundation parts, implemented): `principal::resolve(conn, settings, authorization,
session_secret)` (Bearer PAT, then the `oh_session` cookie, then dev auth), `principal::dev_principal`,
`csrf::check(method, headers, principal, base_url)` and `csrf::check_origin`.

`ownhealth-api` (implemented): `ApiError` (`{"detail": {"code", "message", ...}}`, `From<core::Error>`
as in section 5), `Caller` (the extractor `get_principal` was: resolve, refuse PATs with 403
`forbidden`, CSRF), `SessionCaller`, `caller.require(conn, p, scope, action)`.

### 4.2 Lane interfaces

Each crate's `src/lib.rs` documents them. The shapes:

| Crate | Function (Rust) | Called by |
|---|---|---|
| normalize (A) | `parse_value(printed, locale, unit) -> Result<ParsedValue>` | B, C, F |
| | `resolve_analyte(conn: Option<&mut PgConnection>, name, &ResolveQuery) -> Result<Resolution>` | B, C, F |
| | `to_ucum(unit) -> Option<String>`; `canonical_unit(group)`; `group_units(group)`; `display_name(group, lang)` | B, C, E, I |
| | `convert(value, from, to, group) -> Result<Option<Decimal>>` | E, I |
| | `parse_range(printed, locale, unit) -> Result<Vec<RefRange>>`; `pick_range(&ranges, sex, age)` | B, C |
| | `interpret(value, text_code, range, comparator) -> Option<&str>` | B, C |
| | `normalize_row(conn, &RowIn) -> Result<Normalized { observation, checks }>` | B, C, F |
| | `add_alias(conn, &AliasIn) -> Result<Uuid>`; `rederive(conn, profile, since) -> Result<i64>`; `derivation_version() -> String` | I, H |
| ingest-pdf (B) | `import_pdf(conn, blobs, document_id, force) -> Result<ImportSummary>` | job `import.pdf` |
| | `page_words(conn, blobs, document_id, page_no) -> Result<Vec<Word>>` | F, I |
| | `render_page_png(blobs, sha, page_no, scale) -> Result<Vec<u8>>` (blocking) | I |
| | `ground_value(conn, blobs, document_id, page_no, name, value) -> Result<Option<[f64; 4]>>` | F |
| ingest-structured (C) | `fhir::import_fhir`, `apple_health::import_apple_health` `-> Result<ImportSummary>` | jobs |
| | `apple_health::rebuild_rollups(conn, profile, metric) -> Result<i64>` | H |
| | `manual::add_manual_observation(conn, &ManualObservation) -> Result<Committed>` | I |
| | `wearables::series(conn, profile, metric, since, until, resolution) -> Result<Value>`; `metric_info(key)` | E, I |
| genome (D) | `gate_for(&Permission, surface, on, age_of_majority) -> Result<Gate>` | E, F, I |
| | `validate_facts(&Value) -> Result<Vec<String>>`; `import_genome_facts(conn, blobs, doc, attachments, actor)` | job |
| | `list_findings(conn, profile, &Gate, &FindingsQuery) -> Result<Value>` | E, F, I |
| | `lookup_variant(conn, blobs, profile, &Gate, &VariantQuery) -> Result<Result<Value, GenomeQueryError>>` (inner: a query it cannot answer, `{code, message, fix}`) | F, I |
| | `pgx_for_drug(conn, profile, cpic_drug_id, as_of) -> Result<Vec<Value>>`; `is_sensitive_gene(gene)`; `CALL_STATES`, `EVIDENCE`, `CONFIDENCE` (final) | E, F |
| rules (E) | `rules_version()`; `build_state(conn, profile, as_of, known_at) -> Result<Value>` | F, H, I |
| | `render(&body, lang, units, identity_mode, scopes) -> Result<Value>` | F, I |
| | `status_word(conn, profile, group, as_of) -> Result<&str>`; `dashboard(conn, profile, as_of, lang, &scopes)` | I |
| | `check_drug(conn, profile, drug, &scopes, lang, as_of) -> Result<Value>`; `resolve_drug(conn, name)` | F, I, C |
| mcp (F) | `router(Runtime) -> Router` (`/mcp` behind the PAT guard, and `/.well-known/oauth-protected-resource`) | bin |
| auth (G) | `tokens::{create_token, verify_token, revoke_token}`; `sessions::{resolve_session, create_session, require_step_up}` | foundation, F, I |
| ops (H) | `backup(settings, dest) -> Result<PathBuf>`; `restore` / `verify_restore -> Result<Value>` (the report, `ok` decides the exit code); `export::export_profile(conn, blobs, profile, out: Write + Seek, fmt) -> Result<Value>` | bin, I |

Python keyword arguments become a small options struct with `::new(...)` and `..Default::default()`
when there are more than about four of them (`CommitOptions`, `RouteFact`, `EnqueueOptions`, `RowIn`).
Python dicts that go out as JSON stay `serde_json::Value` (or a `Serialize` struct whose field order is
the Python key order; `serde_json` has `preserve_order` on).

## 5. REST surface (lane per file, as in CONTRACTS.md section 5)

`ownhealth-api/src/routes/<file>.rs` has `pub fn router() -> Router<Runtime>`; `routes::all()` merges
them in module-name order and the app nests the result at `/api`. Unknown `/api/...` paths answer 404
`{"detail": {"code": "not_found", "message": "Not found."}}` from the UI fallback, as in Python.

| File | Lane | File | Lane | File | Lane |
|---|---|---|---|---|---|
| `core.rs` | I | `observations.rs` | A | `genome.rs` | D |
| `review.rs` | I | `documents.rs` | B | `status.rs`, `drug_check.rs` | E |
| `imports.rs` | I | `records.rs`, `wearables.rs` | C | `mcp_usage.rs` | F |
| `timeline.rs` | I | `auth.rs`, `tokens.rs`, `audit.rs`, `family.rs`, `export.rs` | G | | |

Error mapping (`ApiError::from(core::Error)`), the same codes FastAPI answered:

| core::Error | HTTP | detail.code |
|---|---|---|
| `AccessDenied { code: "profile_not_granted", allowed }` | 404 | `profile_not_granted`, with `allowed` |
| `AccessDenied { code, allowed }` | 403 | `code` (`insufficient_scope`, `forbidden`, `step_up_required`, ...), with `allowed` |
| `Review`, `Lifecycle`, `Provenance` | 409 | `conflict` |
| `Upload` | 415 | `unsupported_upload` |
| `Invalid` | 422 | `invalid` (a route that must match FastAPI's 422 body builds it itself) |
| `NotFound` | 404 | `unknown` |
| `NotImplemented` | 501 | `not_implemented` |
| anything else | 500 | `internal` (logged) |
| no caller | 401 | `unauthenticated` |
| CSRF | 403 | `csrf_failed` |

Probes (bin): `GET /healthz` `{"status": "ok", "version": "0.2.0"}`; `GET /readyz` `{"status": "ready"}` or 503
`db_unavailable` / `migrations_pending` / `blob_dir_missing`. The UI is embedded from `server/static`
(`OWNHEALTH_STATIC_DIR` serves a directory instead); unknown non-API paths get `index.html` with
`Cache-Control: no-cache`; without a build, 503 `ui_not_built`.

## 6. Registration: job handlers, accept handlers, demo hooks

Each lane crate has `pub fn register()`; the binary calls all of them at startup
(`ownhealth::register_lanes()`). Registration is by plain function pointers in global registries, so a
lane adds a kind by editing only its own crate.

```rust
// a job kind (the names are the Python ones: jobs::JOB_KINDS)
jobs::register_handler("import.pdf", |ctx, job| Box::pin(import_pdf_job(ctx, job)));
//   async fn import_pdf_job(ctx: JobContext, job: Job) -> Result<Option<Value>, JobError>
//   job.uuid("document_id")? gives a PermanentJobError for a bad payload; `?` on core::Error keeps
//   Python's exception type names in last_error.

// a review kind other than candidate_row
review::register_accept_handler("agent_proposal", |conn, item, user, edits| Box::pin(accept(conn, item, user, edits)));

// demo data: base (H) is order 0; A 100, B 200, C 300, D 400, E 500, F 600, G 700, H 900
demo::register_demo_hook("genome", 400, |conn, ctx| Box::pin(seed(conn, ctx)));
//   ctx.family.profile("alex"), ctx.documents, ctx.src_ids; all hooks run in one transaction
```

Handlers must be idempotent (a job may run twice after a crash). A panic in a handler is recorded as a
failed attempt (`RuntimeError: handler panicked: ...`), never a dead worker.

## 7. Formats that must not change

| What | Format |
|---|---|
| PAT | `oh_pat_<id8>_<secret43>`; id8 from `[A-Za-z0-9]`, secret = base64url(32 random bytes) without padding; `api_token.prefix = "oh_pat_<id8>"`, `secret_sha256 = sha256(secret)` |
| password | `scrypt$16384$8$1$<b64 salt16>$<b64 dk32>` (standard base64), compared in constant time |
| recovery code | 10 codes `XXXX-XXXX-XXXX` from `0123456789ABCDEFGHJKMNPQRSTVWXYZ`; stored `sha256(normalised)` where normalised = upper alnum with O->0, I->1, L->1 |
| invite code | `sha256(code)` |
| session | cookie `oh_session` = `token_urlsafe(32)`, HttpOnly, SameSite=Lax, Secure when the base URL is https; row `secret_sha256 = sha256(secret)`; idle 30 min, absolute 12 h, step-up 300 s |
| passkey row | credential id, raw COSE public key, sign count (SPIKES.md c) |
| blob | `<data_dir>/blobs/sha256/<2>/<2>/<sha256 hex>`, mode 0440 |
| src id | `^src_[a-z0-9_]+$`, `src_` + 8 random chars from the core alphabet |
| state_hash | `sha256:` + hex sha256 of RFC 8785 canonical JSON of the body without `state_hash` |
| job | kinds in `jobs::JOB_KINDS`; backoff 2^attempts x 30 s; lease 300 s, heartbeat 60 s, reaper every 60 s; `last_error` `"<Type>: <message>"` cut at 500 chars; a dead job files one `job_failed` review item |
| audit | one `audit_event` row per read; free-text arguments only as `hash_arg` |
| JSON numbers | Decimal columns print as FastAPI did (`pyjson::decimal_json`: integer when the scale is 0, else float); real (float4) columns with the shortest repr |
| JSON datetimes | as the Python route sent them, field by field: a datetime object returned from a route annotated `-> dict` went through pydantic's JSON mode, `Z` for UTC (`pyjson::pydantic_datetime`, `validate::pydantic_datetimes`); one from an unannotated route, or a string Python built with `isoformat()`, is `+00:00` (`pyjson::py_isoformat`). Micros only when non-zero. The contract goldens record which field is which |

## 8. Tests

- Unit tests next to the code; database tests in `crates/<crate>/tests/` with
  `ownhealth_core::testing::TestDb` (dev-dependency `ownhealth-core` with feature `testing`). Each test
  gets its own database in the shared test Postgres (`bash scripts/test-db.sh`, container
  `ownhealth-testpg` on 127.0.0.1:55432, `OWNHEALTH_TEST_ADMIN_URL` to override) and drops it on exit.
- Port the Python tests of your area into your crate. The core's are in `ownhealth-core/tests/`.
- Run one crate at a time on the shared mini: `cargo test -p <crate>`. Use the shared `server/target`.
- Contract tests: `contract-tests/run.sh rust` starts the server on a fresh database with
  `OWNHEALTH_DEMO=1 OWNHEALTH_DEV_AUTH=1` and runs `cargo test -p ownhealth-contract-tests --
  --include-ignored`. The goldens were recorded from 0.1.1, and 0.1.1 passed the whole suite (79 of 79)
  before the Python tree was removed.

## 9. Known differences from 0.1.1

Each lane's `.lanes/<lane>-requests.md` lists them; the release notes carry the ones a user can see.
The ones that touch stored data: `extraction_run.engine` is `pdfium` (`chromium/8125`), so a forced
re-import of a 0.1.1 document makes a new extraction run, and the PDF Info key `Trapped` is no longer in
`flags.metadata_text`. FastAPI's 422 lists keep their status, `type` and `loc`; a message can differ.
