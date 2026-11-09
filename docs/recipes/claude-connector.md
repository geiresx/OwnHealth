# claude.ai remote connector

claude.ai can use OwnHealth's MCP server as a custom connector. It signs in with OAuth: you approve it once in OwnHealth, choosing which profiles and which kinds of data it may read, and you can revoke it at any time. There is no token to paste. Claude Code and other local agents keep using personal access tokens, as in [agent-assisted extraction](../agent-extraction.md).

claude.ai connects from Anthropic's servers, not from your browser, so `/mcp` has to be reachable from the internet. This recipe puts OwnHealth behind a Cloudflare Tunnel and keeps everything a person uses behind Cloudflare Access. Only the four machine endpoints the connector needs are public, and those need an OAuth token or hand one out only after your approval.

## What has to be public

| Path | Who calls it | Why |
|---|---|---|
| `/mcp` | claude.ai | the MCP server; every call carries a token whose audience is exactly `https://<your name>/mcp` |
| `/.well-known/oauth-protected-resource` and `/.well-known/oauth-protected-resource/mcp` | claude.ai | where to sign in (RFC 9728) |
| `/.well-known/oauth-authorization-server` | claude.ai | the sign-in endpoints (RFC 8414) |
| `/oauth/token` | claude.ai | swaps the approved code for a token |

Everything else (the app, `/oauth/authorize`, the consent screen, the API) is opened by your own browser, and can stay behind Cloudflare Access.

## Before you start

- **The public name is the instance's name.** `OWNHEALTH_BASE_URL` sets the OAuth issuer, the token audience and the passkey domain, so it must be the name the tunnel serves, for example `https://health.example.com`. OAuth runs only on an `https://` base URL.
- **Passkeys belong to a name.** Passkeys made under another name (`health.example.lan`) do not work under the new one. Pick the public name before enrolling, or plan to sign in with a recovery code and enrol again.
- **OwnHealth needs outbound HTTPS.** It fetches claude.ai's client description (its Client ID Metadata Document) from the URL claude.ai gives as `client_id`, with a 5 second timeout and a 64 KiB cap, and only from public addresses. There is no client registration step.
- A Cloudflare account with a domain on it.

## 1. The tunnel

In the Cloudflare dashboard, **Zero Trust**, **Networks**, **Tunnels**: create a tunnel, copy its token, and add a public hostname `health.example.com` with service `http://app:8080`.

Add the connector to your compose file, next to `app`:

```yaml
  tunnel:
    image: cloudflare/cloudflared:2026.10.7
    restart: unless-stopped
    command: ["tunnel", "--no-autoupdate", "run"]
    environment:
      TUNNEL_TOKEN: ${CLOUDFLARE_TUNNEL_TOKEN:?set CLOUDFLARE_TUNNEL_TOKEN in .env}
    depends_on: [app]
```

In `.env`, set `CLOUDFLARE_TUNNEL_TOKEN`, and set the name: `OWNHEALTH_DOMAIN=health.example.com` for `deploy/compose.homelab.yaml`, or `OWNHEALTH_BASE_URL=https://health.example.com` for the plain `compose.yaml`. The tunnel now does HTTPS, so the homelab file's `caddy` service is no longer needed for this name.

`docker compose up -d`.

## 2. Cloudflare Access in front of the app

**Zero Trust**, **Access**, **Applications**:

1. A self-hosted application for `health.example.com` (every path), with a policy that **allows** only your own email addresses.
2. A second self-hosted application for the same name with the paths `mcp`, `.well-known/*` and `oauth/token`, with a **Bypass** policy for everyone.

Access applies the most specific path, so the second one lets claude.ai through to those four endpoints, and only those. Check from a browser in a private window: `https://health.example.com/` asks for the Access login, and `https://health.example.com/.well-known/oauth-authorization-server` answers JSON.

## 3. Connect claude.ai

In claude.ai, **Settings**, **Connectors**, **Add custom connector**, URL `https://health.example.com/mcp`. Then **Connect**:

1. claude.ai opens OwnHealth's sign-in page in your browser (after the Access login).
2. Sign in with your passkey. Approving a client needs a sign-in from the last five minutes.
3. The consent screen shows the client's name and the URL of its description, the data it asks for, and a checkbox for each profile you can open. Tick what this client may see. Start small: the health summary and labs for your own profile. Leave **raw document text** and **raw genome** off unless you need them.
4. Approve. claude.ai gets a token for 90 days (you can pick from 1 to 365).

Ask Claude something only your data can answer ("What was my last LDL?"). OwnHealth's audit log shows each call under the client's name.

## Revoke

**Settings**, **Connected apps** lists every approved client. Revoking one stops its tokens at once. Your personal access tokens are not affected.

## If it does not connect

- claude.ai says it cannot reach the server: check the tunnel is up (`docker compose logs tunnel`) and that the Bypass application covers `mcp`.
- The sign-in page says the client is not allowed: OwnHealth could not fetch claude.ai's client description. The app needs outbound HTTPS.
- `401` with `invalid_token` on every call: the token's audience does not match. `OWNHEALTH_BASE_URL` must be exactly the name claude.ai uses, with `https://` and no path.

## Without Cloudflare

Tailscale Funnel also gives a public HTTPS name (`<machine>.<tailnet>.ts.net`), with no account at a third party beyond Tailscale. It publishes the whole app, though. Funnel has no path rule, so the sign-in page is on the internet too, protected only by your passkeys. Run `tailscale funnel --bg 8080` on the host with the app's port published on `127.0.0.1:8080`, and set `OWNHEALTH_BASE_URL` to the `ts.net` name.
