#!/bin/sh
# Uploads the packaged backup artifact to S3.  Runs in the rclone image
# (S3_CLIENT_IMAGE) — keep this script free of package installs.
#
# Required env vars:
#   S3_ENDPOINT, S3_BUCKET, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
# Optional env vars:
#   S3_KEY        — destination object key (defaults to packaged filename)
#   S3_INSECURE   — "true" to skip TLS verification
#
# Reads ARTIFACT and FILENAME from /workspace/.artifact-meta written by the
# package init container.

# Do NOT add -x here: the shell trace expands secrets held in env into the
# container log, which ships to the cluster log aggregator.
set -e

[ -f /workspace/.artifact-meta ] || { echo "missing /workspace/.artifact-meta from package step" >&2; exit 1; }
. /workspace/.artifact-meta

[ -n "$ARTIFACT" ] && [ -f "$ARTIFACT" ] || { echo "artifact not found: $ARTIFACT" >&2; exit 1; }
ls -lh "$ARTIFACT"

DEST_KEY="${S3_KEY:-$FILENAME}"
[ -n "$S3_BUCKET" ] && [ -n "$S3_ENDPOINT" ] || { echo "S3 config missing" >&2; exit 1; }

# The remote is defined entirely through env: env_auth makes rclone read
# AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY itself, so the credentials never
# appear on a command line.  Provider "Other" = plain S3 (MinIO, Ceph RGW, …).
export RCLONE_CONFIG_DEST_TYPE=s3
export RCLONE_CONFIG_DEST_PROVIDER=Other
export RCLONE_CONFIG_DEST_ENV_AUTH=true
export RCLONE_CONFIG_DEST_ENDPOINT="$S3_ENDPOINT"
# The AWS SDK's default ("when_supported") adds a trailing CRC checksum sent as
# an aws-chunked body, which MinIO rejects past 16 MiB ("chunk too big").
# Only send checksums the API requires.
export AWS_REQUEST_CHECKSUM_CALCULATION=when_required

RCLONE_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && RCLONE_INSECURE="--no-check-certificate"

# --s3-no-check-bucket: never try to create the bucket, so upload-only keys work.
rclone copyto "$ARTIFACT" "dest:$S3_BUCKET/$DEST_KEY" \
    --s3-no-check-bucket --stats-one-line --stats 30s -v $RCLONE_INSECURE
echo "=== Upload complete: $S3_BUCKET/$DEST_KEY ==="
