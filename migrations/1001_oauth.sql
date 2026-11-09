-- 1001 foundation, v0.2: the built-in OAuth 2.1 authorization server for remote MCP clients
-- (mcp.md 2.2, security-privacy.md 5). Clients are identified by Client ID Metadata Documents: the
-- client_id is an https URL, and the JSON there names the client and its redirect URIs. No DCR.
-- Access tokens are api_token rows (the architecture.md promise: "OAuth grants reuse it"), so the MCP
-- guard, budgets, audit and revocation work the same for PATs and OAuth clients.

create table oauth_client (            -- the CIMD cache; one row per client_id URL ever seen
  client_id text primary key check (client_id ~ '^https://'),
  client_name text,                    -- from the document; untrusted text, shown with its URL
  client_uri text,
  logo_uri text,
  redirect_uris text[] not null,
  document jsonb not null,             -- the fetched document, verbatim
  fetched_at timestamptz not null default now(),
  expires_at timestamptz not null,     -- HTTP cache lifetime, capped (lane E)
  status text not null default 'active' check (status in ('active', 'blocked')));

-- What a person approved for a client: profiles, scopes, expiry, identity mode. Approving needs step-up.
create table oauth_consent (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null references app_user on delete cascade,
  client_id text not null references oauth_client,
  scopes text[] not null,
  profile_ids uuid[] not null,         -- never an implicit "all"
  default_profile uuid references profile,
  identity_mode text not null default 'full' check (identity_mode in ('full', 'pseudonymous', 'minimal')),
  expires_at timestamptz not null,     -- 90 days default, 365 at most (mcp.md 9.4)
  created_at timestamptz not null default now(),
  revoked_at timestamptz);
create index on oauth_consent (user_id, client_id);

create table oauth_code (              -- authorization codes: single use, short lived, stored hashed
  code_sha256 bytea primary key,
  consent_id uuid not null references oauth_consent on delete cascade,
  client_id text not null references oauth_client,
  redirect_uri text not null,
  code_challenge text not null,        -- PKCE S256 only
  resource text not null,              -- RFC 8707: must be our canonical /mcp URI
  scopes text[] not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz);

-- Access tokens: api_token rows with kind 'oauth'. audience is the RFC 8707 resource the token was
-- issued for; the MCP guard refuses a token whose audience is not its own canonical URI. PATs have no
-- audience (minted by us, for us).
alter table api_token
  add column kind text not null default 'pat' check (kind in ('pat', 'oauth')),
  add column audience text,
  add column oauth_client_id text references oauth_client,
  add column consent_id uuid references oauth_consent on delete cascade,
  add constraint api_token_oauth_shape check (kind = 'pat' or (audience is not null and consent_id is not null));

create table oauth_refresh_token (     -- rotated on every use; a reused one revokes the consent's tokens
  id uuid primary key default gen_random_uuid(),
  consent_id uuid not null references oauth_consent on delete cascade,
  secret_sha256 bytea not null unique,
  access_token_id uuid references api_token on delete set null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz,
  revoked_at timestamptz);
