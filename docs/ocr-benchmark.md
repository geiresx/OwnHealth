# OCR benchmark

This is how well the `ocr` image reads scanned and photographed lab slips, and how fast it does it on
a CPU. It covers the v0.2 OCR exit criteria in [ROADMAP.md](ROADMAP.md): row F1 of at least 0.90 on
the scanned and photographed eval set, CPU throughput measured and published, and nothing silent.

## Result

Measured on 2026-11-09 on an Apple M4 (10 cores, CPU only, 4 ONNX Runtime threads), with a load
average of about 5 from other work on the machine.

| Engine | Set | Pages | Row F1 | Precision | Recall | Mean s/page | Max s/page | Pages/s | Accepted | Review | Pending page | Dropped | Duplicated |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| pp-ocrv5-mobile-det+latin-mobile-rec@oar-ocr-0.10.2 | scan | 39 | 0.971 | 0.991 | 0.952 | 0.81 | 1.20 | 1.24 | 21 | 562 | 24 | 0 | 0 |
| pp-ocrv5-mobile-det+latin-mobile-rec@oar-ocr-0.10.2 | photo | 15 | 0.906 | 0.906 | 0.906 | 0.80 | 1.15 | 1.25 | 15 | 209 | 0 | 0 | 0 |
| pp-ocrv5-mobile-det+latin-mobile-rec@oar-ocr-0.10.2 | all | 54 | 0.954 | 0.968 | 0.940 | 0.80 | 1.20 | 1.24 | 36 | 771 | 24 | 0 | 0 |

- **Row F1 0.954** over all 831 ground-truth rows (0.971 on scans, 0.906 on photos). The target is 0.90.
- **Under a second per page** on CPU, with 1.2 s the slowest page. The target was under 60 s per page.
- **Nothing silent.** Every ground-truth row has exactly one outcome: 0 dropped and 0 duplicated.
  807 rows were read and routed (36 committed without a click, 771 waiting in the review queue). The 24 rows
  of one page wait together, because OCR found no table on that page (see below).
- **No wrong value committed.** No auto-accepted row holds a number other than the printed one.
  Because OCR rows need a matched vendor template and a row confidence of 0.98 or more to skip
  review, most rows go to review. That is by design.

The time is engine time per page: rendering at 200 dpi, text detection and recognition. Going
through the import and the review routing adds a few milliseconds per row.

## What the set is

The generator lives in `server/crates/ownhealth-ocr/tests/eval/mod.rs` and is deterministic. The
images are never committed.

- **Slips.** Every synthetic layout in four languages (es, en, de, fr) with every kind of table in
  the lab-slip fixtures: basic, flag styles, bold sections, wrapped names, two columns, several pages,
  the unit inside the value, range first. The three-page LabNorte report is also in the set. All
  people and labs are synthetic.
- **Scans.** Every page is rendered at 200 dpi and degraded the way a flatbed scanner would: a tilt
  of 0.5 to 1.2 degrees, blur of σ 0.7, noise of ±8 and JPEG at quality 75. The pages are stored as
  an image-only PDF. This gives 33 documents and 39 pages.
- **Photos.** One JPEG per page, for every third slip and every page of the LabNorte report. Each
  photo has a tilt of 2.5 to 3.5 degrees, blur of σ 1.1, noise of ±14, uneven light, a warm cast
  and JPEG at quality 55. This gives 15 pages.
- **Ground truth.** Every row carries its page, printed name, printed value and box. A row counts
  as read (row F1) when page, name and value match. Its outcome comes from the one candidate row
  whose box holds the truth row's centre.

## How OCR works

1. `import.pdf` reads the text layer first. A page goes to OCR only when it has no usable text layer
   or its glyphs are garbage. Every image goes to OCR too. A page with a good text layer is never
   read again (`route::needs_ocr`).
2. `ocr.document` renders the page at 200 dpi and runs PP-OCRv5 mobile detection and Latin mobile
   recognition on ONNX Runtime. It turns every detected line into words with boxes in PDF points
   (`engine::words_from_lines`). Two corrections were needed:
   - **Skew.** A scan tilted by 1 degree moves the right end of a row 10 pt. The engine measures the
     skew from the long lines and turns the words back. The short boxes are left out of the
     measurement, because the detector draws them axis-aligned.
   - **Margin.** The detector grows every box by a margin, so 9 pt text gets a 15.5 pt box. The engine
     takes the margin off, because otherwise the boxes of neighbouring rows overlap and the table
     reader merges them.
3. The words go through the same table reader, normaliser and routing as a text layer. Two checks are
   added per row:
   - the recognizer's confidence in the row's words caps the row's confidence;
   - the value must be on the page's OCR words inside the row (`grounding::ground`), or the row goes
     to review flagged `not_grounded`;
   - the row is read together with the lines just above and below it. If they hold
     instruction-like text, the row goes to review flagged `possible_injection`. An instruction
     printed into an image usually runs over several lines, and each line alone looks harmless.
   OCR rows are `ocr_template` (auto-accept only at 0.98) or `ocr_rules` (never auto-accept).
4. A page OCR cannot read, or where it finds no table, waits as one `pending_document` item for an
   outside agent or a person. With the `ocr` profile off, every scanned page waits that way. The
   `ocr_off` test checks this over the whole eval set.

## Known gaps

- **One scanned page found no table** (`es-05-two-column`). The two-column header runs to the page
  edge, and after the tilt the last letter of "referencia" falls outside the image. The table reader
  does not take the shortened header, so the page waits whole as a `pending_document`. Its rows are
  in the review queue through that page and are not dropped. A real scanner cuts edges the same way.
- **Photos lose about 1 row in 10.** A misread name or value means the row does not count as read
  in the F1. It is still routed to review as a row, so it is not lost. The spike measured the weak
  spot: accented letters (recall 0.53). Rows are matched on folded names, so an accent alone does
  not cost a row.
- **One engine.** Only the shipped engine runs in this harness. The OCR spike ([server/SPIKES-v0.2.md](../server/SPIKES-v0.2.md))
  compared the other candidates on word recall: PP-OCRv5 server, PP-OCRv6 small and ocrs. To rank
  another model set on row F1, put its files in a models directory under the names
  `scripts/fetch-ocr-models.sh` uses and run the harness on that directory.
- **Fixture font.** The generator's embedded Vera font draws no accented letters, so a picture of a
  Vera page shows an empty box where "é" should be. The eval draws those layouts in Helvetica. This
  was reported to the fixture owner.

## Run it

The test database comes from `scripts/test-db.sh`, and the models from `scripts/fetch-ocr-models.sh`
(pinned and sha256-checked).

```sh
bash scripts/test-db.sh
bash scripts/fetch-ocr-models.sh /tmp/ocr-models
cd server
# the table above
OWNHEALTH_OCR_MODELS_DIR=/tmp/ocr-models cargo run --release -p ownhealth-ocr --features engine --example bench
# the exit-criteria gate: F1 >= 0.90, 0 dropped, 0 duplicated, 0 wrong auto-accepts, both OCR-only
# injection fixtures held
OWNHEALTH_OCR_MODELS_DIR=/tmp/ocr-models \
  cargo test --release -p ownhealth-ocr --features engine --test exit_criteria -- --ignored --nocapture
# with the ocr profile off: every eval page waits for an agent (no models needed)
cargo test -p ownhealth-ocr --test ocr_off
```
