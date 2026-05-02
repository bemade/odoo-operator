#!/bin/sh
# Loads /workspace/dump.{sql,dump} into $DB_NAME on the target cluster.
# Drops the DB on failure so the state machine retries from a clean slate.
# Runs in postgres:<N>-alpine (matches target server major).
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — target PostgreSQL connection (admin)
#   DB_NAME                    — target database name
#   BACKUP_FORMAT              — "zip" | "sql" | "dump"

set -eu
export PGPASSWORD=$PASSWORD

DB_TOUCHED=0
cleanup_on_failure() {
    rc=$?
    if [ "$rc" -ne 0 ] && [ "$DB_TOUCHED" -eq 1 ]; then
        echo "=== Load failed (rc=$rc) — dropping $DB_NAME ==="
        dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists --force "$DB_NAME" || \
            echo "WARNING: dropdb failed — DB may be left behind"
    fi
    exit "$rc"
}
trap cleanup_on_failure EXIT

# Drop any pre-existing target database.  An "invalid" database from an
# interrupted CREATE DATABASE exists in pg_catalog but refuses connections;
# DROP DATABASE IF EXISTS works against the catalog directly.
echo "=== Dropping existing $DB_NAME if present ==="
psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres -v ON_ERROR_STOP=1 \
    -c "DROP DATABASE IF EXISTS \"$DB_NAME\" WITH (FORCE)"

echo "=== Creating empty $DB_NAME ==="
createdb -h "$HOST" -p "$PORT" -U "$USER" "$DB_NAME"
DB_TOUCHED=1

case "$BACKUP_FORMAT" in
    dump)
        echo "=== Loading custom-format dump ==="
        pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            --no-owner --no-acl --exit-on-error /workspace/dump.dump
        ;;
    *)
        # Plain SQL.  Filter cross-cluster noise (ownership/ACL statements
        # reference roles that may not exist in the target).  Patterns match
        # complete single-line statements (pg_dump emits these on one line
        # terminated with `;`); requiring the terminator keeps the filter
        # robust against any future multi-line variant.  Also strips
        # `SET transaction_timeout = 0` from pg_dump 17+ which earlier
        # servers reject as an unrecognized parameter.
        echo "=== Loading plain SQL dump ==="
        sed -E \
            -e '/^SET transaction_timeout /d' \
            -e '/^ALTER [A-Z ]+[^;]*OWNER TO [^;]*;$/d' \
            -e '/^GRANT [^;]*;$/d' \
            -e '/^REVOKE [^;]*;$/d' \
            -e '/^REASSIGN OWNED BY [^;]*;$/d' \
            /workspace/dump.sql | \
            psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
                -v ON_ERROR_STOP=1 --quiet
        ;;
esac

# Re-initialize per-database identity / URL parameters.  Tagged dollar quote
# ($body$) sidesteps shell $$ expansion in heredocs.
echo "=== Re-initializing database parameters ==="
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -v ON_ERROR_STOP=1 << 'EOSQL'
DO $body$
DECLARE
    new_secret TEXT := gen_random_uuid()::text;
    new_uuid TEXT := gen_random_uuid()::text;
BEGIN
    DELETE FROM ir_config_parameter WHERE key IN (
        'database.secret', 'database.uuid', 'database.create_date',
        'web.base.url', 'base.login_cooldown_after', 'base.login_cooldown_duration'
    );
    INSERT INTO ir_config_parameter (key, value, create_uid, create_date, write_uid, write_date) VALUES
        ('database.secret',              new_secret,              1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.uuid',                new_uuid,                1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.create_date',         LOCALTIMESTAMP::text,    1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('web.base.url',                 'http://localhost:8069', 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_after',    '10',                    1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_duration', '60',                    1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP);
END $body$;
EOSQL

echo "=== Database load complete ==="
