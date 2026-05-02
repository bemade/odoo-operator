#!/bin/sh
# Packages the dump (and filestore for zip format) and uploads to S3.
# Runs in quay.io/minio/mc (alpine-based); installs `zip` on demand for zip format.
#
# For zip format the filestore PVC is read directly into the archive via a
# symlink — no intermediate copy.
#
# Required env vars:
#   BACKUP_FORMAT, INSTANCE_NAME, DB_NAME
#   S3_ENDPOINT, S3_BUCKET, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY
# Optional env vars:
#   LOCAL_FILENAME — output filename (extension appended if missing)
#   S3_KEY         — destination object key (defaults to LOCAL_FILENAME)
#   S3_INSECURE    — "true" to skip TLS verification
#   MC_CONFIG_DIR  — defaults to /tmp/.mc
#   BACKUP_WITH_FILESTORE — "false" to omit filestore from zip (default true)

set -ex
MC_CONFIG_DIR="${MC_CONFIG_DIR:-/tmp/.mc}"
mkdir -p "$MC_CONFIG_DIR"

case "$BACKUP_FORMAT" in
    zip) apk add --no-cache zip > /dev/null ;;
esac

FILENAME="${LOCAL_FILENAME:-${INSTANCE_NAME}-$(date +%Y%m%d-%H%M%S)}"

case "$BACKUP_FORMAT" in
    zip)
        case "$FILENAME" in *.zip) ;; *) FILENAME="$FILENAME.zip" ;; esac
        ARTIFACT="/workspace/$FILENAME"
        if [ "${BACKUP_WITH_FILESTORE:-true}" = "true" ] && \
           [ -d "/var/lib/odoo/filestore/$DB_NAME" ]; then
            echo "=== Adding filestore to zip (streamed from PVC) ==="
            ln -sfn "/var/lib/odoo/filestore/$DB_NAME" /tmp/filestore
            (cd /tmp && zip -qr "$ARTIFACT" filestore)
        fi
        echo "=== Adding dump.sql to zip ==="
        (cd /workspace && zip -q "$FILENAME" dump.sql)
        ;;
    dump)
        case "$FILENAME" in *.dump) ;; *) FILENAME="$FILENAME.dump" ;; esac
        ARTIFACT="/workspace/$FILENAME"
        mv /workspace/dump.dump "$ARTIFACT"
        ;;
    *)
        case "$FILENAME" in *.sql) ;; *) FILENAME="$FILENAME.sql" ;; esac
        ARTIFACT="/workspace/$FILENAME"
        mv /workspace/dump.sql "$ARTIFACT"
        ;;
esac

ls -lh "$ARTIFACT"

DEST_KEY="${S3_KEY:-$FILENAME}"
[ -n "$S3_BUCKET" ] && [ -n "$S3_ENDPOINT" ] || { echo "S3 config missing" >&2; exit 1; }

MC_INSECURE=""
[ "${S3_INSECURE}" = "true" ] && MC_INSECURE="--insecure"

mc $MC_INSECURE alias set dest "$S3_ENDPOINT" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
mc $MC_INSECURE cp "$ARTIFACT" "dest/$S3_BUCKET/$DEST_KEY"
echo "=== Upload complete: dest/$S3_BUCKET/$DEST_KEY ==="
