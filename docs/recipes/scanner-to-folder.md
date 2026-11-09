# Scanner to folder

A scanner that saves to a network folder, or a phone app that syncs a folder, can feed OwnHealth. Drop a file into the folder named after a profile and it is imported for that person.

## How it works

OwnHealth watches one directory, `OWNHEALTH_INBOX_DIR` (`/inbox` in the container, `./inbox` next to `compose.yaml` on the host):

```text
inbox/
  alex/          files for Alex's profile (the folder name is the profile's slug)
  lucia/         files for Lucía's profile
  report.pdf     a file in the root goes to the source's default profile
  .done/alex/    imported files are moved here
  .failed/alex/  files that could not be imported, each with a <name>.reason.txt
```

- It looks every 60 seconds. It polls instead of waiting for change events, because those are unreliable on SMB and NFS shares.
- A file is taken once nothing has written to it for 10 seconds, so a half-written scan is never read.
- PDFs and images (JPEG, PNG) go through the same path as an upload. HEIC photos are stored but not read: have the phone app save JPEG. A PDF with a text layer is read at once. A scan or photo goes to OCR when the `ocr` profile is on ([below](#scans-and-ocr)), and waits for your agent otherwise.
- The same file dropped twice is imported once.
- Hidden files, partial downloads (`.part`, `.crdownload`, `.tmp`, `~$...`) and symbolic links are skipped.

## 1. The folder

Both compose files mount `./inbox` into the app as `/inbox` and set `OWNHEALTH_INBOX_DIR=/inbox`. The app runs as user 10001 and moves files after import, so it needs write access:

```sh
cd /opt/ownhealth
mkdir -p inbox/alex inbox/lucia
sudo chown -R 10001:10001 inbox
sudo chmod -R 2775 inbox          # group-writable, new files keep the group
```

To let your scanner's user write too, add it to group 10001, or share the folder with that group (Samba `force group`).

## 2. Share it with the scanner

Most network scanners save to an SMB share. A minimal Samba share on the OwnHealth host:

```ini
[health-inbox]
   path = /opt/ownhealth/inbox
   valid users = scanner
   force group = 10001
   create mask = 0664
   directory mask = 2775
   writable = yes
```

Point the scanner's "scan to network folder" at `\\<host>\health-inbox\alex`. Keep the share on your LAN. These files are medical records.

For a phone, a sync tool that writes into `inbox/<profile>/` (Syncthing, for example) works the same way.

## 3. The folder inbox in OwnHealth

In OwnHealth, open **Inbox** and add a source of kind **Watched folder**. Pick the default profile for files in the root. To watch a subfolder of `/inbox` instead of the whole folder, give its relative path.

## 4. Check it

Copy a lab PDF into `inbox/alex/`. Within about a minute and a quarter (60 s poll plus 10 s settle) it moves to `inbox/.done/alex/` and shows in the review queue.

If it moves to `.failed/` instead, read the `.reason.txt` next to it. If it stays where it is, check in **Inbox** that the source is on and look at its last error. An error saying `OWNHEALTH_INBOX_DIR is not set` means the compose file lacks the variable.

## Scans and OCR

A flatbed scan has no text layer. You have two ways to turn it into values:

- **The `ocr` profile.** Add `COMPOSE_PROFILES=ocr` and `OWNHEALTH_OCR=1` to `.env`, then `docker compose up -d`. An extra `ocr` worker reads scans on the CPU. It took under a second a page on the Mac mini we measured on ([benchmark](../ocr-benchmark.md)). Every value it reads still goes to the review queue, and a value that is not on the page's words is flagged.
- **Your own agent.** Without the profile, scans wait as pending documents. An agent such as Claude Code reads them through MCP and proposes the values: [agent-assisted extraction](../agent-extraction.md).

Scan to PDF or JPEG (TIFF is not read), at 200 to 300 dpi, in grayscale, one report per file.
