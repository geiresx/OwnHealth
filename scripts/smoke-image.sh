#!/usr/bin/env bash
# Smoke-test one built app image on this machine's architecture: the non-root user, the CLI
# version, every tool the deploy recipes call, PDFium's shared libraries, then a demo run against
# Postgres 17 (readyz, the embedded UI, healthcheck, backup, verify-restore).
#
#   scripts/smoke-image.sh drumsergio/ownhealth:ci             # CI, after docker build
#   scripts/smoke-image.sh drumsergio/ownhealth@sha256:<digest> # release, the pushed platform image
set -euo pipefail

img=${1:?usage: smoke-image.sh <image>}
root=$(cd "$(dirname "$0")/.." && pwd)
net=oh-smoke-$$
db=oh-smoke-db-$$
app=oh-smoke-app-$$
port=${SMOKE_PORT:-18080}

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then docker logs "$app" 2>&1 | tail -100 || true; fi
  docker rm -f "$app" "$db" >/dev/null 2>&1 || true
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
# PDFium loads: every library it links against is in the image
libs=$(docker run --rm --entrypoint ldd "$img" /usr/local/lib/libpdfium.so)
if grep "not found" <<<"$libs"; then exit 1; fi

docker network create "$net" >/dev/null
docker run -d --name "$db" --network "$net" -e POSTGRES_PASSWORD=ci postgres:17.11 >/dev/null
for _ in $(seq 1 60); do docker exec "$db" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker run -d --name "$app" --network "$net" -p "127.0.0.1:$port:8080" \
  -e OWNHEALTH_DATABASE_URL="postgresql://postgres:ci@$db:5432/postgres" \
  -e OWNHEALTH_SECRET_KEY="$(openssl rand -hex 48)" \
  -e OWNHEALTH_DEMO=1 "$img" >/dev/null
for _ in $(seq 1 90); do curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "http://127.0.0.1:$port/readyz"
echo
# captured first: grep -q stops reading early, and under pipefail the writer's SIGPIPE fails the step
page=$(curl -fsS "http://127.0.0.1:$port/")
grep -qi "<!doctype html" <<<"$page"
docker exec "$app" ownhealth healthcheck
docker exec "$app" ownhealth backup /data/backups/smoke >/dev/null
report=$(docker exec "$app" ownhealth verify-restore /data/backups/smoke)
grep -q '"ok": true' <<<"$report"
echo "smoke ok: $img ($(docker run --rm --entrypoint uname "$img" -m))"
