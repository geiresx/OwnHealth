#!/usr/bin/env bash
# Fetch the pinned PDFium shared library (bblanchon/pdfium-binaries) for this platform, check its
# sha256, and unpack it into server/vendor/pdfium/. The Rust PDF reader (pdfium-render) loads
# lib/libpdfium.{so,dylib} from there at runtime, or from OWNHEALTH_PDFIUM_LIB.
#
#   scripts/fetch-pdfium.sh            # this machine
#   scripts/fetch-pdfium.sh linux-x64  # another platform (Docker build stage)
#
# Bump the version: change RELEASE and the four sums together (the release page lists each asset's
# sha256), and the pdfium-render feature `pdfium_<RELEASE>` in server/Cargo.toml.
set -euo pipefail

RELEASE=8125
sha256_for() { # bash 3 (stock macOS) has no associative arrays
  case "$1" in
    mac-arm64) echo 4939bd935ac83ddc09a3a4f038bc43a19cea1660316a3667f26cb07a070e139d ;;
    mac-x64) echo 8a5193535b80c7215d8932f6647ae5097465b6dba2811602c21e120c2af112b0 ;;
    linux-x64) echo e1c3ce6e270cb5e0c20816b379ad9f2c1309972697c505f508de92bf5e7d6659 ;;
    linux-arm64) echo 8b8818f2ec57d1b213d952ca6d7f0ffeca9d0110487d0e20ad83ba918bc92efa ;;
  esac
}

platform=${1:-}
if [[ -z "$platform" ]]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=mac-arm64 ;;
    Darwin-x86_64) platform=mac-x64 ;;
    Linux-x86_64) platform=linux-x64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
    *) echo "no pinned PDFium build for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
fi
want=$(sha256_for "$platform")
[[ -n "$want" ]] || { echo "unknown platform $platform (want mac-arm64, mac-x64, linux-x64 or linux-arm64)" >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/server/vendor/pdfium/$platform"
if [[ -f "$dest/VERSION" ]] && grep -qx "chromium/$RELEASE $want" "$dest/VERSION"; then
  echo "pdfium chromium/$RELEASE ($platform) already in $dest"
  exit 0
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
url="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/$RELEASE/pdfium-$platform.tgz"
curl -fsSL --retry 3 -o "$work/pdfium.tgz" "$url"
if command -v sha256sum >/dev/null; then got=$(sha256sum "$work/pdfium.tgz" | cut -d' ' -f1); else got=$(shasum -a 256 "$work/pdfium.tgz" | cut -d' ' -f1); fi
if [[ "$got" != "$want" ]]; then
  echo "sha256 mismatch for $url: got $got, want $want" >&2
  exit 1
fi
mkdir "$work/x"
tar -xzf "$work/pdfium.tgz" -C "$work/x"
rm -rf "$dest"
mkdir -p "$dest"
cp -R "$work/x/lib" "$work/x/LICENSE" "$dest/"
[[ -d "$work/x/licenses" ]] && cp -R "$work/x/licenses" "$dest/"
echo "chromium/$RELEASE $want" > "$dest/VERSION"
echo "pdfium chromium/$RELEASE ($platform) -> $dest/lib"
