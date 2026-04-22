#!/bin/bash
# Neutralizes a freshly-cloned Odoo database and verifies that no active
# mail servers remain.  Used as the post-clone barrier step of
# OdooStagingRefreshJob, *after* both the DB clone and filestore copy
# have succeeded.
#
# This is the same neutralize + verify sequence that restore.sh runs at
# the end of its pipeline, extracted into a standalone script so the
# staging-refresh flow doesn't have to pretend it has a backup artifact.
#
# On any failure — reinit error, neutralize error, neutralization flag
# missing, or an active mail server with a non-'invalid' smtp_host —
# the script exits non-zero.  The operator reacts by transitioning the
# target OdooInstance to InitFailed and leaves the live DB as-is
# (we're working on the live DB *after* the rename cutover, so failure
# here means staging is actively running against an un-neutralized
# production clone — callers MUST scale down immediately).
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — target database name

set -euo pipefail
export PGPASSWORD=$PASSWORD

echo "=== Neutralize starting ==="
echo "Database: $DB_NAME"

echo "=== Re-initializing database parameters ==="
psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
     -v ON_ERROR_STOP=1 << 'EOSQL'
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

echo "=== Running odoo neutralize ==="
odoo neutralize \
    --db_host "$HOST" --db_port "$PORT" \
    --db_user "$USER" --db_password "$PASSWORD" \
    -d "$DB_NAME"

echo "=== Verifying neutralization ==="
NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';")
if [ "$NEUTRALIZED" != "true" ] && [ "$NEUTRALIZED" != "True" ]; then
    echo "CRITICAL: database.is_neutralized='$NEUTRALIZED' (expected 'true')"
    exit 1
fi
echo "database.is_neutralized = $NEUTRALIZED"

echo "=== Verifying no active mail servers ==="
# Odoo's neutralize replaces ir_mail_server with a single sentinel row
# whose smtp_host is 'invalid'.  That's expected.  Any survivor with a
# real host is a threat (custom module that didn't neutralize its own
# mail model, or neutralize hook that didn't fire).
OUT_DANGEROUS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT COUNT(*) FROM ir_mail_server WHERE active AND smtp_host != 'invalid'")
if [ "$OUT_DANGEROUS" != "0" ]; then
    echo "CRITICAL: $OUT_DANGEROUS active outgoing mail servers with real hosts"
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
        -c "SELECT id, name, smtp_host FROM ir_mail_server WHERE active AND smtp_host != 'invalid'"
    exit 1
fi
echo "Outgoing mail servers: no dangerous hosts (sentinel-only)"

FETCH_EXISTS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT to_regclass('public.fetchmail_server') IS NOT NULL")
if [ "$FETCH_EXISTS" = "t" ]; then
    IN_ACTIVE=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
        -c "SELECT COUNT(*) FROM fetchmail_server WHERE active")
    if [ "$IN_ACTIVE" != "0" ]; then
        echo "CRITICAL: $IN_ACTIVE active incoming mail servers"
        psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            -c "SELECT id, name, server FROM fetchmail_server WHERE active"
        exit 1
    fi
    echo "Incoming mail servers: 0 active"
else
    echo "fetchmail_server table absent (module not installed) — skipping"
fi

# Staging mail redirect: the operator sets MAIL_SMTP_HOST (and port /
# encryption) when the instance is tagged `environment: Staging` and a
# cluster-wide Mailpit (or any SMTP sink) has been configured via
# --default-staging-smtp-host.  After neutralize there is exactly one
# active ir_mail_server row with smtp_host='invalid' — we rewrite it in
# place to point at the sink.  Production instances and operators with
# no sink configured skip this block entirely (env var empty).
if [ -n "${MAIL_SMTP_HOST:-}" ]; then
    echo "=== Rewriting neutralize sentinel → $MAIL_SMTP_HOST:${MAIL_SMTP_PORT:-1025} (${MAIL_SMTP_ENCRYPTION:-none}) ==="
    # psql's :'var' interpolation only works when reading from stdin,
    # not with -c, so we use a heredoc.
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
         -v ON_ERROR_STOP=1 \
         -v mail_host="$MAIL_SMTP_HOST" \
         -v mail_port="${MAIL_SMTP_PORT:-1025}" \
         -v mail_enc="${MAIL_SMTP_ENCRYPTION:-none}" <<'EOSQL'
UPDATE ir_mail_server
SET smtp_host = :'mail_host',
    smtp_port = :'mail_port'::integer,
    smtp_encryption = :'mail_enc',
    name = 'Mailpit (operator-injected for staging)'
WHERE active AND smtp_host = 'invalid';
EOSQL
fi

echo "=== Neutralize complete ==="
