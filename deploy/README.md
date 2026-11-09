# Deploy, back up, restore

This folder holds the homelab setup and the backup recipe. For a first look on one machine, use the plain [compose.yaml](../compose.yaml) and the [quick start](../docs/quickstart.md) instead.

| File | What it is |
|---|---|
| [compose.homelab.yaml](compose.homelab.yaml) | The app, Postgres 17 and Caddy with HTTPS, and the `ocr` worker when that profile is on. Secrets are files, the containers are read-only |
| [db/app-role.sh](db/app-role.sh) | Gives the app its own database role, which owns no table and cannot rewrite the audit log |
| [Caddyfile](Caddyfile) | HTTPS for `OWNHEALTH_DOMAIN`, open to the LAN and a Tailscale tailnet only |
| [env.homelab.example](env.homelab.example) | The `.env` for the homelab compose |
| [restic.env.example](restic.env.example) | The `restic.env`: the backup repository and its keys, read by the `admin` service only |
| [restic/backup.sh](restic/backup.sh) | Nightly backup into a restic repository, with retention |
| [restic/restore-test.sh](restic/restore-test.sh) | Restores a snapshot into a scratch database and checks it |
| [restic/restore.sh](restic/restore.sh) | Restores a snapshot into a fresh stack |

## Homelab setup

You need Docker with Compose v2 and a name for OwnHealth that your devices resolve, such as `health.example.lan`.

```sh
mkdir -p /opt/ownhealth && cd /opt/ownhealth
git clone --depth 1 https://github.com/GeiserX/OwnHealth.git src
cp src/deploy/compose.homelab.yaml compose.yaml
cp src/deploy/Caddyfile Caddyfile
cp src/deploy/env.homelab.example .env        # set OWNHEALTH_DOMAIN
cp src/deploy/restic.env.example restic.env   # set RESTIC_REPOSITORY, and the S3 keys if it needs them
mkdir -p deploy && cp -r src/deploy/restic src/deploy/db deploy/

mkdir -p inbox && sudo chown 10001:10001 inbox   # the watched folder (docs/recipes/scanner-to-folder.md)
mkdir -m 700 secrets
openssl rand -base64 32 | tr -d '\n' > secrets/db_password
openssl rand -base64 32 | tr -d '\n' > secrets/app_db_password
openssl rand -base64 48 | tr -d '\n' > secrets/secret_key
openssl rand -base64 32 | tr -d '\n' > secrets/restic_password
openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key     # seals inbox passwords and tokens
chmod 644 secrets/*                           # the containers run as their own users; the folder stays 700

docker compose up -d
docker compose run --rm admin restic init     # once, creates the backup repository
```

Open `https://<OWNHEALTH_DOMAIN>` and create the first account with a passkey. Write down the recovery codes.

**Two database roles.** `ownhealth` owns the tables. The one-shot `admin` service connects as it, runs the migrations, then runs `db/app-role.sh`, which creates `ownhealth_app` and grants it SELECT, INSERT, UPDATE and DELETE on every table except UPDATE, DELETE and TRUNCATE on the audit log. The app connects as `ownhealth_app` and waits for `admin` to finish. It owns nothing, so it cannot disable the audit log's trigger or alter a table. The backup scripts run in `admin` too, because a restore needs the owner. Only `admin` reads `restic.env`, so the app, the process that faces the network, never holds the keys to the backups. An older install has `RESTIC_REPOSITORY` and the S3 keys in `.env`; it keeps working, and moving those lines into `restic.env` takes them away from the app.

**Store the restic password in your password manager now.** The backup is encrypted with it and it is not inside the backup. Without it, the backup cannot be read.

**Store `secrets/secrets_key` there too.** It encrypts the passwords and tokens OwnHealth keeps for its inboxes (an IMAP password, a Paperless or Nightscout token, PDF passwords). The backup holds them only in encrypted form. Restored with another key, those inboxes stop until you enter their secrets again.

**HTTPS.** Caddy gets a certificate on its own when `OWNHEALTH_DOMAIN` is a public DNS name. For a name only your LAN knows, put `tls internal` in the site block of the Caddyfile and trust Caddy's root certificate on your devices, or use a Caddy build with your DNS provider's challenge plugin. Passkeys need HTTPS on a real name, so plain HTTP on an IP address does not work past the first look.

**Who can connect.** The Caddyfile answers 403 to every address outside the private ranges and the Tailscale range `100.64.0.0/10`. Keep it that way unless you mean to put health data on the public internet. The check works only if Caddy sees the real client address. Rootless Docker, and Docker's userland proxy for IPv6 clients when the compose network has no IPv6, hand Caddy a private gateway address instead, so every client passes. On such a host, or one with a public address, also close ports 80 and 443 (TCP and UDP) to the internet in the host firewall or the router, and check from outside that the name does not answer. The claude.ai connector is the one case that needs a public endpoint: [its recipe](../docs/recipes/claude-connector.md) opens only the four paths it uses and keeps the rest behind a login.

**Your own reverse proxy.** If one already runs on the host, drop the `caddy` service, give `app` a port such as `127.0.0.1:8080:8080`, and point your proxy at it. Set `OWNHEALTH_BASE_URL` to the HTTPS address people open.

## Inboxes and OCR

Since 0.3.0, documents and device data can arrive without an upload: a mailbox, a watched folder, Paperless-ngx, an iOS Shortcut, Health Auto Export, Nightscout. You add each one in the app, under **Inbox**. The setup for each is in [docs/recipes](../docs/recipes/README.md).

**The watched folder** is `./inbox` next to `compose.yaml`, mounted as `/inbox`. A subfolder per profile slug (`inbox/alex/`) sends files to that person. The app runs as user 10001 and moves imported files to `inbox/.done/`, so the folder must be writable by that user.

**OCR for scans and photos** is the `ocr` profile. Add two lines to `.env`:

```sh
COMPOSE_PROFILES=ocr   # starts the `ocr` worker, from drumsergio/ownhealth:<version>-ocr
OWNHEALTH_OCR=1        # the app sends pages without a text layer to it
```

then `docker compose up -d`. The worker is the same program from the image variant that carries the OCR engine (PaddleOCR PP-OCRv5 on ONNX Runtime) and its models, about 75 MB more than the default image (the OCR engine and 13 MB of models). It serves no HTTP and only runs jobs on the `ocr` queue, on the CPU. Turn both lines on together. With only `OWNHEALTH_OCR=1`, scans queue for a worker that never starts. Without the profile, scans wait as pending documents for your own agent ([agent-assisted extraction](../docs/agent-extraction.md)) or for you.

Scans that arrived while OCR was off are not lost: each time the `ocr` worker starts with `OWNHEALTH_OCR=1`, it queues every scan and photo still waiting as a pending document for pages without text, and takes it off the pending list. Pages OCR then cannot read go back on it.

## Monitoring the inboxes

`GET /api/inboxes/health` lists every inbox you can see with its last run, its last successful sync, its last error, the failed runs in a row, and `stale`: switched on, polled, and no success for three poll intervals. `last_run_status` is `ok`, `partial` (some items were skipped or failed) or `error`, with `last_run_problems` and the first problem as `last_run_problem` (a code and a short message, never the content of a mail or document). A run in which every item failed counts as an error: the failed-runs count goes up and the last successful sync stays where it was. Add `?format=prometheus` for the Prometheus text format. Labels carry the source id and kind only, never names or error text:

```text
ownhealth_source_enabled{source="…",kind="email"} 1
ownhealth_source_stale{source="…",kind="email"} 0
ownhealth_source_consecutive_failures{source="…",kind="email"} 0
ownhealth_source_last_success_timestamp_seconds{source="…",kind="email"} 1762700112.5
ownhealth_source_last_run_timestamp_seconds{source="…",kind="email"} 1762700112.5
ownhealth_source_last_run_ok{source="…",kind="email"} 1
```

`ownhealth_source_last_run_ok` is 1 when the newest finished run brought everything in (or no run has finished yet) and 0 when it was partial or failed.

A monitor has no browser session, so it reads one inbox at a time with that inbox's own push token (`oh_ing_...`, created in **Inbox**): `GET /api/inboxes/<id>/health?format=prometheus` with `Authorization: Bearer oh_ing_...`. The token sees its own inbox and nothing else. For a push inbox (Paperless, Shortcut, Health Auto Export) it is the token the sender already uses. For a polled one (mailbox, folder, Nightscout), create a token just for the monitor.

A Prometheus job per inbox:

```yaml
scrape_configs:
  - job_name: ownhealth-mailbox
    scheme: https
    metrics_path: /api/inboxes/<source id>/health
    params: { format: [prometheus] }
    authorization: { credentials_file: /etc/prometheus/ownhealth-mailbox.token }
    static_configs: [{ targets: ["health.example.lan"] }]
```

Alert rules that cover both kinds of inbox:

```yaml
groups:
  - name: ownhealth-inboxes
    rules:
      - alert: OwnHealthInboxStale        # a polled inbox stopped syncing
        expr: ownhealth_source_stale == 1
        for: 30m
      - alert: OwnHealthPushSilent        # nothing pushed for 6 hours
        expr: ownhealth_source_enabled == 1 and time() - ownhealth_source_last_success_timestamp_seconds{kind=~"health_auto_export|paperless|shortcut"} > 6 * 3600
      - alert: OwnHealthInboxPartial      # runs finish but skip items
        expr: ownhealth_source_last_run_ok == 0
        for: 3m                           # three runs of a mailbox polled every minute
```

A push inbox has no schedule, so OwnHealth never marks it stale; the second rule alerts on silence instead. Pick the hours to match how often the sender pushes.

## Backups

`ownhealth backup` writes three parts into one directory.

- `db.dump`: `pg_dump -Fc` of the database.
- `blobs/`: the original documents and genome files, hard-linked from the data volume, so the staging copy costs no space.
- `manifest.json`: app version, migrations, row count per table, the list of blob hashes and the `state_hash` of every profile.

The dump, the row counts and the state hashes come from one database snapshot. A backup taken while someone imports a lab report is still consistent.

`restic/backup.sh` writes that directory, takes one restic snapshot of it, deletes the staging copy and applies retention (7 daily, 5 weekly, 12 monthly; set `OWNHEALTH_RESTIC_KEEP` to change it). restic encrypts and deduplicates, so a nightly run only uploads what changed.

```sh
cd /opt/ownhealth
./deploy/restic/backup.sh
```

Run it from cron or a systemd timer:

```cron
15 3 * * *  cd /opt/ownhealth && ./deploy/restic/backup.sh >> backup.log 2>&1
45 4 * * 0  cd /opt/ownhealth && ./deploy/restic/restore-test.sh >> backup.log 2>&1
```

## Restore test

A backup that was never restored counts as missing. `restic/restore-test.sh` proves the latest snapshot works:

1. restic restores the snapshot into `/data/backups/restore`.
2. `ownhealth verify-restore` loads the dump into a scratch database `<db>_restore_check`.
3. It compares the row count of every table with the manifest, hashes every blob again, rebuilds the state of every profile and compares each `state_hash` with the manifest.
4. It drops the scratch database and the restored files.

It prints a JSON report and exits 1 on any difference.

```json
{
  "ok": true,
  "as_of": "2026-11-07",
  "tables": {},
  "profiles": {
    "2f42707f-20c9-406c-b04a-d17c398b68a6": {"expected": "sha256:5c61…", "actual": "sha256:5c61…", "ok": true}
  },
  "blobs": {"checked": 27, "missing": [], "corrupt": []}
}
```

`tables` lists only the tables whose counts differ. Pass a snapshot id to test an older one: `./deploy/restic/restore-test.sh 4be1a2c3`. It only reads the live database and blobs.

## Restore

Use this to restore onto a new machine, or after losing the volumes.

```sh
cd /opt/ownhealth                 # same compose.yaml, .env and secrets/ as before
docker compose down               # if anything still runs
docker volume rm ownhealth_ownhealth-data ownhealth_ownhealth-pg   # only when you mean to replace them
docker compose up -d db
./deploy/restic/restore.sh        # or: ./deploy/restic/restore.sh <snapshot id>
docker compose up -d
```

`restore.sh` runs in a one-off admin container before the app starts. It refuses a database that already has tables, so it cannot overwrite a running instance. After loading the dump and the blobs it rebuilds every profile's state and compares the `state_hash` with the manifest; the same hash means the restored data is the data you backed up. It exits 1 if anything differs.

Restore with the OwnHealth version that made the backup (`app_version` in the manifest), then upgrade. A newer rule pack can change a `state_hash` on purpose, and the check would report it as a difference.

## Upgrades

Change the image tag in `compose.yaml` to the new version (and the `ocr` service's `<version>-ocr` tag with it), then:

```sh
./deploy/restic/backup.sh         # a fresh backup before every upgrade
docker compose pull && docker compose up -d
```

Migrations are forward-only and run at start, in the `admin` service, before the app comes up. To go back to an older version, restore the backup you took before the upgrade.

**To 0.3.0.** Take the new `compose.homelab.yaml` (it adds the `ocr` worker, the inbox folder and the `secrets_key` secret), then before `docker compose up -d`:

```sh
openssl rand -base64 32 | tr -d '\n' > secrets/secrets_key && chmod 644 secrets/secrets_key
mkdir -p inbox && sudo chown 10001:10001 inbox
```

Compose refuses to start without the secret file. Keep a copy of it with the restic password.

Scans a 0.2.0 install kept as pending documents are read by OCR once you turn the `ocr` profile on: the worker queues them when it starts.
