-- 1220, v1.0 Connected (wearables block 1220-1229): vendor scores, stored apart from measurements.
--
-- Readiness, recovery, sleep and activity scores, stress, strain and Body Battery are numbers a
-- vendor computes with its own method (connectors.md section 4). They are not measurements, so they never
-- go into wearable_sample: they live here, tagged computed_by 'provider', with the provider's name.
-- The rule pack, the calculators and MCP never read this table; only the Wearables page shows it.
--
-- One row per profile, connection, provider, category and time, so a re-pull updates the row in place
-- and two connections that bring the same vendor keep a row each. `source_id` is the connection that
-- brought the score; removing the connection keeps the score, which then names no connection.
-- `qualifier` is the vendor's own word for the score ("Optimal"), `components` its contributors.
-- Not a clinical row: no lifecycle trigger. Deleting the profile deletes its scores.
create table vendor_score (
  id bigint generated always as identity primary key,
  profile_id uuid not null references profile,
  source_id uuid references source on delete set null,
  provider text not null check (provider ~ '^[a-z0-9_]{1,40}$'),
  category text not null check (category in ('readiness', 'recovery', 'sleep', 'activity', 'stress',
    'strain', 'body_battery')),
  t timestamptz not null,
  value double precision not null check (value = value and value between -1e9 and 1e9),
  qualifier text check (char_length(qualifier) <= 80),
  components jsonb check (jsonb_typeof(components) = 'object'),
  computed_by text not null default 'provider' check (computed_by = 'provider'),
  unique (profile_id, source_id, provider, category, t));

-- The guard: a series key that names a vendor score cannot reach wearable_sample. A key must be plain
-- (lowercase letters, digits and '_', starting with a letter), so 'BodyBattery', 'body-battery' and
-- 'stress.level' are refused as they are. Then the key, lowercased with everything but letters and
-- digits taken out, is refused when it contains one of the score words, or starts with recovery
-- ("hr_recovery" is a measurement). hrv_status and training_status are Garmin's verdicts, so 'status'
-- is a score word. ownhealth-devices scores::is_sample_key is the same rule in Rust, and its tests
-- check that both agree.
alter table wearable_sample add constraint wearable_sample_not_a_vendor_score check (
  metric ~ '^[a-z][a-z0-9_]*$'
  and lower(regexp_replace(metric, '[^A-Za-z0-9]', '', 'g'))
    !~ '(readiness|battery|stress|strain|score|charge|resilience|status|performance)'
  and lower(regexp_replace(metric, '[^A-Za-z0-9]', '', 'g')) !~ '^recovery');
