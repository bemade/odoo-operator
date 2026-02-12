#!/bin/sh
# Uploads the backup artifact from /mnt/backup/ to S3/MinIO.
# Reads the filename from /mnt/backup/.artifact-name if LOCAL_FILENAME is not set.
#
# Required env vars:
#   S3_ENDPOINT, S3_BUCKET
#   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
#
# Optional env vars:
#   LOCAL_FILENAME — artifact filename under /mnt/backup/
#   S3_KEY         — destination object key (defaults to LOCAL_FILENAME)
#   S3_INSECURE    — set to "true" to skip TLS verification
#   MC_CONFIG_DIR  — defaults to /tmp/.mc

set -ex
MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

[ -f /mnt/backup/.artifact-name ] && LOCAL_FILENAME="$(cat /mnt/backup/.artifact-name)"
FILE="/mnt/backup/${LOCAL_FILENAME}"
DEST_KEY="${S3_KEY:-$LOCAL_FILENAME}"

[ -f "$FILE" ] || { echo "artifact $FILE not found" >&2; exit 1; }
[ -n "$S3_BUCKET" ] && [ -n "$S3_ENDPOINT" ] || { echo "S3 config missing" >&2; exit 1; }

MC_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && MC_INSECURE="--insecure"

mc $MC_INSECURE alias set dest "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_INSECURE cp "$FILE" "dest/$S3_BUCKET/$DEST_KEY"
echo "Upload complete: dest/$S3_BUCKET/$DEST_KEY"
