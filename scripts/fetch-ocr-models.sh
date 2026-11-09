#!/usr/bin/env bash
# Fetch the pinned OCR models (PaddleOCR PP-OCRv5 mobile detection + Latin recognition, ONNX, as
# published by oar-ocr) and check each sha256. The models are never committed; only the `ocr` image
# variant carries them. The app reads them from OWNHEALTH_OCR_MODELS_DIR.
#
#   scripts/fetch-ocr-models.sh                       # into server/vendor/ocr-models/
#   scripts/fetch-ocr-models.sh /opt/ownhealth/ocr-models   # the image's default directory
#
# Why these models and not the server ones or ocrs: server/SPIKES-v0.2.md section 1. Bump: change a
# URL and its sum together, and ENGINE_VERSION in crates/ownhealth-ocr.
set -euo pipefail

BASE=https://github.com/GreatV/oar-ocr/releases/download/v0.3.0
FILES="pp-ocrv5_mobile_det.onnx:1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039
latin_pp-ocrv5_mobile_rec.onnx:e3a6bfeea1c8a01d6fccfd480a0bd363fd907f8c65931e228bb2736f5c3e142f
ppocrv5_latin_dict.txt:ccbcc45730b3fbbd9050c5bc74db6a99067141ef1035e3d14889a84a6b9b1aff"

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/server/vendor/ocr-models}
mkdir -p "$dest"

sum() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

for entry in $FILES; do
  name=${entry%%:*}
  want=${entry##*:}
  if [[ -f "$dest/$name" && "$(sum "$dest/$name")" == "$want" ]]; then
    echo "ok      $name"
    continue
  fi
  tmp="$dest/.$name.part"
  curl -fsSL --retry 3 -o "$tmp" "$BASE/$name"
  got=$(sum "$tmp")
  if [[ "$got" != "$want" ]]; then
    rm -f "$tmp"
    echo "sha256 mismatch for $BASE/$name: got $got, want $want" >&2
    exit 1
  fi
  mv "$tmp" "$dest/$name"
  echo "fetched $name"
done
echo "OCR models in $dest"
