#!/bin/sh
# Dumps the source database to /workspace/.  Runs in postgres:<N>-alpine
# (pg client tools matching the server major — see operator's pg_tools_image()).
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — source database
#   BACKUP_FORMAT              — "zip" | "sql" | "dump"
#                                (zip and sql produce dump.sql; dump produces dump.dump)

set -ex
export PGPASSWORD=$PASSWORD
echo "=== pg_dump $DB_NAME from $HOST:$PORT (format=$BACKUP_FORMAT) ==="

case "$BACKUP_FORMAT" in
    dump)
        pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            --format=custom --no-owner --no-acl -f /workspace/dump.dump
        ;;
    *)
        pg_dump -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            --no-owner --no-acl > /workspace/dump.sql
        ;;
esac

ls -lh /workspace/
echo "=== Dump complete ==="
