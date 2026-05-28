#!/usr/bin/env bash
# Regression test for restore-extract.sh.
#
# The script runs as root (apk add unzip needs it) inside the operator's
# extract init container. Whatever it writes to the filestore PVC must be
# usable by the Odoo container (uid 100, gid 101). On cephfs the CSI driver
# honors fsGroup so kubelet papers over root-owned files; on JuiceFS RWX
# volumes fsGroup is skipped (fsGroupPolicy=ReadWriteOnceWithFSType) and
# root-owned files break Odoo with PermissionError.
#
# This test asserts the script's *contract*: after it runs, uid 100 must be
# able to write under filestore/$DB_NAME — without any fsGroup rescue.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/restore-extract.sh"

[ -f "$SCRIPT" ] || { echo "missing $SCRIPT" >&2; exit 1; }
command -v docker >/dev/null || { echo "docker required" >&2; exit 1; }
command -v zip    >/dev/null || { echo "zip required (apt install zip)" >&2; exit 1; }

WORKDIR=$(mktemp -d)
cleanup() {
    # Files written by root-uid containers can't be removed by the test user;
    # use a throwaway container to chown back before rm.
    docker run --rm -u 0 -v "$WORKDIR:/w" alpine:3.20 \
        sh -c "chown -R $(id -u):$(id -g) /w" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

DB_NAME=testdb

# Build a fixture zip: dump.sql + filestore/<sharding>/<file>.
FIX="$WORKDIR/fixture"
mkdir -p "$FIX/filestore/aa" "$FIX/filestore/bb"
echo "-- dump" > "$FIX/dump.sql"
echo "content-a" > "$FIX/filestore/aa/file1"
echo "content-b" > "$FIX/filestore/bb/file2"
(cd "$FIX" && zip -qr "$WORKDIR/artifact.zip" dump.sql filestore)

mkdir -p "$WORKDIR/workspace" "$WORKDIR/odoo"
cp "$WORKDIR/artifact.zip" "$WORKDIR/workspace/artifact"

echo "=== Running restore-extract.sh as root ==="
docker run --rm \
    -u 0 \
    -v "$WORKDIR/workspace:/workspace" \
    -v "$WORKDIR/odoo:/var/lib/odoo" \
    -v "$SCRIPT:/restore-extract.sh:ro" \
    -e DB_NAME="$DB_NAME" \
    -e INPUT_FILE=/workspace/artifact \
    alpine:3.20 sh /restore-extract.sh

echo "=== Asserting uid 100 (odoo) can write to filestore ==="
# Explicitly no fsGroup — we are testing the script's contract, not kubelet.
docker run --rm \
    -u 100:101 \
    -v "$WORKDIR/odoo:/var/lib/odoo" \
    alpine:3.20 sh -euxc "
        # The DB subdirectory itself must accept new files.
        touch /var/lib/odoo/filestore/$DB_NAME/.probe
        # Sharded subdirs (created by unzip) must accept new files too.
        touch /var/lib/odoo/filestore/$DB_NAME/aa/.probe
        # Every file/dir must be owned by uid 100 (the only way to guarantee
        # write access without leaning on fsGroup or world-write).
        wrong=\$(find /var/lib/odoo/filestore/$DB_NAME ! -user 100)
        if [ -n \"\$wrong\" ]; then
            echo 'FAIL: paths not owned by uid 100:'
            echo \"\$wrong\"
            exit 1
        fi
    "

echo "PASS: filestore usable by uid 100 without fsGroup"
