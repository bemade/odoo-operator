#!/bin/sh
# Downloads a backup from a running Odoo instance to /mnt/backup/.
#
# Required env vars:
#   ODOO_URL        — base URL of the source instance (e.g. https://src.example.com)
#   SOURCE_DB       — database name to dump
#   MASTER_PASSWORD — Odoo master (admin) password
#   BACKUP_FORMAT   — "zip" or "dump"
#   OUTPUT_FILE     — target path, e.g. /mnt/backup/backup.zip

set -ex
echo "=== Downloading backup from Odoo instance ==="
echo "URL: $ODOO_URL  DB: $SOURCE_DB"

curl -X POST \
     -F "master_pwd=$MASTER_PASSWORD" \
     -F "name=$SOURCE_DB" \
     -F "backup_format=$BACKUP_FORMAT" \
     -o "$OUTPUT_FILE" \
     -w "HTTP Status: %{http_code}\n" \
     --fail-with-body \
     "$ODOO_URL/web/database/backup"

echo "Download complete:"
ls -lh "$OUTPUT_FILE"

FILE_SIZE=$(stat -c%s "$OUTPUT_FILE" 2>/dev/null || stat -f%z "$OUTPUT_FILE")
if [ "$FILE_SIZE" -lt 102400 ]; then
    echo "ERROR: Downloaded file is too small ($FILE_SIZE bytes) — likely an error response!"
    cat "$OUTPUT_FILE"
    exit 1
fi

# Format-specific validation
if [ "$BACKUP_FORMAT" = "zip" ]; then
    MAGIC=$(head -c 2 "$OUTPUT_FILE" | od -An -tx1 | tr -d ' ')
    if [ "$MAGIC" != "504b" ]; then
        echo "ERROR: Downloaded file is not a valid zip file (magic: $MAGIC)!"
        head -c 500 "$OUTPUT_FILE"
        exit 1
    fi
    echo "Valid zip file confirmed (PK header found)"
else
    HEADER=$(head -c 5 "$OUTPUT_FILE")
    if echo "$HEADER" | grep -qE '^(--|PGDMP|CREAT|SET )'; then
        echo "Valid SQL dump confirmed"
    else
        echo "ERROR: Downloaded file does not appear to be a valid SQL dump!"
        head -c 500 "$OUTPUT_FILE"
        exit 1
    fi
fi

echo "=== Odoo download complete ==="
