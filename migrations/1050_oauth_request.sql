-- 1050 lane E, v0.2: a pending authorization. /oauth/authorize checks the client (CIMD), the
-- redirect URI, PKCE S256 and the resource, stores what it asked for here and sends the browser to the
-- consent screen, which reads it by id. Approving or denying settles it once; it lapses after ten
-- minutes. Nothing here is a secret: the code is issued only on approval, into oauth_code.

create table oauth_request (
  id uuid primary key default gen_random_uuid(),
  client_id text not null references oauth_client,
  redirect_uri text not null,
  state text,                          -- the client's opaque value, echoed back; never shown
  code_challenge text not null,        -- S256
  resource text not null,              -- RFC 8707, our canonical /mcp URI
  scopes text[] not null,              -- what the client asked for; the person may grant less
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  decided_at timestamptz,
  decided_by uuid references app_user on delete set null,
  decision text check (decision in ('approved', 'denied')));
