# Quick start

From nothing to your first lab report in OwnHealth, on one machine, in about ten minutes. For a homelab with HTTPS, a real name and backups, follow [deploy/README.md](../deploy/README.md) afterwards.

## What you need

- Docker with Compose v2.
- About 1 GB of free RAM for the app and Postgres, and disk for your documents.
- A browser on the same machine. Passkeys need `localhost` or HTTPS.

## 1. Start it

```sh
git clone https://github.com/GeiserX/OwnHealth.git
cd OwnHealth
cp .env.example .env
openssl rand -hex 32     # paste as OWNHEALTH_DB_PASSWORD in .env
openssl rand -hex 32     # paste as OWNHEALTH_APP_DB_PASSWORD in .env
openssl rand -hex 48     # paste as OWNHEALTH_SECRET_KEY in .env
openssl rand -base64 32  # paste as OWNHEALTH_SECRETS_KEY in .env (encrypts inbox passwords and tokens)
mkdir -p inbox && sudo chown 10001:10001 inbox   # the watched folder; the app runs as user 10001
docker compose up -d
```

The app moves each file it imports from `inbox/` to `inbox/.done/`. If it cannot write there, the folder inbox shows as failing.

Compose pulls `drumsergio/ownhealth` and `postgres:17.11`. Before the first release there is no image to pull, so Compose builds it from the source tree; that takes a few minutes.

Check that it runs:

```sh
docker compose ps -a                     # app and db: healthy; admin: exited (0)
curl -s http://localhost:8080/healthz    # {"status":"ok","version":"..."}
```

`admin` is a one-shot container: it runs the migrations as the database owner `ownhealth`, gives the app its own role `ownhealth_app`, and exits. The app connects as `ownhealth_app`, which can add to the audit log but not change or delete it.

The app listens on `127.0.0.1:8080` only. Nothing on your network can reach it until you put a reverse proxy in front.

## 2. Create your account

Open http://localhost:8080. The first visit asks for your name and language and creates the household, your login and your own profile, with you as owner. Register a passkey, then print or save the recovery codes it shows. They are the only way back in if you lose the passkey.

## 3. Import a lab report

1. Open **Import & review** and drop a lab report PDF on it, or pick the file.
2. Values the importer is sure of are saved at once, each linked to its page and position on the page. A value is sure only when every check passes and the lab layout or code is known.
3. Everything else waits in the review queue. The page is on the left with each value boxed, the extracted rows on the right. Accept, fix or reject each row. "Accept all" takes only the rows that passed every check.
4. Open **Health at a glance**. Each body system shows OK, Watch, Out, Stale or No data, with the date of the newest value.

Other sources work the same way: a FHIR or IPS bundle, the Apple Health `export.zip`, and the `genome-facts.json` that [OwnGenome](https://github.com/GeiserX/OwnGenome) writes.

To have reports arrive without an upload, open **Inbox** and add a mailbox, the watched folder, Paperless-ngx or a phone. The [recipes](recipes/README.md) walk through each one. Mail from a sender you have not allowed waits in quarantine, unread, until you release it.

## 4. Connect an agent

Open **MCP & access**, create a token, choose the profiles and scopes, and copy it. It is shown once. Then:

```sh
export OWNHEALTH_PAT=oh_pat_...
claude mcp add --transport http ownhealth http://localhost:8080/mcp \
  --header "Authorization: Bearer ${OWNHEALTH_PAT}"
```

Ask Claude Code "how is my health right now?". It calls `get_health_status` and cites a `src` id for every value. The tools, scopes and limits are in [mcp-tools.md](mcp-tools.md).

## 5. Look around with the demo family first

To see every screen with data before you import your own, start a throwaway copy with the synthetic Rivera family:

```sh
OWNHEALTH_DEMO=1 OWNHEALTH_DEV_AUTH=1 OWNHEALTH_HTTP_PORT=8081 docker compose -p ownhealth-demo up -d
```

Open http://localhost:8081. It needs the same `.env` as step 1 and runs beside your real install, with its own volumes.

`OWNHEALTH_DEV_AUTH=1` logs every visitor in as the first owner. Never set it, or `OWNHEALTH_DEMO`, on an install that holds real data. Remove the demo with `docker compose -p ownhealth-demo down -v`.

## Day to day

| Task | Command |
|---|---|
| Logs | `docker compose logs -f app` |
| Stop | `docker compose down` (your data stays in the volumes) |
| Update | set the new version in `compose.yaml`, then `docker compose pull && docker compose up -d` |
| Back up | [deploy/README.md](../deploy/README.md#backups) |
| Delete everything | `docker compose down -v` (removes the volumes: all records and documents) |

## Good to know

- OwnHealth keeps records and shows values with the formulas and sources they come from. It is information, not diagnosis.
- Only synthetic data belongs in bug reports and pull requests. See [CONTRIBUTING.md](../CONTRIBUTING.md).
- What we protect, and from whom: [design/security-privacy.md](design/security-privacy.md#3-threat-model).
