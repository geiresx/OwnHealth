# Paperless-ngx

Lab reports you already file in Paperless-ngx show up in OwnHealth's review queue. Tag a document `health` and Paperless tells OwnHealth about it. OwnHealth fetches the original file and reads it. The review item links back to the document in Paperless.

## How it works

1. A Paperless workflow fires on "Document Added" for documents tagged `health` and posts the document's URL to OwnHealth.
2. OwnHealth takes the id from that URL and nothing else from the request. It asks the Paperless API for the document's tags and downloads the original with a read-only API token.
3. The file then takes the same path as an upload. OwnHealth catches duplicates by hash, reads PDFs with a text layer at once, and leaves scans for OCR or your agent.
4. Every 15 minutes OwnHealth also asks Paperless for tagged documents changed since the last run, so a missed webhook costs at most a quarter of an hour.

OwnHealth reads only a document's id, tags, file name and change time. Its title, content, notes and custom fields are text anyone with Paperless access can write, so they are never fetched.

## You need

- Paperless-ngx 2.15 or newer (workflow webhooks), with `PAPERLESS_URL` set, because the `{{doc_url}}` placeholder needs it.
- OwnHealth 0.3.0 or newer, with `OWNHEALTH_SECRETS_KEY` set (the Paperless token is stored sealed with it).
- Network: Paperless must reach OwnHealth for the webhook, and OwnHealth must reach Paperless for the API.

## 1. A read-only Paperless user and token

In Paperless, create a user (for example `ownhealth`) with view permission on documents and tags only. Open its profile and create an API token. Keep it for step 2.

Also create the `health` tag if you do not have one.

## 2. The Paperless inbox in OwnHealth

In OwnHealth, open **Inbox**, add a source of kind **Paperless-ngx** and fill in:

| Field | Example | Notes |
|---|---|---|
| Profile | Alex | Where documents go unless a tag says otherwise |
| Paperless URL | `http://paperless.example.lan:8000` | The address OwnHealth uses to call the API |
| Health tag | `health` | The tag name or its numeric id |
| Allow a private address | on | Needed when Paperless is on your LAN. OwnHealth refuses private addresses unless the source allows them |
| API token | the token from step 1 | Stored encrypted, never shown again |

Optionally map tags to profiles (for example a `health-lucia` tag to Lucía's profile).

Save, then create the source's push token. It starts with `oh_ing_` and is shown once.

## 3. The workflow in Paperless

**Workflows**, add one:

- Trigger: **Document Added**, filter on the tag `health`.
- Action: **Webhook**.
  - URL: `https://health.example.lan/api/ingest/paperless`
  - Body as JSON: one key `doc_url` with the value `{{doc_url}}`.
  - Header: `X-OwnHealth-Secret` with the `oh_ing_...` token from step 2.

Paperless sends the token in a header because its webhook action has no bearer setting. The token works only for this source and only on this route.

If Paperless runs with `PAPERLESS_WEBHOOKS_ALLOW_INTERNAL_REQUESTS=false`, it will not call a LAN address. Leave it at the default (`true`) or give OwnHealth an address Paperless accepts.

## 4. Check it

Tag a lab PDF `health` in Paperless. Within a minute it shows in OwnHealth's review queue, with a link to the document in Paperless. The Paperless inbox in **Inbox** shows the last successful sync.

If nothing arrives:

- **Inbox** shows the source's last error. `401` from OwnHealth means the `X-OwnHealth-Secret` header is wrong or the source is switched off. `403 wrong_source` means the token belongs to another source.
- An error naming the Paperless API means OwnHealth could not reach Paperless or the token cannot read the document.
- A document tagged after the fact (not at "Document Added") arrives with the next 15-minute pull.

To alert on a stalled inbox, see [Monitoring the inboxes](../../deploy/README.md#monitoring-the-inboxes).

## Notes

- The same document re-sent by the webhook and found again by the pull is imported once, keyed on the Paperless id.
- OwnHealth does not tag documents back in Paperless (`ownhealth-imported`): that needs a token that can write, and the read-only one is safer.
- Scans: OwnHealth takes the original, not Paperless's archived PDF with its OCR layer. With the `ocr` profile on, OwnHealth reads the scan itself; without it, the scan waits for your agent ([agent-assisted extraction](../agent-extraction.md)).
