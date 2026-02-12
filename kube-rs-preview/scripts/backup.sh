#!/bin/sh
# Creates an Odoo database backup artifact in /mnt/backup/.
# Writes the final filename to /mnt/backup/.artifact-name for the uploader to consume.
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — source database name
#   BACKUP_FORMAT              — "zip", "dump", or "sql"
#   INSTANCE_NAME              — used for default filename
#
# Optional env vars:
#   BACKUP_WITH_FILESTORE — "true" to include filestore in zip backups (default "true")
#   LOCAL_FILENAME — override the output filename (extension appended if missing)

set -ex
export PGPASSWORD=$PASSWORD
FILENAME="${INSTANCE_NAME}-$(date +%Y%m%d-%H%M%S)"
TARGET="${LOCAL_FILENAME:-$FILENAME}"

if [ "$BACKUP_FORMAT" = "zip" ] && [ "$BACKUP_WITH_FILESTORE" != "false" ]; then
    case "$TARGET" in *.zip) ;; *) TARGET="$TARGET.zip" ;; esac
    odoo db \
      --db_host "$HOST" --db_port "$PORT" \
      --db_user "$USER" --db_password "$PASSWORD" \
      dump "$DB_NAME" "/mnt/backup/$TARGET"
elif [ "$BACKUP_FORMAT" = "dump" ]; then
    case "$TARGET" in *.dump) ;; *) TARGET="$TARGET.dump" ;; esac
    pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
      --format=custom -f "/mnt/backup/$TARGET"
else
    case "$TARGET" in *.sql) ;; *) TARGET="$TARGET.sql" ;; esac
    pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
      > "/mnt/backup/$TARGET"
fi

echo "$TARGET" > /mnt/backup/.artifact-name
ls -lh "/mnt/backup/$TARGET"
