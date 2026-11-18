-- 1131, v0.3 Insight (prevention block 1130-1139): a calendar feed of one profile's due items.
--
-- A feed is a secret URL a calendar app polls (GET /api/ics/<token>.ics, no cookie). The token is
-- 'oh_ics_<id8>_<secret43>' and is kept as a push token is: the prefix and the sha256 of the secret,
-- never the plaintext. Each fetch reads as its creator: the feed stops when the creator loses read
-- access to the profile or is disabled, and when someone revokes it.
--
-- Access control, not a clinical row: no lifecycle trigger, and the raw part of a person's export
-- leaves the table out (ownhealth-ops export.rs SKIP_RAW). Deleting the profile removes its feeds.
-- Closing a login keeps its user row (disabled), so its feeds stay but stop answering.
create table calendar_feed (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile on delete cascade,
  created_by uuid not null references app_user on delete cascade,
  prefix text not null unique check (prefix ~ '^oh_ics_[a-z0-9]{8}$'),
  secret_sha256 bytea not null,
  -- generic: every event reads "OwnHealth: 1 item due"; named: the screening or vaccine's name
  detail text not null default 'generic' check (detail in ('generic', 'named')),
  created_at timestamptz not null default now(),
  last_used_at timestamptz,
  revoked_at timestamptz);
create index on calendar_feed (profile_id, created_by);
