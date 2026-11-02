#!/bin/bash
# Copies the source OdooInstance's filestore into the staging instance's
# filestore PVC (the copy path of a staging refresh).  Both PVCs are mounted
# on this single pod; the copy is entirely local to the pod.
#
# The copy REPLACES the target filestore rather than merging into it, and is
# verified before it replaces anything:
#
#   1. copy filestore/<SRC_DB> into a sibling, filestore/<TGT_DB>.refresh-new,
#      on the target volume;
#   2. verify that copy against ir_attachment (filestore-verify.sh): every
#      file the database references must be there at its recorded size;
#   3. only then swap it in for filestore/<TGT_DB> and drop the old tree.
#
# Any failure before the swap leaves the existing target untouched.  A merge
# would let target-only files from the previous staging run survive, and an
# unverified copy can come out short (an empty or wrong source volume) and
# still report success.
#
# Unchanged files are hard-linked from the old target instead of copied.
# Filestore names are content hashes ("<2-char prefix>/<sha1>"), so a
# same-named file of the same size IS the same content; linking keeps the
# swap's peak usage at the old tree plus the delta, not twice the filestore,
# and makes a repeat refresh copy only what changed.
#
# Point in time: the copy walks the live source while production keeps
# writing and while the DB dump runs, so attachments created after the copy
# starts may be in the database but not in the copy.  FILESTORE_PIT is taken
# here, before copying, and exempts them; see filestore-verify.sh.
#
# Required env vars:
#   SRC_DB, TGT_DB  — database names used as subdirectories under each PVC
#   SRC_FILESTORE   — mount path of the source filestore PVC (e.g. /src)
#   TGT_FILESTORE   — mount path of the target filestore PVC (e.g. /tgt)
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection for verification
#   DB_NAME         — database whose ir_attachment is the authority
# Optional:
#   FILESTORE_PIT   — override the copy-start point in time (RFC 3339 UTC)
#
# V1 limitation: both PVCs must be in the same namespace (K8s PVCs are
# namespace-scoped).  Cross-namespace support is a future enhancement
# using pod-to-pod rsync-over-network.

set -euo pipefail

# verify_filestore comes from filestore-verify.sh.  In a Job pod the operator
# prepends that file to this script, so it is already defined; run from a
# checkout, source it from alongside this script.
if ! declare -F verify_filestore >/dev/null; then
    # shellcheck source=filestore-verify.sh
    . "$(dirname "${BASH_SOURCE[0]}")/filestore-verify.sh"
fi

SRC_DIR="${SRC_FILESTORE}/filestore/${SRC_DB}"
TGT_DIR="${TGT_FILESTORE}/filestore/${TGT_DB}"
NEW_DIR="${TGT_DIR}.refresh-new"
OLD_DIR="${TGT_DIR}.refresh-old"

echo "=== Clone filestore: $SRC_DIR -> $TGT_DIR (verified against $DB_NAME) ==="

# An earlier attempt killed between the swap's two renames left no target at
# all: put the old one back before anything else, so the target is never
# lost to an interrupted attempt.  Then clear any half-built copy.
if [ ! -e "$TGT_DIR" ] && [ -d "$OLD_DIR" ]; then
    echo "Restoring the previous target from an interrupted swap"
    mv "$OLD_DIR" "$TGT_DIR"
fi
rm -rf "$NEW_DIR" "$OLD_DIR"
mkdir -p "${TGT_FILESTORE}/filestore"

FILESTORE_PIT="${FILESTORE_PIT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
export FILESTORE_PIT

if [ -d "$SRC_DIR" ]; then
    # copytree's copy_function links a same-named, same-sized file from the
    # old target (identical by content addressing) and copies the rest.  A
    # failed link (e.g. a filesystem without hard links) falls back to a copy.
    python3 -c '
import os, shutil, sys
src, new, old = sys.argv[1:4]
linked = copied = 0
def place(s, d):
    global linked, copied
    candidate = os.path.join(old, os.path.relpath(d, new))
    try:
        if os.stat(candidate).st_size == os.stat(s).st_size:
            os.link(candidate, d)
            linked += 1
            return d
    except OSError:
        pass
    copied += 1
    return shutil.copy2(s, d)
shutil.copytree(src, new, copy_function=place)
print(f"copied {copied} file(s), reused {linked} unchanged file(s) from the old target")
' "$SRC_DIR" "$NEW_DIR" "$TGT_DIR"
else
    # Correct only if the database references no file older than the copy;
    # the verification below decides.
    echo "Source filestore directory absent"
    mkdir -p "$NEW_DIR"
fi

if ! verify_filestore "$NEW_DIR"; then
    rm -rf "$NEW_DIR"
    echo "ERROR: the copy of $SRC_DIR is incomplete; $TGT_DIR left untouched." >&2
    exit 1
fi

# Swap.  Each mv is a rename within filestore/ on one volume.
if [ -e "$TGT_DIR" ]; then
    mv "$TGT_DIR" "$OLD_DIR"
fi
mv "$NEW_DIR" "$TGT_DIR"
rm -rf "$OLD_DIR"

echo "=== Filestore clone complete ==="
du -sh "$TGT_DIR" 2>/dev/null || true
