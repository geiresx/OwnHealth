# Health Auto Export

The iOS app Health Auto Export can push Apple Health data to a URL on a schedule. Point it at OwnHealth and steps, heart rate, sleep, weight and the rest arrive every 15 minutes, with no export file to upload.

## How it works

- The app posts JSON (export version 2) to `POST /api/ingest/health-auto-export` with the source's push token.
- Each push is stored as received, then mapped to wearable samples for the source's profile.
- Pushes overlap on purpose, and the hour still running at one push is sent again, larger, at the next. OwnHealth keeps one sample per profile, metric, device and time, and updates a bucket that grew. Seven days of 15-minute pushes give zero duplicate samples.
- A sample summed from Watch and iPhone is filed under the Watch, so the same bucket always lands on the same device.
- Metrics OwnHealth does not chart yet (workouts, nutrition, symptoms) are skipped and named in the log, without values.

## You need

- Health Auto Export with its REST API automation (a paid feature of the app).
- An iPhone that can reach OwnHealth: on your LAN, or through Tailscale or another VPN on the phone. The app pushes only when iOS lets it run, which is more often with the phone unlocked and charging.

## 1. The source in OwnHealth

In OwnHealth, open **Inbox** and add a source of kind **Health Auto Export**. Pick the profile the data belongs to (one source per person and phone). Save, then create the push token. It starts with `oh_ing_`, is shown once, and works only for this source on this route.

## 2. The automation in the app

In Health Auto Export, add an automation of type **REST API**:

| Setting | Value |
|---|---|
| URL | `https://health.example.lan/api/ingest/health-auto-export` |
| Headers | `Authorization` = `Bearer oh_ing_...` |
| Export format | JSON |
| Export version | 2 |
| Data | Health Metrics (pick the ones you want) |
| Date range | a range that overlaps the last push, for example the last day |
| Sync cadence | every 15 minutes |

Menu names change between app versions. What matters is the URL, the header, JSON and version 2.

## 3. Check it

Run the automation once by hand from the app. The source in **Inbox** shows a successful run with how many samples were new and how many were already there. Run it again and the second run adds no new samples. The values appear under the profile's wearables.

If the push fails:

- `401`: the token is wrong, or the source is switched off.
- `403 wrong_source`: the token belongs to another kind of source.
- No run at all: the phone cannot reach the server, or iOS did not let the app run. Open the app to push now.

To be told when pushes stop, alert on the source's last successful sync: [Monitoring the inboxes](../../deploy/README.md#monitoring-the-inboxes). A push source is never marked stale by OwnHealth itself (it has no schedule), so alert on the time since the last success instead.

## Notes

- One source per person. A shared iPad pushing two people's data cannot be split by OwnHealth.
- The original Apple Health export (`export.zip`) still works as an upload, for history older than what the app sends.
