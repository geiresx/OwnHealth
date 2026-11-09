# Spikes for v0.2 Inboxes (0.3.0)

Before the lanes started, four questions had to be answered, because each one picks a crate. The spike code lives in `server/spikes/`. Each spike is a standalone cargo project (its own `[workspace]`) so its heavy dependencies stay out of the app's lockfile. Run it with the shared target dir:

```sh
cd server/spikes/<name> && CARGO_TARGET_DIR=../../target cargo run --release -- <args>
```

| Spike | Picks | Answer |
|---|---|---|
| 1. OCR in Rust | `ownhealth-ocr` (lane C) | `oar-ocr` 0.10.2 (ort, onnxruntime 1.28) with PP-OCRv5 mobile detection and Latin mobile recognition |
| 2. IMAP | `ownhealth-inbox::email` (lane A) | `async-imap` 0.12 + `mail-parser` 1.0, TLS through `tokio-rustls` |
| 3. Password PDFs | `ownhealth-inbox::email::pdf_password` (lane A) | pdfium checks the password; `lopdf` 0.47 writes the unlocked copy |
| 4. OAuth 2.1 and CIMD | `ownhealth-auth::oauth`, `ownhealth-mcp::oauth` (lane E) | hand-rolled on the crates we already ship; rmcp's `auth` feature is client-only and is the test client |

## 1. OCR

Code: `spikes/ocr`. It renders five synthetic LabNorte pages (`fixtures/slips`) at 200 dpi, degrades them, and reads each one with every engine. Scans get a slight rotation, blur, noise and JPEG; photos get more of each plus uneven light. The ground truth is each page's own text layer. Word recall and precision compare word multisets; numeric recall counts the words that hold a digit; accented recall counts the words with á é í ó ú ñ ü. 681 ground-truth words, 244 numeric, 49 accented. CPU only, on the MacMini (Apple M4, 10 cores), with the box's load average around 18 from other lanes, so the times are pessimistic.

| Engine and models | Word recall | Precision | Numeric recall | Accented recall | Mean s/page | Max s/page | Models |
|---|---|---|---|---|---|---|---|
| **oar-ocr, PP-OCRv5 mobile det + Latin mobile rec** | **0.899** | **0.933** | **0.898** | 0.53 | **1.93** | 2.07 | 4.6 + 7.7 MiB |
| oar-ocr, PP-OCRv5 server det + server rec | 0.568 | 0.777 | 0.496 | 0.14 | 8.18 | 10.09 | 84 + 81 MiB |
| oar-ocr, PP-OCRv6 small det + small rec | 0.806 | 0.899 | 0.664 | 0.65 | 1.54 | 1.99 | 9.4 + 20 MiB |
| ocrs 0.14 (rten) | 0.558 | 0.655 | 0.422 | 0.00 | 0.99 | 1.56 | 2.5 + 9.7 MB |

What we saw:

- The values are what matter, and the Latin mobile recognizer reads 9 numbers in 10 on degraded pages. On three of the five pages it reads 0.93 or better.
- The server models drop the spaces between words ("Colesterol97mg/dL"), which wrecks word and number recall. A tokenizer fix could recover some of it, but the models are also 13 times bigger and 4 times slower.
- PP-OCRv6 small reads accents better (0.65) and numbers worse (0.66). It exists and is worth a re-measure on the next bump.
- ocrs has no accented characters in its alphabet, so "Triglicéridos" can never come back. That rules it out for Spanish.
- Model load takes 0.11 s. Use one engine per worker, loaded once.
- `ort` with `download-binaries` links onnxruntime statically for x86_64 and aarch64 linux-gnu (the image is Debian trixie, glibc) and aarch64-apple-darwin. GPU stays optional: an execution provider is a cargo feature, and CPU works.

Decision: `oar-ocr = "=0.10.2"` behind the `ownhealth-ocr/engine` feature, which the binary's `ocr` feature turns on. Only the `ocr` image variant builds with it and carries the models. `scripts/fetch-ocr-models.sh <dir>` downloads the three files, pins their sha256 and is idempotent. The models are never committed.

| File | sha256 |
|---|---|
| `pp-ocrv5_mobile_det.onnx` | `1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039` |
| `latin_pp-ocrv5_mobile_rec.onnx` | `e3a6bfeea1c8a01d6fccfd480a0bd363fd907f8c65931e228bb2736f5c3e142f` |
| `ppocrv5_latin_dict.txt` | `ccbcc45730b3fbbd9050c5bc74db6a99067141ef1035e3d14889a84a6b9b1aff` |

Where this differs from the docs: `docs/design/ingestion.md` names PP-StructureV3 and Docling, which are Python. The decision for v0.2 is no Python, so table structure comes from the existing Rust table reader running on OCR words with boxes. Accented recall (0.53) is the weak spot: lane C should normalize accents when matching analyte names, never when storing text.

## 2. IMAP

Code: `spikes/imap`, against GreenMail 2.1.15 in a throwaway container (`docker run --rm -p 3025:3025 -p 3143:3143 greenmail/standalone:2.1.15`).

What we saw:

- GreenMail advertises `MOVE` and `IDLE`.
- We sent a synthetic LabNorte mail with a UTF-8 subject and a PDF attachment over SMTP. `UID FETCH ... BODY.PEEK[]` brought the whole message back and left it unseen.
- `mail-parser` 1.0.1 returned the From address, the decoded subject, the Message-ID, every `Authentication-Results` header (`headers().all(..)` with `raw_value()`), and the attachment through `decoded()`, byte for byte the same as what we sent.
- `CREATE OwnHealth/Done`, `UID STORE +FLAGS (\Seen)` and `UID MOVE` left INBOX at 0 messages and Done at 1, seen. The full round trip took 0.524 s.
- TLS is not proven here, because GreenMail's certificate is self-signed. The plan is `tokio-rustls` 0.26 with `aws_lc_rs` and `webpki-roots`, with port 993 implicit TLS as the default and STARTTLS on 143. A home server with a private CA needs a CA file option. Lane A owns that.

Decision: `async-imap = { version = "0.12", default-features = false, features = ["runtime-tokio"] }`, `mail-parser = "1"`. Lane A's tests use the same GreenMail image as a throwaway container, never a real mailbox.

## 3. Password PDFs

Code: `spikes/pdfpass`. It writes an AES-256 encrypted copy of a LabNorte slip with `lopdf`; `pdfinfo` reports AES-256, V5.

What we saw:

- pdfium opens the file with the right password. With no password, or a wrong one, it fails with `PdfiumLibraryInternalError(PasswordError)`, which we can tell apart from a broken file.
- pdfium cannot save an unlocked copy: pdfium-render calls `FPDF_SaveAsCopy` with flags 0, and `FPDF_REMOVE_SECURITY` would need unsafe code, which the workspace forbids.
- `lopdf` 0.47 can: `Document::load_mem_with_options(bytes, LoadOptions::with_password(pw))`, then remove `/Encrypt` from the trailer, set `encryption_state = None` and save. pdfium opens the result without a password and reads the same words. A wrong password is refused at load.
- `Document::load_mem` followed by `decrypt` wrote a broken 105-byte file. Don't use that path.

Decision: `lopdf = { version = "=0.47.0", default-features = false }`. pdfium decides "encrypted, and is this password right"; lopdf writes the unlocked copy. The unlocked copy becomes a new document with `derived_from` pointing at the locked original. The locked original stays as the source of truth, and the password stays sealed in `secret`.

## 4. OAuth 2.1 and CIMD

What we checked:

- rmcp 3.9's `auth` feature is client-side only: discovery, PKCE, a token store, and `client_id_metadata_document_supported` for CIMD. It is useful as the client in lane E's end-to-end tests. Nothing on the server side validates tokens, so the server keeps doing that in axum middleware (the existing `pat_guard`, extended for `kind = 'oauth'`).
- Tokens: opaque, hashed and stored in `api_token` like PATs (`kind`, `audience`, `oauth_client_id`, `consent_id`), so revocation and expiry come for free and no JWT library is needed.
- PKCE S256 is `sha2` + `base64`, already dependencies; the RFC 7636 appendix B vector is a unit test in `ownhealth-auth::oauth`.
- CIMD fetch goes through `ownhealth-core::net::get` with `Egress::cimd()`: https only, public addresses only (the DNS answer is checked and pinned with `resolve_to_addrs`, so a rebind cannot reach the LAN), no redirects, 5 s and 64 KiB caps.

Decision: hand-roll it in `ownhealth-auth::oauth` (tokens, codes, consent, CIMD validation) and `ownhealth-mcp::oauth` (the RFC 8414 and RFC 9728 metadata, `/oauth/authorize`, `/oauth/token`). No new crypto dependency.

## 5. Near-duplicate thresholds (foundation, not a spike)

These are measured on synthetic LabNorte pages with 30 rows (`dedup` unit tests):

- Text simhash over word pairs, where a pair ending in a number weighs 8 times a template pair. A changed footer timestamp moves 0 to 2 bits. Another report on the same template with other values moves 8 to 20 bits. Threshold: 4. At equal weights, other reports came within 2 to 9 bits, so the template alone made them look like duplicates.
- On a short page the footer weighs more and a re-download can be missed. That is harmless, because level 3 still matches the facts.
- pHash (DCT, 64 bits) on rendered pages: threshold 6.
