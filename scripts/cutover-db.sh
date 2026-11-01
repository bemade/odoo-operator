#!/bin/bash
# Cutover step of an OdooStagingRefreshJob: swaps the cloned database
# ($TEMP_DB) into the staging instance's live database name ($TGT_DB).
#
# This is the only place a staging refresh writes the live name.  It runs as
# the main container of the refresh's final Job; when the refresh
# neutralizes (the default) scripts/neutralize.sh runs against $TEMP_DB as
# that Job's init container, and Kubernetes starts this container only once
# the init container has exited 0.  REQUIRE_NEUTRALIZED re-checks the
# neutralization flag here anyway, so nothing un-neutralized is ever renamed
# into the live name.
#
# Failure before the DROP leaves the live DB as it was and the temp DB in
# place, so the operator's neutralize retry can re-run this Job against it.
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection (the staging role)
#   TEMP_DB                    — database the clone was loaded into
#   TGT_DB                     — the staging instance's live database name
# Optional:
#   REQUIRE_NEUTRALIZED        — "true": refuse unless $TEMP_DB carries
#                                database.is_neutralized = true

set -euo pipefail
export PGPASSWORD=$PASSWORD

echo "=== Cutover: $TEMP_DB -> $TGT_DB ==="

if [ -z "$TEMP_DB" ] || [ "$TEMP_DB" = "$TGT_DB" ]; then
    echo "CRITICAL: TEMP_DB='$TEMP_DB' must be set and differ from TGT_DB='$TGT_DB'"
    exit 1
fi

# psql only interpolates :'var' / :"var" when reading from stdin, hence the
# heredocs.  -X skips any psqlrc; -t -A gives bare values.
TEMP_EXISTS=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres -X -t -A \
    -v ON_ERROR_STOP=1 -v temp_db="$TEMP_DB" <<'EOSQL'
SELECT count(*) FROM pg_database WHERE datname = :'temp_db';
EOSQL
)
if [ "$TEMP_EXISTS" != "1" ]; then
    echo "CRITICAL: temp database $TEMP_DB does not exist; live $TGT_DB left as it is"
    exit 1
fi

if [ "${REQUIRE_NEUTRALIZED:-}" = "true" ]; then
    NEUTRALIZED=$(psql -h "$HOST" -p "$PORT" -U "$USER" -d "$TEMP_DB" -X -t -A \
        -c "SELECT value FROM ir_config_parameter WHERE key = 'database.is_neutralized';" \
        2>/dev/null) || NEUTRALIZED=""
    if [ "$NEUTRALIZED" != "true" ] && [ "$NEUTRALIZED" != "True" ]; then
        echo "CRITICAL: $TEMP_DB is not neutralized (database.is_neutralized='$NEUTRALIZED');" \
             "refusing to cut over, live $TGT_DB left as it is"
        exit 1
    fi
    echo "$TEMP_DB is neutralized"
fi

# Two statements, each in its own implicit transaction: neither DROP
# DATABASE nor ALTER DATABASE ... RENAME may run inside a transaction block.
# A crash between them leaves no live DB and the temp DB intact; re-running
# this script finishes the job (the DROP is IF EXISTS).
psql -h "$HOST" -p "$PORT" -U "$USER" -d postgres -X -q \
    -v ON_ERROR_STOP=1 -v temp_db="$TEMP_DB" -v tgt_db="$TGT_DB" <<'EOSQL'
DROP DATABASE IF EXISTS :"tgt_db" WITH (FORCE);
ALTER DATABASE :"temp_db" RENAME TO :"tgt_db";
EOSQL

echo "=== Cutover complete: $TGT_DB now holds the refreshed database ==="
