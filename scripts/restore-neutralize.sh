#!/bin/sh
# Runs `odoo neutralize` against an already-loaded database, verifies the
# neutralization flag and that no real-host mail servers survived, and (for
# staging instances with a configured Mailpit sink) rewrites the neutralize
# sentinel to point at the sink.
#
# Drops the database on failure so the state machine retries from a clean
# slate (matching the previous monolithic restore.sh contract).
#
# Runs in the Odoo image — this is the only step in the restore pipeline
# that needs an Odoo binary.  When NEUTRALIZE=False this script is replaced
# at the orchestration layer by an `alpine` no-op container.
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — target PostgreSQL connection
#   DB_NAME                    — target database
# Optional env vars:
#   MAIL_SMTP_HOST, MAIL_SMTP_PORT, MAIL_SMTP_ENCRYPTION

set -eu
export PGPASSWORD=$PASSWORD

cleanup_on_failure() {
    rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "=== Neutralize failed (rc=$rc) — dropping $DB_NAME ==="
        dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists --force "$DB_NAME" || \
            echo "WARNING: dropdb failed"
    fi
    exit "$rc"
}
trap cleanup_on_failure EXIT

echo "=== Running odoo neutralize ==="
odoo neutralize \
    --db_host "$HOST" --db_port "$PORT" \
    --db_user "$USER" --db_password "$PASSWORD" \
    -d "$DB_NAME"

echo "=== Verifying neutralization flag ==="
NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';")
case "$NEUTRALIZED" in
    true|True) echo "database.is_neutralized = $NEUTRALIZED" ;;
    *) echo "CRITICAL: database.is_neutralized='$NEUTRALIZED' (expected 'true')"; exit 1 ;;
esac

# Odoo's neutralize wipes ir_mail_server and inserts a sentinel with
# smtp_host='invalid'.  We guard against custom modules whose neutralize
# hook is missing or didn't run, leaving real-host mail servers active.
echo "=== Verifying no active mail servers with real hosts ==="
OUT_DANGEROUS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT COUNT(*) FROM ir_mail_server WHERE active AND smtp_host != 'invalid'")
if [ "$OUT_DANGEROUS" != "0" ]; then
    echo "CRITICAL: $OUT_DANGEROUS active outgoing mail servers with real hosts after neutralize"
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
        -c "SELECT id, name, smtp_host FROM ir_mail_server WHERE active AND smtp_host != 'invalid'"
    exit 1
fi

# fetchmail_server only exists when the fetchmail module is installed.
FETCH_EXISTS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
    -c "SELECT to_regclass('public.fetchmail_server') IS NOT NULL")
if [ "$FETCH_EXISTS" = "t" ]; then
    IN_ACTIVE=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
        -c "SELECT COUNT(*) FROM fetchmail_server WHERE active")
    if [ "$IN_ACTIVE" != "0" ]; then
        echo "CRITICAL: $IN_ACTIVE active incoming mail servers after neutralize"
        psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            -c "SELECT id, name, server FROM fetchmail_server WHERE active"
        exit 1
    fi
fi

# Staging mail redirect: rewrite the neutralize sentinel to point at the
# operator's configured Mailpit (or any SMTP sink).  Production instances
# and operators with no sink configured skip this block.
if [ -n "${MAIL_SMTP_HOST:-}" ]; then
    echo "=== Rewriting neutralize sentinel → $MAIL_SMTP_HOST:${MAIL_SMTP_PORT:-1025} (${MAIL_SMTP_ENCRYPTION:-none}) ==="
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
