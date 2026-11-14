-- 1101 supplements as first-class records (v0.3 records block 1100-1109): the person's own decision on a
-- medicine or supplement, and photos of its label.
--
-- The decision columns are nullable and have no foreign key. The lifecycle trigger keeps them immutable
-- like every other column, so a new decision is a new version (the /correct route). They never enter the
-- state body.
--   decision:        keep | stop | do_not_repurchase, as the person recorded it.
--   decision_reason: why, as the person wrote it.
--   decision_on:     the day the person decided.
alter table medication_statement
  add column decision text check (decision in ('keep', 'stop', 'do_not_repurchase')),
  add column decision_reason text,
  add column decision_on date;

-- A label photo is a document (doc_type 'other', no extraction job). The link sits outside the lifecycle
-- trigger: a cascaded SET NULL on an immutable row raises 23514, which delete_profile does not retry.
-- Both sides cascade, so deleting the record, the document or the profile removes the link. A new
-- version of the record gets a copy of the link in the same transaction.
create table medication_label (
  medication_id uuid not null references medication_statement on delete cascade,
  document_id uuid not null references document on delete cascade,
  created_at timestamptz not null default now(),
  primary key (medication_id, document_id));
create index on medication_label (document_id);
