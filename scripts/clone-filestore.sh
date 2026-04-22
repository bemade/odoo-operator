#!/bin/sh
# Copies the source OdooInstance's filestore into the staging instance's
# filestore PVC.  Both PVCs are mounted on this single pod; copy is
# entirely local to the pod.  Filestore entries are content-addressable
# (sha1 filenames), so merging source into target is intrinsically safe —
# any name collision refers to identical content by construction.
#
# Required env vars:
#   SRC_DB, TGT_DB  — database names used as subdirectories under each PVC
#   SRC_FILESTORE   — mount path of the source filestore PVC (e.g. /src)
#   TGT_FILESTORE   — mount path of the target filestore PVC (e.g. /tgt)
#
# V1 limitation: both PVCs must be in the same namespace (K8s PVCs are
# namespace-scoped).  Cross-namespace support is a future enhancement
# using pod-to-pod rsync-over-network.

set -euo pipefail

SRC_DIR="${SRC_FILESTORE}/filestore/${SRC_DB}"
TGT_DIR="${TGT_FILESTORE}/filestore/${TGT_DB}"

echo "=== Clone filestore: $SRC_DIR -> $TGT_DIR ==="

if [ ! -d "$SRC_DIR" ]; then
    # Source filestore is empty (unusual but not fatal — brand-new prod
    # may have zero attachments).  Ensure target dir exists and exit.
    echo "Source filestore directory absent — nothing to copy"
    mkdir -p "$TGT_DIR"
    exit 0
fi

mkdir -p "$TGT_DIR"

# shutil.copytree with dirs_exist_ok=True merges source into target,
# overwriting files with the same name (safe: content-addressable) and
# leaving target-only files in place.  Matches the rsync -a semantics we
# use in restore.sh.  rsync isn't in the odoo image.
python3 -c "
import shutil, sys
shutil.copytree(sys.argv[1], sys.argv[2], dirs_exist_ok=True)
" "$SRC_DIR" "$TGT_DIR"

echo "=== Filestore clone complete ==="
du -sh "$TGT_DIR" 2>/dev/null || true
