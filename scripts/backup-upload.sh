#!/bin/sh
# Uploads the packaged backup artifact to S3.  Runs in quay.io/minio/mc which
# no longer ships with apk — keep this script free of package installs.
#
# Required env vars:
#   S3_ENDPOINT, S3_BUCKET, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
# Optional env vars:
#   S3_KEY        — destination object key (defaults to packaged filename)
#   S3_INSECURE   — "true" to skip TLS verification
#   MC_CONFIG_DIR — defaults to /tmp/.mc
#
# Reads ARTIFACT and FILENAME from /workspace/.artifact-meta written by the
# package init container.

set -ex
MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

[ -f /workspace/.artifact-meta ] || { echo "missing /workspace/.artifact-meta from package step" >&2; exit 1; }
. /workspace/.artifact-meta

[ -n "$ARTIFACT" ] && [ -f "$ARTIFACT" ] || { echo "artifact not found: $ARTIFACT" >&2; exit 1; }
ls -lh "$ARTIFACT"

DEST_KEY="${S3_KEY:-$FILENAME}"
[ -n "$S3_BUCKET" ] && [ -n "$S3_ENDPOINT" ] || { echo "S3 config missing" >&2; exit 1; }

MC_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && MC_INSECURE="--insecure"

mc $MC_INSECURE alias set dest "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_INSECURE cp "$ARTIFACT" "dest/$S3_BUCKET/$DEST_KEY"
echo "=== Upload complete: dest/$S3_BUCKET/$DEST_KEY ==="
