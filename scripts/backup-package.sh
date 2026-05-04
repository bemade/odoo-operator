#!/bin/sh
# Packages the dump (and filestore for zip format) into the final artifact at
# /workspace/$FILENAME.  Runs in alpine; installs `zip` on demand for zip format.
#
# For zip format the filestore PVC is read directly into the archive via a
# symlink — no intermediate copy.
#
# Required env vars:
#   BACKUP_FORMAT, INSTANCE_NAME, DB_NAME
# Optional env vars:
#   LOCAL_FILENAME — output filename (extension appended if missing)
#   BACKUP_WITH_FILESTORE — "false" to omit filestore from zip (default true)
#
# Writes the resolved artifact path and filename to /workspace/.artifact-meta
# so the upload container can pick them up without recomputing.

set -ex

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
printf 'ARTIFACT=%s\nFILENAME=%s\n' "$ARTIFACT" "$FILENAME" > /workspace/.artifact-meta
echo "=== Package complete: $ARTIFACT ==="
