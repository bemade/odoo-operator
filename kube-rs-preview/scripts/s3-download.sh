#!/bin/sh
# Downloads a backup artifact from S3/MinIO to /mnt/backup/.
#
# Required env vars:
#   S3_ENDPOINT, S3_BUCKET, S3_KEY
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
#   OUTPUT_FILE  — target path, e.g. /mnt/backup/backup.zip
#
# Optional env vars:
#   S3_INSECURE  — set to "true" to skip TLS verification
#   MC_CONFIG_DIR — defaults to /tmp/.mc

set -ex
echo "=== Downloading backup from S3 ==="
echo "Endpoint: $S3_ENDPOINT  Bucket: $S3_BUCKET  Key: $S3_KEY"

MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

MC_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && MC_INSECURE="--insecure"

mc $MC_INSECURE alias set source "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_INSECURE cp "source/$S3_BUCKET/$S3_KEY" "$OUTPUT_FILE"

echo "Download complete:"
ls -lh "$OUTPUT_FILE"
echo "=== S3 download complete ==="
