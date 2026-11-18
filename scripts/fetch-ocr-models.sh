#!/usr/bin/env bash
# Fetch the pinned OCR models (PaddleOCR PP-OCRv5 mobile detection + Latin recognition, ONNX, as
# published by oar-ocr) and check each sha256. The models are never committed; only the `ocr` image
# variant carries them. The app reads them from OWNHEALTH_OCR_MODELS_DIR.
#
#   scripts/fetch-ocr-models.sh                       # into server/vendor/ocr-models/
#   scripts/fetch-ocr-models.sh /opt/ownhealth/ocr-models   # the image's default directory
#
# The licence texts the `ocr` image must ship go to a second directory (default: <models>-licenses),
# one folder per project: PaddleOCR's (Apache-2.0, the models) and ONNX Runtime's (MIT, linked
# statically into the `ocr` binary by ort-sys; its download carries no licence file). Kept out of the
# models directory so it holds only the pinned model files. ONNX Runtime's tag follows the version
# ort-sys downloads (ort-sys 2.0.0-rc.13: 1.28.0): bump it with ort.
# ONNX Runtime's texts come through jsDelivr's copy of the tag, because github.com/microsoft/onnxruntime
# answers 404 since 2026-11-17; the pinned sums are the ones the GitHub tag served.
#
# Why these models and not the server ones or ocrs: server/SPIKES-v0.2.md section 1. Bump: change a
# URL and its sum together, and ENGINE_VERSION in crates/ownhealth-ocr.
set -euo pipefail

BASE=https://github.com/GreatV/oar-ocr/releases/download/v0.3.0
FILES="pp-ocrv5_mobile_det.onnx:1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039
latin_pp-ocrv5_mobile_rec.onnx:e3a6bfeea1c8a01d6fccfd480a0bd363fd907f8c65931e228bb2736f5c3e142f
ppocrv5_latin_dict.txt:ccbcc45730b3fbbd9050c5bc74db6a99067141ef1035e3d14889a84a6b9b1aff"
# <folder>/<file> <url> <sha256>
LICENSES="paddleocr/LICENSE https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/v3.0.0/LICENSE 3840c5c0c61c294264d2dd77b8777be6ddd90121ef4e0e64abcd22edea581d6e
onnxruntime/LICENSE https://cdn.jsdelivr.net/gh/microsoft/onnxruntime@v1.28.0/LICENSE 2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c
onnxruntime/ThirdPartyNotices.txt https://cdn.jsdelivr.net/gh/microsoft/onnxruntime@v1.28.0/ThirdPartyNotices.txt 0e07b95f3a8d6230037707c5c4a2b554d12c4cb67369669ac255635528ffcee2"

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/server/vendor/ocr-models}
licenses=${2:-${dest%/}-licenses}
mkdir -p "$dest"

sum() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

# fetch <url> <path> <sha256>: download once, keep only a file with the pinned sum
fetch() {
  local url=$1 path=$2 want=$3
  if [[ -f "$path" && "$(sum "$path")" == "$want" ]]; then
    echo "ok      $path"
    return
  fi
  mkdir -p "$(dirname "$path")"
  local tmp
  tmp="$(dirname "$path")/.$(basename "$path").part"
  curl -fsSL --retry 3 -o "$tmp" "$url"
  local got
  got=$(sum "$tmp")
  if [[ "$got" != "$want" ]]; then
    rm -f "$tmp"
    echo "sha256 mismatch for $url: got $got, want $want" >&2
    exit 1
  fi
  mv "$tmp" "$path"
  echo "fetched $path"
}

for entry in $FILES; do
  fetch "$BASE/${entry%%:*}" "$dest/${entry%%:*}" "${entry##*:}"
done
while read -r name url want; do
  fetch "$url" "$licenses/$name" "$want"
done <<<"$LICENSES"
echo "OCR models in $dest, their licences in $licenses"
