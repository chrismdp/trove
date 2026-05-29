#!/usr/bin/env bash
# Full containerised end-to-end test.
#
# Brings up MinIO (S3) + Postgres/pgvector + two trove instances that mount the
# SAME vault, then asserts a write in one instance is visible (byte-identical)
# in the other — both directions. Proves the live-projection / attach / fleet
# model over a real object store + metadata DB.
#
# Runs the same locally (`bash scripts/e2e.sh`) and in CI (.github/workflows/e2e.yml).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
COMPOSE="docker compose -f e2e/compose.yaml -p trove-e2e"

cleanup() { $COMPOSE down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "== build libjfs + trove (release) =="
[ -f libjfs/build/libjfs-amd64.so ] || ./libjfs/build.sh
cargo build --release --features mount

echo "== build runtime image =="
STAGE="$(mktemp -d)"
cp target/release/trove "$STAGE/trove"
cp libjfs/build/libjfs-amd64.so "$STAGE/libjfs-amd64.so"
cp e2e/docker-entrypoint.sh "$STAGE/docker-entrypoint.sh"
cp e2e/Dockerfile "$STAGE/Dockerfile"
docker build -t trove-e2e:local "$STAGE"
rm -rf "$STAGE"

echo "== bring up the stack (A creates, B attaches) =="
$COMPOSE up -d

wait_mounted() { # $1 = service
  for i in $(seq 1 60); do
    if $COMPOSE exec -T "$1" sh -c 'grep -q " /vault/notes fuse" /proc/mounts' 2>/dev/null; then
      echo "  $1 mounted (${i}s)"; return 0
    fi
    sleep 2
  done
  echo "  $1 never mounted; logs:"; $COMPOSE logs "$1"; return 1
}
wait_mounted trove-a
wait_mounted trove-b

# Write in $1, expect content in $2 within the timeout.
assert_sync() { # $1=writer $2=reader $3=file $4=msg
  echo "== $1 -> $2 ($3) =="
  $COMPOSE exec -T "$1" sh -c "printf '%s\n' '$4' > /vault/notes/$3"
  for i in $(seq 1 25); do
    got="$($COMPOSE exec -T "$2" sh -c "cat /vault/notes/$3 2>/dev/null" | tr -d '\r\n')"
    if [ "$got" = "$4" ]; then echo "  $2 sees it (${i}s)"; return 0; fi
    sleep 1
  done
  echo "  FAIL: $2 never saw $3 (last read: '${got:-}')"; return 1
}

ts="$(date -u +%s)"
assert_sync trove-a trove-b from_a.md "hello-from-A-$ts"
assert_sync trove-b trove-a from_b.md "hello-from-B-$ts"

echo
echo "E2E PASS — two trove instances share one vault and sync both ways."
