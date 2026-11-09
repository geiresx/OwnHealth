#!/usr/bin/env bash
# Smoke-test one built app image on this machine's architecture: the non-root user, the CLI
# version, every tool the deploy recipes call, PDFium's shared libraries, then a demo run against
# Postgres 17 (readyz, the embedded UI, healthcheck, backup, verify-restore).
#
# The `ocr` variant also gets: the pinned models with the sums scripts/fetch-ocr-models.sh pins, and
# an `ocr` worker (`ownhealth worker`, OWNHEALTH_QUEUES=ocr) beside the app with OWNHEALTH_OCR=1, which
# must read the demo's scanned page: every OCR job done, none failed. The default image must carry
# no models.
#
#   scripts/smoke-image.sh drumsergio/ownhealth:ci                  # CI, after docker build
#   scripts/smoke-image.sh drumsergio/ownhealth:ci-ocr ocr          # CI, after docker build --target ocr
#   scripts/smoke-image.sh drumsergio/ownhealth@sha256:<digest> [ocr] # release, the pushed platform image
set -euo pipefail

img=${1:?usage: smoke-image.sh <image> [default|ocr]}
variant=${2:-default}
root=$(cd "$(dirname "$0")/.." && pwd)
net=oh-smoke-$$
db=oh-smoke-db-$$
app=oh-smoke-app-$$
worker=oh-smoke-ocr-$$
# /data is shared by the app and the worker, as the compose volume shares it: the worker reads the
# blobs the app stored
vol=oh-smoke-data-$$
port=${SMOKE_PORT:-18080}
models=/opt/ownhealth/ocr-models
case "$variant" in default | ocr) ;; *) echo "variant must be default or ocr, got $variant" >&2; exit 2 ;; esac

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    docker logs "$app" 2>&1 | tail -100 || true
    docker logs "$worker" 2>&1 | tail -50 || true
  fi
  docker rm -f "$worker" "$app" "$db" >/dev/null 2>&1 || true
  docker volume rm "$vol" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/server/Cargo.toml" | head -1)
test "$(docker run --rm --entrypoint id "$img" -u)" = "10001"
docker run --rm "$img" ownhealth --version | grep -qx "ownhealth $version"
docker run --rm "$img" pg_dump --version | grep -q "PostgreSQL) 17\."
docker run --rm "$img" psql --version | grep -q "PostgreSQL) 17\."
docker run --rm "$img" restic version
docker run --rm "$img" test -f /opt/ownhealth-db/app-role.sh
# PDFium and the binary load: every library they link against is in the image
for lib in /usr/local/lib/libpdfium.so /usr/local/bin/ownhealth; do
  libs=$(docker run --rm --entrypoint ldd "$img" "$lib")
  if grep "not found" <<<"$libs"; then exit 1; fi
done

if [ "$variant" = ocr ]; then
  # every pinned model, byte for byte
  pinned=$(sed -n 's/^\(FILES="\)\{0,1\}\([A-Za-z0-9_.-]*\):\([0-9a-f]\{64\}\)"\{0,1\}$/\3  \2/p' "$root/scripts/fetch-ocr-models.sh")
  test -n "$pinned"
  got=$(docker run --rm --entrypoint sh "$img" -c "cd $models && sha256sum *")
  diff <(sort <<<"$pinned") <(sort <<<"$got")
  docker run --rm --entrypoint printenv "$img" OWNHEALTH_OCR_MODELS_DIR | grep -qx "$models"
else
  if docker run --rm --entrypoint test "$img" -e "$models"; then
    echo "the default image carries OCR models" >&2
    exit 1
  fi
fi

docker network create "$net" >/dev/null
docker run -d --name "$db" --network "$net" -e POSTGRES_PASSWORD=ci postgres:17.11 >/dev/null
for _ in $(seq 1 60); do docker exec "$db" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
db_url="postgresql://postgres:ci@$db:5432/postgres"
ocr_flag=""
if [ "$variant" = ocr ]; then ocr_flag=1; fi
docker run -d --name "$app" --network "$net" -v "$vol:/data" -p "127.0.0.1:$port:8080" \
  -e OWNHEALTH_DATABASE_URL="$db_url" \
  -e OWNHEALTH_SECRET_KEY="$(openssl rand -hex 48)" \
  -e OWNHEALTH_OCR="$ocr_flag" \
  -e OWNHEALTH_DEMO=1 "$img" >/dev/null
for _ in $(seq 1 90); do curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://127.0.0.1:$port/readyz"
echo
# captured first: grep -q stops reading early, and under pipefail the writer's SIGPIPE fails the step
page=$(curl -fsS "http://127.0.0.1:$port/")
grep -qi "<!doctype html" <<<"$page"
docker exec "$app" ownhealth healthcheck

if [ "$variant" = ocr ]; then
  # the app migrated and seeded; the worker only runs `ocr` jobs
  docker run -d --name "$worker" --network "$net" -v "$vol:/data" \
    -e OWNHEALTH_DATABASE_URL="$db_url" \
    -e OWNHEALTH_RUN_MIGRATIONS=0 \
    -e OWNHEALTH_QUEUES=ocr "$img" ownhealth worker >/dev/null
  sql() { docker exec "$db" psql -U postgres -tAc "$1"; }
  done_jobs=0
  for _ in $(seq 1 180); do
    open=$(sql "select count(*) from job where queue = 'ocr' and state in ('queued', 'running')")
    done_jobs=$(sql "select count(*) from job where queue = 'ocr' and state = 'done'")
    if [ "$open" = 0 ] && [ "$done_jobs" -gt 0 ]; then break; fi
    sleep 1
  done
  test "$(docker inspect -f '{{.State.Running}}' "$worker")" = true
  failed=$(sql "select count(*) from job where queue = 'ocr' and state not in ('queued', 'running', 'done')")
  test "$failed" = 0 || { sql "select kind, state, last_error from job where queue = 'ocr'"; exit 1; }
  test "$done_jobs" -gt 0 || { echo "no OCR job finished" >&2; exit 1; }
  test "$(sql "select count(*) from document where ocr_state = 'done'")" -gt 0
  test "$(sql "select count(*) from document where ocr_state = 'failed'")" = 0
  echo "ocr ok: $done_jobs job(s)"
fi

docker exec "$app" ownhealth backup /data/backups/smoke >/dev/null
report=$(docker exec "$app" ownhealth verify-restore /data/backups/smoke)
grep -q '"ok": true' <<<"$report"
echo "smoke ok: $img ($variant, $(docker run --rm --entrypoint uname "$img" -m))"
