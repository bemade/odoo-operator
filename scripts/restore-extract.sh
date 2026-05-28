#!/bin/sh
# Extracts a zip backup: dump.sql to /workspace, filestore/* directly to the
# target filestore PVC via a symlink (no intermediate copy).
# Runs in alpine; installs `unzip` on demand.
#
# Only invoked when BACKUP_FORMAT=zip.  For sql/dump formats the artifact is
# already in place at /workspace/dump.{sql,dump} and this step is skipped.
#
# Required env vars:
#   DB_NAME    — target database (used as filestore subdirectory name)
#   INPUT_FILE — path to the zip artifact (e.g. /workspace/artifact)

set -ex

apk add --no-cache unzip > /dev/null

INPUT_FILE="${INPUT_FILE:-/workspace/artifact}"
[ -f "$INPUT_FILE" ] || { echo "missing $INPUT_FILE" >&2; exit 1; }

echo "=== Extracting dump.sql to /workspace ==="
unzip -o "$INPUT_FILE" dump.sql -d /workspace/

echo "=== Streaming filestore content into PVC at /var/lib/odoo/filestore/$DB_NAME ==="
mkdir -p "/var/lib/odoo/filestore/$DB_NAME"
ln -sfn "/var/lib/odoo/filestore/$DB_NAME" /tmp/filestore
# unzip writes through the symlink directly into the PVC.  The 'filestore/*'
# pattern is permissive — if the archive has no filestore entries (DB-only
# zip), unzip exits 11 (nothing matched) which we treat as non-fatal.
unzip -o "$INPUT_FILE" 'filestore/*' -d /tmp/ || rc=$?
if [ -n "${rc:-}" ] && [ "$rc" -ne 0 ] && [ "$rc" -ne 11 ]; then
    echo "unzip filestore/* failed (rc=$rc)" >&2
    exit "$rc"
fi

ls -l /workspace/dump.sql

# The script runs as root so apk can install unzip, which means everything
# unzip wrote — including the mkdir'd parent — is owned root.  The Odoo
# container runs as uid 100 (odoo) and must be able to write here.  On
# cephfs the CSI driver's fsGroupPolicy=File lets kubelet rescue this with
# a recursive chown, but JuiceFS RWX volumes (fsGroupPolicy=ReadWriteOnceWithFSType)
# skip fsGroup entirely.  Chown unconditionally so the script's output is
# usable regardless of the underlying CSI driver.
chown -R 100:101 "/var/lib/odoo/filestore/$DB_NAME"

echo "=== Extract complete ==="
