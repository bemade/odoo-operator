#!/bin/sh
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
# pg_restore --jobs parallelises index builds, which is typically the
# wall-clock dominant factor on the target side.  --no-owner / --no-acl
# drop ownership & ACL statements (roles from the source cluster rarely
# exist on the target).  --exit-on-error aborts on real failures.
JOBS=$(nproc 2>/dev/null || echo 2)
echo "=== Streaming pg_dump | pg_restore (jobs=$JOBS) ==="
PGPASSWORD=$SRC_PASSWORD pg_dump \
    -h "$SRC_HOST" -p "$SRC_PORT" -U "$SRC_USER" \
    -Fc --no-owner --no-acl "$SRC_DB" | \
PGPASSWORD=$TGT_PASSWORD pg_restore \
    -h "$TGT_HOST" -p "$TGT_PORT" -U "$TGT_USER" \
    -d "$TEMP_DB" --no-owner --no-acl --exit-on-error --jobs="$JOBS"

echo "=== Cutover: drop $TGT_DB, rename $TEMP_DB -> $TGT_DB ==="
PGPASSWORD=$TGT_PASSWORD psql -h "$TGT_HOST" -p "$TGT_PORT" -U "$TGT_USER" \
    -d postgres -v ON_ERROR_STOP=1 <<EOSQL
DROP DATABASE IF EXISTS "$TGT_DB" WITH (FORCE);
ALTER DATABASE "$TEMP_DB" RENAME TO "$TGT_DB";
EOSQL

# Disarm trap — we succeeded and the temp DB is now the live one.
TEMP_DB="__ALREADY_RENAMED__"

echo "=== Clone DB complete ==="
