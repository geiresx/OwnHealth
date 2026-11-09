-- 1000 foundation, v0.2 Inboxes: sources and their secrets, runs, quarantine, sender rules, PDF
-- password rules, the three dedup levels, OCR runs and grounding (ingestion.md 3, 4, 6, 7.2;
-- connectors.md 6; security-privacy.md 4.2, 7.1). Lanes add their own files in their v0.2 ranges
-- (RUST-CONTRACTS.md section 10).

-- ---------------------------------------------------------------- secrets ----
-- Credentials an inbox needs in clear to log in somewhere: an IMAP password, a Paperless API token, a
-- Nightscout token, a PDF password. AES-256-GCM with the key from OWNHEALTH_SECRETS_KEY (core::secrets);
-- the row id and purpose are the associated data, so a ciphertext cannot be moved to another row.
-- Never logged, never in a job payload, an audit row or an error message.
create table secret (
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household on delete cascade,
  purpose text not null check (purpose in ('imap_password', 'paperless_token', 'nightscout_token',
        'pdf_password')),
  label text,                        -- shown in the UI ('DNI de Alex'); never the value
  key_fingerprint bytea not null,    -- first 8 bytes of sha256(key): which key sealed it
  ciphertext bytea not null,         -- version byte, 12-byte nonce, ciphertext and tag
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now());
create index on secret (household_id);

-- -------------------------------------------------------------- sources ----
-- One configured way data arrives without an upload: a mailbox, a watched folder, a Paperless
-- instance, a device push (share target, iOS Shortcut, Health Auto Export), a Gadgetbridge export, a
-- Nightscout site. profile_id is the profile the data belongs to; null when a rule picks it per item
-- (email sender rules, folder subfolders, Paperless tags).
create table source (
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household on delete cascade,
  profile_id uuid references profile,
  kind text not null check (kind in ('email', 'folder', 'paperless', 'share', 'shortcut',
        'health_auto_export', 'gadgetbridge', 'nightscout')),
  label text not null,
  config jsonb not null default '{}',  -- non-secret settings: host, port, user, folder, tag, url
  secret_id uuid references secret on delete set null,
  -- push sources (webhook, share target, Shortcut, Health Auto Export): a bearer token
  -- 'oh_ing_<id8>_<secret43>', stored as PATs are (prefix + sha256 of the secret)
  token_prefix text unique,
  token_sha256 bytea unique,
  enabled boolean not null default true,
  poll_seconds int check (poll_seconds is null or poll_seconds >= 60),
  cursor jsonb,                        -- opaque per kind: IMAP UIDVALIDITY + UID, modified__gt, ...
  last_run_at timestamptz,
  last_success_at timestamptz,
  last_error_at timestamptz,
  last_error text,                     -- type and message, never content or a secret
  consecutive_failures int not null default 0,
  created_by uuid references app_user,
  created_at timestamptz not null default now());
create index on source (household_id, kind);

create table source_run (
  id bigint generated always as identity primary key,
  source_id uuid not null references source on delete cascade,
  trigger text not null check (trigger in ('poll', 'push', 'manual')),
  job_id bigint,
  started_at timestamptz not null default now(),
  finished_at timestamptz,
  status text not null default 'running' check (status in ('running', 'ok', 'partial', 'error')),
  items_seen int not null default 0,
  items_new int not null default 0,
  items_duplicate int not null default 0,
  items_quarantined int not null default 0,
  errors jsonb not null default '[]',  -- [{code, message}] without content
  error text);
create index on source_run (source_id, started_at desc);

-- Every item a source handed us, by the source's own id (Message-ID, Paperless document id, folder
-- path + size + mtime, push batch hash): the per-source dedup in front of the blob hash.
create table source_item (
  source_id uuid not null references source on delete cascade,
  external_id text not null,
  blob_sha256 bytea,
  document_id uuid references document on delete set null,
  quarantine_id uuid,                  -- FK added below
  seen_at timestamptz not null default now(),
  primary key (source_id, external_id));

-- ------------------------------------------------------------ quarantine ----
-- Mail from a sender not on the allowlist (or failing authentication) is stored as a blob and listed;
-- it is never classified, extracted or indexed until a person releases it (security-privacy.md 4.2 #7).
create table quarantine_item (
  id uuid primary key default gen_random_uuid(),
  household_id uuid not null references household on delete cascade,
  source_id uuid references source on delete set null,
  raw_blob bytea not null,             -- sha256 of the whole message (.eml) as received
  message_id text,
  sender text,                         -- the From address; untrusted text
  subject text,                        -- untrusted text, cut at 300 characters
  attachments jsonb not null default '[]',   -- [{filename, mime, size}]: names are untrusted text
  reason text not null check (reason in ('sender_not_allowed', 'auth_failed', 'too_large',
        'unsupported_type', 'no_profile')),
  status text not null default 'held' check (status in ('held', 'released', 'deleted')),
  received_at timestamptz not null default now(),
  decided_by uuid references app_user,
  decided_at timestamptz,
  released_document_ids uuid[]);
create index on quarantine_item (household_id, status, received_at desc);
alter table source_item add foreign key (quarantine_id) references quarantine_item on delete set null;

-- Email allowlist: who may send, and whose profile their mail belongs to (ingestion.md 3.2).
create table sender_rule (
  id uuid primary key default gen_random_uuid(),
  source_id uuid not null references source on delete cascade,
  pattern text not null check (pattern = lower(pattern) and pattern ~ '^([^@\s]+@)?[^@\s]+\.[^@\s]+$'),
                                       -- 'resultados@labnorte.example' or the domain 'labnorte.example'
  profile_id uuid not null references profile,
  doc_type_hint text,
  trust_unauthenticated boolean not null default false,
  created_by uuid references app_user,
  created_at timestamptz not null default now(),
  unique (source_id, pattern));

-- Passwords tried, in order, on a locked PDF from this sender only; never a global list.
create table pdf_password_rule (
  id uuid primary key default gen_random_uuid(),
  sender_rule_id uuid not null references sender_rule on delete cascade,
  secret_id uuid not null references secret on delete cascade,
  position int not null default 0,
  created_at timestamptz not null default now());
create index on pdf_password_rule (sender_rule_id, position);

-- ---------------------------------------------------------- documents ----
alter table document
  add column source_id uuid references source on delete set null,
  add column derived_from uuid references document,     -- the unlocked copy of a password PDF
  add column near_duplicate_of uuid references document,
  add column ocr_state text not null default 'not_needed'
    check (ocr_state in ('not_needed', 'pending', 'queued', 'running', 'done', 'failed', 'off'));

alter table document_source drop constraint document_source_channel_check;
alter table document_source add constraint document_source_channel_check check (channel in ('upload',
  'fhir', 'apple_health', 'email', 'folder', 'paperless', 'manual', 'genome', 'demo', 'share', 'shortcut',
  'health_auto_export', 'gadgetbridge', 'nightscout'));
alter table document_source add column source_id uuid references source on delete set null;

-- Which word layer a page carries now: its own text layer, our OCR pass, or Paperless's OCR layer.
alter table document_page
  add column word_layer text not null default 'text_layer'
    check (word_layer in ('text_layer', 'ocr', 'paperless_ocr', 'none')),
  add column text_simhash bigint;     -- 64-bit simhash of the normalised page text (dedup level 2)
-- document_page.phash (0001) holds the 64-bit pHash of the rendered page, 8 bytes big-endian.

-- ------------------------------------------------------------------ OCR ----
create table ocr_run (
  id uuid primary key default gen_random_uuid(),
  document_id uuid not null references document on delete cascade,
  page_no int not null check (page_no >= 1),
  engine text not null,                -- 'paddleocr-onnx'
  engine_version text not null,        -- 'pp-ocrv5-mobile-det+latin-mobile-rec@oar-ocr-0.10.2'
  models_sha256 text not null,         -- sha256 over the model files, so a model change is a new run
  status text not null default 'running' check (status in ('running', 'done', 'failed')),
  words_blob bytea,                    -- sha256 of the word JSON (same format as the text layer)
  word_count int,
  mean_conf real check (mean_conf is null or mean_conf between 0 and 1),
  seconds real,
  error text,
  started_at timestamptz not null default now(),
  finished_at timestamptz);
create index on ocr_run (document_id, page_no);

-- Grounding (ingestion.md 7.2, security-privacy.md 4.2 #5): was the row's value found in the page's
-- word layer, and which layer.
alter table candidate_row
  add column grounding text check (grounding in ('grounded', 'ungrounded', 'ungrounded_no_word_layer')),
  add column word_layer text check (word_layer in ('text_layer', 'ocr', 'paperless_ocr', 'none'));

-- ----------------------------------------------- observation-level dedup ----
-- The same fact from a second source (ingestion.md 4, level 3): one observation, one more provenance
-- link. Trends count the observation once.
create table observation_link (
  observation_id uuid not null references observation,
  src_id text not null references src,
  document_id uuid references document,
  linked_at timestamptz not null default now(),
  primary key (observation_id, src_id));

-- ----------------------------------------------------------- review kinds ----
alter table review_item drop constraint review_item_kind_check;
alter table review_item add constraint review_item_kind_check check (kind in ('candidate_row', 'agent_proposal',
  'fhir_unmapped', 'document_identity', 'amended_report', 'rederive_diff', 'genome_reinterpretation', 'job_failed',
  'restore_check_failed', 'pending_document', 'near_duplicate', 'encrypted_document'));

-- ------------------------------------------------------------- wearables ----
-- The connector a device stream came from (Health Auto Export, Gadgetbridge, Nightscout).
alter table wearable_source add column source_id uuid references source on delete set null;
