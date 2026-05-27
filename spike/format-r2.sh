#!/usr/bin/env bash
# Format a JuiceFS volume on Cloudflare R2 + Postgres metadata, then run the
# Rust FFI proof against it.
#
# Uses the single Cloudflare "API Access" token (CLOUDFLARE_API_TOKEN) as the
# R2 S3 credential via Cloudflare's documented derivation:
#     S3 Access Key ID     = the token's ID  (from /user/tokens/verify)
#     S3 Secret Access Key = SHA-256 hex of the token value
# So no separate R2 access-key/secret needs storing — just the token.
#
# PREREQ: that token must carry "Workers R2 Storage: Edit" (account-scoped)
# permission, or every R2 op returns AccessDenied. Add it in the Cloudflare
# dashboard (My Profile -> API Tokens -> edit the API Access token).
#
# Run:  ! cd ~/code/trove/spike && ./format-r2.sh
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?set CLOUDFLARE_API_TOKEN (source ~/.secret_env)}"
: "${CLOUDFLARE_ACCOUNT_ID:?set CLOUDFLARE_ACCOUNT_ID}"
BUCKET="${R2_BUCKET:-trove}"

AKID=$(curl -s "https://api.cloudflare.com/client/v4/user/tokens/verify" \
  -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["id"])')
SECRET=$(printf '%s' "$CLOUDFLARE_API_TOKEN" | sha256sum | cut -d' ' -f1)
ENDPOINT="https://${BUCKET}.${CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com"

JFS=/home/cp/code/trove/spike/juicefs/juicefs
META="postgres://cp:spike@127.0.0.1:5432/trove_r2_spike?sslmode=disable"
VOL="trover2"

createdb trove_r2_spike 2>/dev/null || echo "(db trove_r2_spike already exists)"

echo "→ formatting JuiceFS volume '$VOL' on R2 ($ENDPOINT)"
"$JFS" format --storage s3 --bucket "$ENDPOINT" \
  --access-key "$AKID" --secret-key "$SECRET" \
  "$META" "$VOL"

echo "→ running Rust FFI proof against the R2-backed volume"
cd /home/cp/code/trove/spike/ffi-proof
TROVE_META="$META" TROVE_VOL="$VOL" cargo run 2>&1 | grep -v "<INFO>\|<WARNING>"
