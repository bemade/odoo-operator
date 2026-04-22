#!/bin/sh
# Restores an Odoo database (and filestore, for zip backups) from an artifact
# in /mnt/backup/.
#
# Format detected by filename:
#   /mnt/backup/backup.zip  — Odoo zip backup: dump.sql + filestore/ tree
#   /mnt/backup/dump.dump   — pg_dump custom format
#   /mnt/backup/dump.sql    — pg_dump plain SQL
#
# Pipeline (identical for all three formats):
#   1. Load the database into $DB_NAME with strict error handling.
#   2. Run `odoo neutralize` if NEUTRALIZE=True.
#   3. Verify neutralization flag and that no active mail servers remain.
#   4. Rsync the extracted filestore into the PVC (zip format only).
#
# On ANY failure after this script touches the target DB, the DB is dropped
# via EXIT trap so the instance transitions to Uninitialized rather than
# starting up against a half-restored or un-neutralized database.
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — target database name
#   NEUTRALIZE                 — "True" to neutralize after restore, "False" to skip

set -eu
export PGPASSWORD=$PASSWORD

DB_TOUCHED=0

cleanup_on_failure() {
    rc=$?
    if [ "$rc" -ne 0 ] && [ "$DB_TOUCHED" -eq 1 ]; then
        echo "=== Restore failed (rc=$rc) — dropping database $DB_NAME ==="
        dropdb -h "$HOST" -p "$PORT" -U "$USER" --if-exists --force "$DB_NAME" || \
            echo "WARNING: dropdb failed — database may be left behind"
    fi
    exit "$rc"
}
trap cleanup_on_failure EXIT

echo "=== Starting restore process ==="
echo "Target database: $DB_NAME"
echo "Neutralize: $NEUTRALIZE"
echo "Backup directory contents:"
ls -lh /mnt/backup/

# Drop any pre-existing target database. An "invalid" database (e.g. from a
# previously interrupted CREATE DATABASE) exists in pg_database but refuses
# connections, so exp_db_exist-based checks silently skip it. DROP DATABASE
# IF EXISTS works on the catalog directly.
echo "=== Dropping existing database if present ==="
psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres \
    -c "DROP DATABASE IF EXISTS \"$DB_NAME\" WITH (FORCE)"

WORKDIR=""
FILESTORE_SRC=""

extract_zip() {
    WORKDIR=$(mktemp -d)
    echo "=== Extracting zip backup to $WORKDIR ==="
    # The odoo image ships python3 but not unzip. zipfile handles Odoo's
    # dump.sql + filestore/ tree without any external deps.
    python3 -c "import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" \
        /mnt/backup/backup.zip "$WORKDIR"
    if [ ! -f "$WORKDIR/dump.sql" ]; then
        echo "ERROR: backup.zip does not contain dump.sql"
        ls -la "$WORKDIR"
        exit 1
    fi
    if [ -d "$WORKDIR/filestore" ]; then
        FILESTORE_SRC="$WORKDIR/filestore"
        echo "Found filestore directory in zip"
    else
        echo "No filestore directory in zip (DB-only backup)"
    fi
}

load_sql() {
    SQL_FILE="$1"
    echo "=== Loading plain SQL dump into $DB_NAME ==="
    createdb -h "$HOST" -p "$PORT" -U "$USER" "$DB_NAME"
    DB_TOUCHED=1
    # Filter cross-cluster noise (ownership/ACL statements reference roles
    # that may not exist in the target cluster). The actual data and schema
    # statements still flow through with ON_ERROR_STOP active.
    #
    # Each pattern requires the matching statement to begin AND end on the
    # same line (ending in `;`) — pg_dump plain output always emits these
    # forms as single-line statements, but requiring the terminator makes
    # the filter robust to any future multi-line variant: a partial match
    # simply won't fire, instead of surgically corrupting a multi-line stmt.
    sed -E \
        -e '/^ALTER [A-Z ]+[^;]*OWNER TO [^;]*;$/d' \
        -e '/^GRANT [^;]*;$/d' \
        -e '/^REVOKE [^;]*;$/d' \
        -e '/^REASSIGN OWNED BY [^;]*;$/d' \
        "$SQL_FILE" | \
        psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
             -v ON_ERROR_STOP=1 --quiet
}

load_custom_dump() {
    echo "=== Loading custom-format dump into $DB_NAME ==="
    createdb -h "$HOST" -p "$PORT" -U "$USER" "$DB_NAME"
    DB_TOUCHED=1
    pg_restore -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
        --no-owner --no-acl --exit-on-error \
        /mnt/backup/dump.dump
}

reinit_db_params() {
    echo "=== Re-initializing database parameters ==="
    # Tagged dollar quote ($body$) avoids a shell-interpretation pitfall:
    # dash (which is /bin/sh in the odoo image) expands $$ inside quoted
    # heredocs, mangling PL/pgSQL's anonymous DO block before it reaches psql.
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
}

verify_neutralization() {
    echo "=== Verifying neutralization ==="
    NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
        -c "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';")
    if [ "$NEUTRALIZED" != "true" ] && [ "$NEUTRALIZED" != "True" ]; then
        echo "CRITICAL: database.is_neutralized='$NEUTRALIZED' (expected 'true')"
        exit 1
    fi
    echo "database.is_neutralized = $NEUTRALIZED"
}

# Verify no active mail servers remain after neutralization. `odoo neutralize`
# relies on per-module hooks; if a custom module owns a mail model whose hook
# is missing, or if fetchmail's neutralize hook didn't fire (module not
# installed at neutralize time, upgrade skew, etc.), active mail servers can
# survive neutralization. Those will cheerfully fetch from / send to real
# mail accounts. Catch them here before the instance starts.
verify_no_active_mail_servers() {
    echo "=== Verifying no active mail servers ==="
    # Odoo's neutralize deletes all ir_mail_server rows and inserts a single
    # sentinel record with smtp_host='invalid'. That's expected. The incident
    # we guard against is a REAL smtp_host (real hostname, IP, or "localhost")
    # surviving neutralization — which can happen when a custom module owns
    # a mail model whose neutralize hook is missing or didn't run.
    OUT_DANGEROUS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -t -A \
        -c "SELECT COUNT(*) FROM ir_mail_server WHERE active AND smtp_host != 'invalid'")
    if [ "$OUT_DANGEROUS" != "0" ]; then
        echo "CRITICAL: $OUT_DANGEROUS active outgoing mail servers with real hosts after neutralize"
        psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
            -c "SELECT id, name, smtp_host FROM ir_mail_server WHERE active AND smtp_host != 'invalid'"
        exit 1
    fi
    echo "Outgoing mail servers: no dangerous hosts (sentinel-only)"

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
        echo "Incoming mail servers: 0 active"
    else
        echo "fetchmail_server table absent (module not installed) — skipping"
    fi
}

# Additive merge — filestore entries are content-addressable (sha1 filenames),
# so leaving existing files on the PVC is safe: any colliding name refers to
# identical content by construction. shutil.copytree with dirs_exist_ok=True
# merges source into dest without removing entries already there. rsync isn't
# available in the odoo image.
sync_filestore() {
    if [ -z "$FILESTORE_SRC" ]; then
        return 0
    fi
    DEST="/var/lib/odoo/filestore/$DB_NAME"
    echo "=== Syncing filestore: $FILESTORE_SRC/ -> $DEST/ ==="
    mkdir -p "$DEST"
    python3 -c "import shutil, sys; shutil.copytree(sys.argv[1], sys.argv[2], dirs_exist_ok=True)" \
        "$FILESTORE_SRC" "$DEST"
    echo "Filestore sync complete"
}

# ── Dispatch on input format ─────────────────────────────────────────────
if [ -f /mnt/backup/dump.dump ]; then
    load_custom_dump
elif [ -f /mnt/backup/dump.sql ]; then
    load_sql /mnt/backup/dump.sql
elif [ -f /mnt/backup/backup.zip ]; then
    extract_zip
    load_sql "$WORKDIR/dump.sql"
else
    echo "ERROR: No backup file found in /mnt/backup/"
    ls -la /mnt/backup/
    exit 1
fi

# ── Neutralize ───────────────────────────────────────────────────────────
if [ "$NEUTRALIZE" = "True" ]; then
    reinit_db_params
    echo "=== Running odoo neutralize ==="
    odoo neutralize \
        --db_host "$HOST" --db_port "$PORT" \
        --db_user "$USER" --db_password "$PASSWORD" \
        -d "$DB_NAME"
    verify_neutralization
    verify_no_active_mail_servers

    # Staging mail redirect: the operator sets MAIL_SMTP_HOST (and port /
    # encryption) when the instance is tagged `environment: Staging` and a
    # cluster-wide Mailpit (or any SMTP sink) has been configured via
    # --default-staging-smtp-host.  After neutralize there is exactly one
    # active ir_mail_server row with smtp_host='invalid' — we rewrite it
    # in place to point at the sink.  Production instances and operators
    # with no sink configured skip this block entirely.
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
else
    echo "Skipping neutralization (NEUTRALIZE=$NEUTRALIZE)"
fi

# ── Filestore (zip backups only) ─────────────────────────────────────────
sync_filestore

# Success — disarm the cleanup trap so we don't drop the DB on normal exit.
DB_TOUCHED=0
echo "=== Restore process complete ==="
