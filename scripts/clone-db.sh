#!/bin/bash
# Clones a Postgres database from a source Odoo instance to a staging
# instance by streaming pg_dump | pg_restore end-to-end through a single
# pod — no intermediate artifact, no S3.  Uses a temp DB name for the
# restore, then swaps into the live name at the end (blue/green cutover
# so a failure leaves the previous staging DB intact).
#
# Required env vars:
#   SRC_HOST, SRC_PORT, SRC_USER, SRC_PASSWORD, SRC_DB
#       — source Postgres connection (the production DB)
#   TGT_HOST, TGT_PORT, TGT_USER, TGT_PASSWORD, TGT_DB
#       — target Postgres connection (the staging DB)
#   TEMP_DB  — ephemeral DB name for the restore (before rename cutover)

set -euo pipefail

cleanup_on_failure() {
    rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "=== Clone failed (rc=$rc) — dropping temp DB $TEMP_DB ==="
        PGPASSWORD=$TGT_PASSWORD dropdb -h "$TGT_HOST" -p "$TGT_PORT" \
            -U "$TGT_USER" --if-exists --force "$TEMP_DB" || \
            echo "WARNING: dropdb failed — temp DB may be left behind"
    fi
    exit "$rc"
}
trap cleanup_on_failure EXIT

echo "=== Clone DB: $SRC_DB@$SRC_HOST -> $TGT_DB@$TGT_HOST (via $TEMP_DB) ==="

# Drop any leftover temp DB from a prior failed run.
PGPASSWORD=$TGT_PASSWORD psql -h "$TGT_HOST" -p "$TGT_PORT" -U "$TGT_USER" \
    -d postgres -c "DROP DATABASE IF EXISTS \"$TEMP_DB\" WITH (FORCE)"

PGPASSWORD=$TGT_PASSWORD createdb -h "$TGT_HOST" -p "$TGT_PORT" \
    -U "$TGT_USER" "$TEMP_DB"

# pg_dump reads via an MVCC snapshot — source writes keep flowing.
# --no-owner / --no-acl drop ownership & ACL statements (roles from the
# source cluster rarely exist on the target).
#
# Plain-SQL format (-Fp) + sed filter + psql ON_ERROR_STOP, mirroring the
# restore.sh path.  The alternative (custom format + pg_restore) has two
# problems:
#   1. pg_restore --jobs doesn't accept stdin, so no parallel restore;
#   2. Custom format carries pg-client-version SET statements (notably
#      `SET transaction_timeout = 0` from pg_dump 17+) that older
#      target servers reject as "unrecognized configuration parameter".
#      We can't sed-filter a binary format.
#
# sed filter patterns match complete single-line statements (pg_dump
# emits these forms on one line, terminated with ;).  The extra
# `SET transaction_timeout` match strips the PG17 incompatibility.
echo "=== Streaming pg_dump | psql ==="
PGPASSWORD=$SRC_PASSWORD pg_dump \
    -h "$SRC_HOST" -p "$SRC_PORT" -U "$SRC_USER" \
    -Fp --no-owner --no-acl "$SRC_DB" | \
sed -E \
    -e '/^SET transaction_timeout /d' \
    -e '/^ALTER [A-Z ]+[^;]*OWNER TO [^;]*;$/d' \
    -e '/^GRANT [^;]*;$/d' \
    -e '/^REVOKE [^;]*;$/d' \
    -e '/^REASSIGN OWNED BY [^;]*;$/d' | \
PGPASSWORD=$TGT_PASSWORD psql \
    -h "$TGT_HOST" -p "$TGT_PORT" -U "$TGT_USER" \
    -d "$TEMP_DB" -v ON_ERROR_STOP=1 --quiet

echo "=== Cutover: drop $TGT_DB, rename $TEMP_DB -> $TGT_DB ==="
PGPASSWORD=$TGT_PASSWORD psql -h "$TGT_HOST" -p "$TGT_PORT" -U "$TGT_USER" \
    -d postgres -v ON_ERROR_STOP=1 <<EOSQL
DROP DATABASE IF EXISTS "$TGT_DB" WITH (FORCE);
ALTER DATABASE "$TEMP_DB" RENAME TO "$TGT_DB";
EOSQL

# Disarm trap — we succeeded and the temp DB is now the live one.
TEMP_DB="__ALREADY_RENAMED__"

echo "=== Clone DB complete ==="
