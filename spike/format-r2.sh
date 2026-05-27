#!/usr/bin/env bash
# Format a JuiceFS volume on Cloudflare R2 + Postgres metadata, then run the
# Rust FFI proof against it.
#
# R2 S3 credentials come from a DEDICATED R2 API token (Cloudflare dashboard:
# R2 -> Manage R2 API Tokens -> Create), which yields an Access Key ID + Secret
# Access Key. A general account API token (even with Workers R2 Storage:Edit)
# does NOT work as an S3 credential — Cloudflare only binds S3 creds to R2 API
# tokens. Set in the environment (e.g. via ~/.secret_env / 1Password):
#
#   export R2_ACCESS_KEY_ID=...
#   export R2_SECRET_ACCESS_KEY=...
#   export CLOUDFLARE_ACCOUNT_ID=...   # already set from ~/.secret_env
#   export R2_BUCKET=trove             # optional, defaults to "trove"
#
# Run:  ! cd ~/code/trove/spike && ./format-r2.sh
set -euo pipefail

: "${R2_ACCESS_KEY_ID:?set R2_ACCESS_KEY_ID (from a dedicated R2 API token)}"
: "${R2_SECRET_ACCESS_KEY:?set R2_SECRET_ACCESS_KEY}"
: "${CLOUDFLARE_ACCOUNT_ID:?set CLOUDFLARE_ACCOUNT_ID}"
BUCKET="${R2_BUCKET:-trove}"

ENDPOINT="https://${BUCKET}.${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"
JFS=/home/cp/code/trove/spike/juicefs/juicefs
META="postgres://cp:spike@127.0.0.1:5432/trove_r2_spike?sslmode=disable"
VOL="trover2"

createdb trove_r2_spike 2>/dev/null || echo "(db trove_r2_spike already exists)"

echo "→ formatting JuiceFS volume '$VOL' on R2 ($ENDPOINT)"
"$JFS" format --storage s3 --bucket "$ENDPOINT" \
  --access-key "$R2_ACCESS_KEY_ID" --secret-key "$R2_SECRET_ACCESS_KEY" \
  "$META" "$VOL"

echo "→ running Rust FFI proof against the R2-backed volume"
cd /home/cp/code/trove/spike/ffi-proof
TROVE_META="$META" TROVE_VOL="$VOL" cargo run 2>&1 | grep -v "<INFO>\|<WARNING>"
