#!/usr/bin/env bash
# Run the contract tests against the server on a fresh database (the goldens were recorded from the
# Python 0.1.1 backend, which passed the same suite before it was removed in 0.2.0):
#   contract-tests/run.sh rust       # server/ (cargo run -p ownhealth -- serve)
# Extra arguments go to the test binaries (e.g. a test name filter). Every binary runs even when one fails.
# The database lives in the shared test Postgres (scripts/test-db.sh) and is dropped on exit.
set -euo pipefail

which=${1:?usage: run.sh rust [cargo test args]}
shift
root=$(cd "$(dirname "$0")/.." && pwd)
admin=${OWNHEALTH_TEST_ADMIN_URL:-postgresql://postgres:postgres@127.0.0.1:55432/postgres}
base=${admin%/*}
db="oh_contract_$(openssl rand -hex 6)"
port=${OWNHEALTH_CONTRACT_PORT:-$((18000 + RANDOM % 1000))}
data=$(mktemp -d)
pid=""

psql_admin() { docker exec ownhealth-testpg psql -U postgres -qAtc "$1" >/dev/null; }

cleanup() {
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  psql_admin "drop database if exists \"$db\" with (force)" || true
  rm -rf "$data"
}
trap cleanup EXIT

bash "$root/scripts/test-db.sh" >/dev/null
psql_admin "create database \"$db\""

export OWNHEALTH_DATABASE_URL="$base/$db"
export OWNHEALTH_DATA_DIR="$data"
export OWNHEALTH_DEMO=1 OWNHEALTH_DEV_AUTH=1 OWNHEALTH_RUN_JOBS=1
export OWNHEALTH_HOST=127.0.0.1 OWNHEALTH_PORT="$port"
export OWNHEALTH_BASE_URL="http://127.0.0.1:$port"
export OWNHEALTH_SECRET_KEY="$(openssl rand -hex 48)"

case "$which" in
  rust)
    (cd "$root/server" && cargo build -q -p ownhealth)
    "$root/server/target/debug/ownhealth" serve &
    ;;
  *) echo "usage: run.sh rust" >&2; exit 2 ;;
esac
pid=$!

for _ in $(seq 1 240); do
  if curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1; then break; fi
  kill -0 "$pid" 2>/dev/null || { echo "the $which backend exited before it was ready" >&2; exit 1; }
  sleep 0.5
done
curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null || { echo "the $which backend never became ready" >&2; exit 1; }

export OWNHEALTH_CONTRACT_BASE_URL="http://127.0.0.1:$port"
cd "$root/server"
cargo test -q --no-fail-fast -p ownhealth-contract-tests -- --include-ignored "$@"
