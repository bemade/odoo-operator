#!/bin/sh
# Restores an Odoo database from a backup artifact in /mnt/backup/.
#
# The format is detected by filename:
#   /mnt/backup/backup.zip  — Odoo zip backup  (odoo db load)
#   /mnt/backup/dump.dump   — pg_dump custom format (pg_restore)
#   /mnt/backup/dump.sql    — pg_dump plain SQL (psql)
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — target database name
#   NEUTRALIZE                 — "True" to neutralize after restore, "False" to skip

set -ex
export PGPASSWORD=$PASSWORD

echo "=== Starting restore process ==="
echo "Target database: $DB_NAME"
echo "Neutralize: $NEUTRALIZE"

echo "Backup directory contents:"
ls -lh /mnt/backup/

echo "=== Dropping existing database if present ==="
odoo db --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" \
  drop "$DB_NAME" || true

if [ -f /mnt/backup/dump.dump ]; then
    echo "Found dump.dump — using pg_restore method"
    createdb -h "$HOST" -p "$PORT" -U "$USER" "$DB_NAME"
    # pg_restore may emit warnings (ownership, sequences) but still succeed; || true guards that.
    pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" --no-owner /mnt/backup/dump.dump || true

elif [ -f /mnt/backup/dump.sql ]; then
    echo "Found dump.sql — using psql method"
    createdb -h "$HOST" -p "$PORT" -U "$USER" "$DB_NAME"
    # psql may emit non-fatal errors; || true guards that.
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -f /mnt/backup/dump.sql || true

elif [ -f /mnt/backup/backup.zip ]; then
    echo "Found backup.zip — using odoo db load method"
    ls -lh /mnt/backup/backup.zip
    NEUTRALIZE_FLAG=""
    [ "$NEUTRALIZE" = "True" ] && NEUTRALIZE_FLAG="-n"
    # odoo db load handles DB creation; || true — see pg_restore comment above.
    odoo db --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" \
      load $NEUTRALIZE_FLAG -f "$DB_NAME" /mnt/backup/backup.zip || true

else
    echo "ERROR: No backup file found in /mnt/backup/"
    ls -la /mnt/backup/
    exit 1
fi

reinit_db_params() {
    echo "=== Re-initializing database parameters ==="
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" << 'EOSQL'
DO $$
DECLARE
    new_secret TEXT := gen_random_uuid()::text;
    new_uuid TEXT := gen_random_uuid()::text;
BEGIN
    DELETE FROM ir_config_parameter WHERE key IN (
        'database.secret', 'database.uuid', 'database.create_date',
        'web.base.url', 'base.login_cooldown_after', 'base.login_cooldown_duration'
    );
    INSERT INTO ir_config_parameter (key, value, create_uid, create_date, write_uid, write_date) VALUES
        ('database.secret',              new_secret,          1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.uuid',                new_uuid,            1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('database.create_date',         LOCALTIMESTAMP::text,1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('web.base.url',                 'http://localhost:8069', 1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_after',    '10',                1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP),
        ('base.login_cooldown_duration', '60',                1, LOCALTIMESTAMP, 1, LOCALTIMESTAMP);
    RAISE NOTICE 'Database parameters re-initialized successfully';
EXCEPTION WHEN OTHERS THEN
    RAISE WARNING 'Could not re-initialize database parameters: %', SQLERRM;
END $$;
EOSQL
    echo "Database parameter re-initialization complete"
}

verify_neutralization() {
    echo "=== Verifying neutralization ==="
    NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
        -c "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';" 2>/dev/null || echo "")
    if [ "$NEUTRALIZED" = "true" ] || [ "$NEUTRALIZED" = "True" ]; then
        echo "Neutralization verified: database.is_neutralized = $NEUTRALIZED"
        return 0
    else
        echo "CRITICAL: Neutralization verification FAILED (database.is_neutralized='$NEUTRALIZED')"
        echo "Dropping database to prevent un-neutralized DB from running."
        dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists "$DB_NAME"
        return 1
    fi
}

if [ "$NEUTRALIZE" = "True" ]; then
    reinit_db_params
    odoo neutralize --db_host "$HOST" --db_port "$PORT" --db_user "$USER" --db_password "$PASSWORD" -d "$DB_NAME"
    verify_neutralization || exit 1
else
    echo "Skipping neutralization (neutralize=False)"
fi

echo "=== Restore process complete ==="
