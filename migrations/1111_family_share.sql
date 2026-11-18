-- 1111, v0.3 Insight: per-relative consent for cross-profile hints (data/family/hints.yaml).
--
-- One row says that the source profile lets its results produce hints in the target profile's family
-- document (ownhealth.family/v1, computed on read): `labs` for a lab hint such as LDL-C of 190 mg/dL
-- or more, `cascade` for a genetic finding. A hint names the rule and the relationship only, never the
-- source, its values or its src ids.
--
-- Consent is a setting, not a clinical record: the table is outside the lifecycle trigger. Granting
-- inserts or widens a row, revoking narrows or deletes it, and the hint goes with it at once. Both keys
-- cascade, so deleting either profile removes the consent.
create table family_share (
  source_profile_id uuid not null references profile on delete cascade,
  target_profile_id uuid not null references profile on delete cascade,
  categories text[] not null
    check (cardinality(categories) > 0 and categories <@ array['labs', 'cascade']),
  granted_by uuid,                   -- the app_user; no key, so removing a login keeps the consent
  granted_at timestamptz not null default now(),
  primary key (source_profile_id, target_profile_id),
  check (source_profile_id <> target_profile_id));
create index on family_share (target_profile_id);
