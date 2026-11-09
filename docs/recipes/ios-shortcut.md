# iOS Shortcut

Share a photo of a lab report, or a PDF from Mail or Files, straight into OwnHealth from an iPhone or iPad. iOS Safari has no web share target, so a Shortcut does the sending. It posts the file with a push token bound to one profile, so the phone needs no login. On Android, install OwnHealth as an app from the browser and it appears in the share sheet by itself.

You build the Shortcut once by hand; it takes about three minutes. We do not publish a signed `.shortcut` file, because a shared Shortcut would have to carry your token or ask for it in a way that is easy to get wrong.

## 1. The source in OwnHealth

1. Open **Inbox**, add a source of kind **iOS Shortcut**.
2. Pick the profile the files belong to. One Shortcut sends to one profile; make one per person.
3. Save, then create the push token. It starts with `oh_ing_` and is shown once. It can only add files to that profile: it cannot read anything.

## 2. The Shortcut

In the Shortcuts app, tap **+** and add:

1. In the details (**i**), switch on **Show in Share Sheet** and keep **Images**, **PDFs** and **Files**.
2. **Convert Image** to **JPEG**. iPhone photos are HEIC, and OwnHealth reads JPEG and PNG. PDFs pass through unchanged.
3. **Get Contents of URL**:
   - URL: `https://health.example.lan/api/ingest/shortcut`
   - Method: **POST**
   - Headers: `Authorization` = `Bearer oh_ing_...` (the token from step 1)
   - Request Body: **Form**, one field of type **File** named `file`, value the converted image (or **Shortcut Input**).
4. **Show Notification** with the **Contents of URL**, so you see the result.

Name it, for example, "OwnHealth: Lucía".

## 3. Use it

From Photos, Files, Mail or Safari, tap **Share** and pick the Shortcut. The file lands in that profile's review queue like an upload. A PDF with a text layer is read at once. A photo goes to OCR with the `ocr` profile on, and otherwise waits for your agent ([agent-assisted extraction](../agent-extraction.md)).

The answer lists each file with its `outcome`:

| outcome | meaning |
|---|---|
| `queued` | stored and being read; it shows in the review queue when it needs you |
| `duplicate` | the same file is already a document of this profile; nothing new is stored |
| `unsupported` | stored, but OwnHealth does not import this file type |

One push holds at most 20 files and 100 MB. The phone must reach OwnHealth, on your LAN or through Tailscale or another VPN.

## If something goes wrong

- **401**: the token is wrong, or the source was switched off or deleted. Make a new token in **Inbox**.
- **403 `wrong_source`**: the token belongs to another kind of source (Paperless or Health Auto Export, for example).
- **409 `no_profile`**: the source has no profile. Edit it in **Inbox** and pick one.
- **413**: the push is over the size limit.

Every push is listed under the source in **Inbox** with its time and result, and a failure counts against the source's health.

## A lost phone

Anyone holding the Shortcut can add files to that profile. Switch off or delete the source in **Inbox** and the token stops working at once. Files it already sent stay.
