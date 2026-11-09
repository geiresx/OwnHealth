# Nightscout

If your continuous glucose monitor (CGM) feeds a Nightscout site, OwnHealth can pull the readings and keep a 14-day CGM summary: mean glucose, glucose management indicator (GMI), time in ranges and sensor wear. FreeStyle Libre readers that reach Nightscout through nightscout-connect (LibreLinkUp) arrive the same way.

## How it works

- Every 5 minutes OwnHealth asks your Nightscout site for readings newer than the last one it has, with a read-only token.
- It speaks the v1 API (`/api/v1/entries/sgv.json`) or the v3 API (`/api/v3/entries`, after exchanging the token for a short-lived JWT).
- Readings become CGM samples under LOINC 99504-3. Values below 39 mg/dL are sensor status codes, not readings, and are dropped, as Nightscout's own reports drop them.
- The first run goes back a number of days (the backfill) and later runs carry on from the newest reading.
- Errors name the HTTP status, never the URL, because the URL carries the token.

## The summary

`GET /api/profiles/<profile>/cgm/summary?days=14` and the profile's wearables page show:

| Measure | Code |
|---|---|
| Mean glucose | LOINC 97507-8 |
| GMI | from the mean, as the international consensus defines it |
| Time in ranges: very low (under 54 mg/dL), low (54 to 69), in range (70 to 180), high (181 to 250), very high (over 250) | panel LOINC 106793-3 |
| Sensor wear | percent of expected readings present |

A summary counts as valid only over 14 days with at least 70% sensor wear, per the international time-in-range consensus. Otherwise OwnHealth says why (`no_readings`, `short_window`, `insufficient_wear`) instead of giving numbers. These are numbers about your data, not a diagnosis.

## You need

- A Nightscout site (any recent version) that OwnHealth can reach.
- OwnHealth with `OWNHEALTH_SECRETS_KEY` set. OwnHealth seals the Nightscout token with it.

## 1. A read-only token in Nightscout

In Nightscout, open **Admin Tools**, add a subject (for example `ownhealth`) with the role `readable`, and copy its access token (it looks like `ownhealth-1a2b3c4d5e6f7a8b`). A readable token cannot change your Nightscout data.

## 2. The source in OwnHealth

In OwnHealth, open **Inbox**, add a source of kind **Nightscout** and fill in:

| Field | Example | Notes |
|---|---|---|
| Profile | Lucía | Whose readings these are |
| Nightscout URL | `https://lucia-ns.example.com` | The site's base address |
| Token | the readable token | Stored encrypted, never shown again |
| API | v1 | v3 also works; v1 is the safe default |
| Backfill days | 90 | How far back the first run reads |
| Allow a private address | off | Turn it on only for a site on your LAN |

## 3. Check it

**Sync now** on the source runs one pull. The run shows how many readings were new. The CGM summary appears on the profile once there is data. Compare it with Nightscout's own **Reports**, **Distribution** for the same 14 days. Time in range should match within one percentage point.

If the pull fails, the source's last error says why: `401` or `403` means the token is wrong or lacks the readable role. A refused private address means the URL is on your LAN and the source does not allow that.

## Notes

- One source per person and site.
- OwnHealth only reads. It never writes treatments, profiles or notes back to Nightscout.
- To be told when readings stop arriving, see [Monitoring the inboxes](../../deploy/README.md#monitoring-the-inboxes).
