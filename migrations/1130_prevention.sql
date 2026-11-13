-- 1130, v0.3 Insight (prevention block 1130-1139): what a person decided about a due item, and what
-- they answered about themselves. Both are inputs of the ownhealth.prevention/v1 document, which stays
-- computed on read; nothing here enters the hashed state body.
--
-- Both tables are under the lifecycle trigger: a new decision or answer supersedes the current one, and
-- an undo retracts it. Nothing points at them, and they point at nothing a person can delete except
-- through delete_profile, which removes them with the profile.

-- One decision about one due item, keyed by the item's stable key (a rule id, stale:<rule>:<group>,
-- gap:<key>):
--   snooze:  hidden until `until`, shown again the day after;
--   dismiss: hidden, with the person's reason;
--   done:    a record (evidence_table, evidence_id) or a document proves it. Plain columns, no foreign
--            key: a record that is retracted, or a document that is gone, reopens the item.
create table prevention_decision (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  item_key text not null check (item_key ~ '^[A-Za-z0-9_.:-]{1,200}$'),
  kind text not null check (kind in ('snooze', 'dismiss', 'done')),
  until date,
  reason text check (reason is null or length(reason) between 1 and 500),
  evidence_table text check (evidence_table in ('immunization', 'procedure', 'encounter')),
  evidence_id uuid,
  document_id uuid,
  decided_by uuid,                   -- the app_user; no key, so removing a login keeps the history
  decided_at timestamptz not null default now(),
  status text not null default 'current' check (status in ('current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references prevention_decision,
  superseded_at timestamptz,
  check ((kind = 'snooze') = (until is not null)),
  check (kind <> 'dismiss' or reason is not null),
  check ((evidence_table is null) = (evidence_id is null)),
  check ((kind = 'done') = (evidence_id is not null or document_id is not null)),
  check (evidence_id is null or document_id is null));
create unique index prevention_decision_current on prevention_decision (profile_id, item_key)
  where status = 'current';
create trigger prevention_decision_lifecycle before update on prevention_decision
  for each row execute function oh_clinical_lifecycle_guard();

-- One answer to a question some guideline rows depend on. The keys are the closed list of
-- data/prevention/pack.yaml `answer_keys`.
create table prevention_answer (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  key text not null check (key ~ '^[a-z_]{1,40}$'),
  value text not null check (value in ('yes', 'no')),
  src_id text not null references src,
  recorded_at timestamptz not null default now(),
  status text not null default 'current' check (status in ('current', 'superseded', 'retracted')),
  status_reason text,
  supersedes uuid references prevention_answer,
  superseded_at timestamptz);
create unique index prevention_answer_current on prevention_answer (profile_id, key)
  where status = 'current';
create index on prevention_answer (src_id);
create trigger prevention_answer_lifecycle before update on prevention_answer
  for each row execute function oh_clinical_lifecycle_guard();
