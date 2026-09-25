#!/bin/sh
# Downloads a backup artifact from S3/MinIO to /mnt/backup/.  Runs in the
# rclone image (S3_CLIENT_IMAGE).
#
# Required env vars:
#   S3_ENDPOINT, S3_BUCKET, S3_KEY
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
#   OUTPUT_FILE  — target path, e.g. /mnt/backup/backup.zip
#
# Optional env vars:
#   S3_INSECURE  — set to "true" to skip TLS verification

# Do NOT add -x here: the shell trace expands secrets held in env into the
# container log, which ships to the cluster log aggregator.
set -e
echo "=== Downloading backup from S3 ==="
echo "Endpoint: $S3_ENDPOINT  Bucket: $S3_BUCKET  Key: $S3_KEY"

# Remote defined through env; env_auth reads AWS_ACCESS_KEY_ID /
# AWS_SECRET_ACCESS_KEY so the credentials never appear on a command line.
export RCLONE_CONFIG_SOURCE_TYPE=s3
export RCLONE_CONFIG_SOURCE_PROVIDER=Other
export RCLONE_CONFIG_SOURCE_ENV_AUTH=true
export RCLONE_CONFIG_SOURCE_ENDPOINT="$S3_ENDPOINT"
# Same SDK checksum setting as backup-upload.sh (see the note there).
export AWS_REQUEST_CHECKSUM_CALCULATION=when_required

RCLONE_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && RCLONE_INSECURE="--no-check-certificate"

rclone copyto "source:$S3_BUCKET/$S3_KEY" "$OUTPUT_FILE" \
    --s3-no-check-bucket --stats-one-line --stats 30s -v $RCLONE_INSECURE

echo "Download complete:"
ls -lh "$OUTPUT_FILE"
echo "=== S3 download complete ==="
