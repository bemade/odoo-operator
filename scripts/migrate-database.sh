#!/bin/sh
# Migrates an Odoo database from one PostgreSQL cluster to another.
#
# Creates the role on the destination cluster, pipes pg_dump | pg_restore,
# and verifies the result by checking that the Odoo base tables exist.
#
# Required env vars:
#   SRC_HOST, SRC_PORT, SRC_ADMIN_USER, SRC_ADMIN_PASSWORD — source cluster admin
#   DST_HOST, DST_PORT, DST_ADMIN_USER, DST_ADMIN_PASSWORD — destination cluster admin
#   DB_USER, DB_PASSWORD — Odoo database role credentials
#   DB_NAME              — database name to migrate

set -ex

echo "=== Starting database cluster migration ==="
echo "Source:      $SRC_HOST:$SRC_PORT"
echo "Destination: $DST_HOST:$DST_PORT"
echo "Database:    $DB_NAME"
echo "Role:        $DB_USER"

# 1. Create role on destination cluster (idempotent).
echo "=== Ensuring role on destination cluster ==="
PGPASSWORD=$DST_ADMIN_PASSWORD psql \
  -h "$DST_HOST" -p "$DST_PORT" -U "$DST_ADMIN_USER" -d postgres \
  -c "DO \$\$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '$DB_USER') THEN CREATE ROLE \"$DB_USER\" WITH PASSWORD '$DB_PASSWORD' CREATEDB LOGIN; END IF; END \$\$;"

# 2. Drop target database on destination if exists (idempotent).
echo "=== Dropping existing database on destination (if any) ==="
PGPASSWORD=$DST_ADMIN_PASSWORD psql \
  -h "$DST_HOST" -p "$DST_PORT" -U "$DST_ADMIN_USER" -d postgres \
  -c "DROP DATABASE IF EXISTS \"$DB_NAME\"" || true

# 3. Create empty database on destination owned by the role.
echo "=== Creating database on destination ==="
PGPASSWORD=$DST_ADMIN_PASSWORD createdb \
  -h "$DST_HOST" -p "$DST_PORT" -U "$DST_ADMIN_USER" -O "$DB_USER" "$DB_NAME"

# 4. Pipe pg_dump from source into pg_restore on destination.
#    Dump with admin creds (full access to read), restore with the Odoo user
#    so tables are owned by the correct role.
echo "=== Migrating data (pg_dump | pg_restore) ==="
PGPASSWORD=$SRC_ADMIN_PASSWORD pg_dump \
  -h "$SRC_HOST" -p "$SRC_PORT" -U "$SRC_ADMIN_USER" -d "$DB_NAME" \
  --format=custom --no-owner | \
PGPASSWORD=$DB_PASSWORD pg_restore \
  -h "$DST_HOST" -p "$DST_PORT" -U "$DB_USER" -d "$DB_NAME" \
  --no-owner || true

# 5. Verify the migrated database has core Odoo tables.
echo "=== Verifying migration integrity ==="
TABLE_COUNT=$(PGPASSWORD=$DB_PASSWORD psql \
  -h "$DST_HOST" -p "$DST_PORT" -U "$DB_USER" -d "$DB_NAME" -t -A \
  -c "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name IN ('ir_module_module', 'res_users', 'ir_config_parameter');" 2>/dev/null || echo "0")
if [ "$TABLE_COUNT" -lt 3 ]; then
    echo "FATAL: migration verification failed — expected core tables not found (count=$TABLE_COUNT)"
    echo "Cleaning up failed database on destination..."
    PGPASSWORD=$DST_ADMIN_PASSWORD psql \
      -h "$DST_HOST" -p "$DST_PORT" -U "$DST_ADMIN_USER" -d postgres \
      -c "DROP DATABASE IF EXISTS \"$DB_NAME\"" || true
    exit 1
fi
echo "Verification passed: $TABLE_COUNT/3 core tables found"

echo "=== Database cluster migration complete ==="
