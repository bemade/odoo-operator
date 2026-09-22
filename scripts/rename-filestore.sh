#!/bin/bash
# Renames the filestore subdirectory on the staging instance's filestore PVC
# from the *source* database name to the *target* database name, after the
# snapshot-path refresh has cloned the source PVC verbatim.
#
# The snapshot path (CSI VolumeSnapshot → dataSourceRef) clones the source
# filestore PVC byte-for-byte, so the filestore lands under the *source* db
# name (e.g. filestore/odoo_<prod-uid>).  Odoo on staging looks under the
# *target* db name (filestore/odoo_<staging-uid>) and would otherwise find
# nothing — every attachment missing, web assets regenerated from scratch.
# This step reconciles the directory name so the cloned DB and filestore
# align.  The copy path (clone-filestore.sh) already maps SRC_DB → TGT_DB at
# copy time; this is its snapshot-path equivalent.
#
# ── Why this script verifies instead of trusting the mount ──────────────────
#
# A PVC provisioned from a VolumeSnapshot can be Bound, mountable and EMPTY.
# JuiceFS CSI returns ProvisioningSucceeded as soon as it has *scheduled* the
# restore, then performs the clone in a background Job that takes minutes
# (measured: ~11 min for a 67k-file filestore).  Worse, that Job rmdir()s the
# target subPath before cloning into it, so a pod that mounted beforehand is
# bind-mounted to an unlinked inode: it reports a healthy, writable, empty
# mount forever.  The poisoning is per-node and sticky, because JuiceFS shares
# one mount pod per (node, volume).
#
# Two consequences drive the logic below:
#
#   1. DO NOT WAIT IN-POD.  Polling for the tree to appear cannot work — this
#      pod's view will never change.  Fail fast instead and let the Job's
#      backoffLimit start a *new* pod, which gets a fresh mount.
#   2. NEVER ASSUME AN EMPTY FILESTORE IS CORRECT.  An absent source directory
#      is indistinguishable, from the filesystem alone, between "prod genuinely
#      has no attachments" and "the clone has not landed".  Guessing optimistically
#      yields a green refresh with every attachment silently missing.  The
#      database is the only authority: ir_attachment says what must be present.
#
# Verification runs BEFORE the rename, so an incomplete tree is never mutated.
#
# Runs as root.  The CSI snapshot clone can hand the whole tree back as
# root:root (JuiceFS CSI < v0.31.6 runs `juicefs clone` without -p), and
# kubelet never applies fsGroup on a JuiceFS RWX mount
# (fsGroupPolicy=ReadWriteOnceWithFSType), so as uid 100 the rename below
# dies with EACCES and Odoo could not write the tree afterwards either.
# Same trap restore-extract.sh documents; same remedy: chown at the end.
#
# Required env vars:
#   SRC_DB, TGT_DB             — database names used as subdirectories under the PVC
#   FILESTORE                  — mount path of the filestore PVC (e.g. /var/lib/odoo)
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection for the verification query
#   DB_NAME                    — target database, authority for what must be present
# Optional:
#   FILESTORE_OWNER — uid:gid to hand the tree to (default 100:101, odoo)

set -euo pipefail

MOUNT="${FILESTORE:-/var/lib/odoo}"
FS="${MOUNT}/filestore"
OWNER="${FILESTORE_OWNER:-100:101}"
SRC_DIR="${FS}/${SRC_DB}"
TGT_DIR="${FS}/${TGT_DB}"

export PGPASSWORD="$PASSWORD"

psql_at() {
    psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" -At -c "$1"
}

# Hand the whole mount — not just filestore/<tgt> — to the odoo uid.  Odoo
# lazily mkdirs sessions/ at the mount root on first request, and a
# root-owned, non-world-writable root there is the health-500 in #156.
finish() {
    chown -R "$OWNER" "$MOUNT"
    echo "=== Filestore rename complete (owner $OWNER) ==="
    du -sh "$TGT_DIR" 2>/dev/null || true
}

# Every ir_attachment row with a store_fname must resolve to a real file under
# $1.  store_fname is content-addressed ("<2-char prefix>/<sha1>"), so this is
# an exact statement of what Odoo will try to read, not a proxy for it.  Size
# is checked too: a half-written file exists but is short.
verify_filestore() {
    psql_at "SELECT DISTINCT store_fname || '|' || COALESCE(file_size, -1) \
             FROM ir_attachment WHERE store_fname IS NOT NULL" \
    | python3 -c '
import os, sys
root = sys.argv[1]
total = missing = short = 0
examples = []
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    name, _, size = line.rpartition("|")
    total += 1
    path = os.path.join(root, name)
    try:
        st = os.stat(path)
    except OSError:
        missing += 1
        if len(examples) < 5:
            examples.append("missing " + name)
        continue
    try:
        expected = int(size)
    except ValueError:
        expected = -1
    if expected >= 0 and st.st_size != expected:
        short += 1
        if len(examples) < 5:
            examples.append(f"{name} is {st.st_size}B, expected {expected}B")
print(f"verified {total - missing - short}/{total} attachment files under {root}")
if missing or short:
    sys.stderr.write(
        f"FILESTORE INCOMPLETE: {missing} missing, {short} wrong-sized "
        f"(of {total} expected).\n"
    )
    for e in examples:
        sys.stderr.write("  - " + e + "\n")
    sys.stderr.write(
        "The snapshot clone has almost certainly not finished. This pod cannot "
        "observe it finishing -- its mount is pinned to the pre-clone inode -- "
        "so failing now so a fresh pod retries against a fresh mount.\n"
    )
    sys.exit(1)
' "$1"
}

echo "=== Rename filestore: $SRC_DIR -> $TGT_DIR ==="

EXPECTED="$(psql_at "SELECT count(*) FROM ir_attachment WHERE store_fname IS NOT NULL")"
echo "Database $DB_NAME references $EXPECTED attachment file(s)"

if [ "$SRC_DB" = "$TGT_DB" ]; then
    # Source and target share a db name (e.g. spec.database.name pinned to
    # the same value on both instances).  The snapshot already placed the
    # filestore under the right name — nothing to rename, but still verify.
    echo "Source and target db names identical — nothing to rename"
    mkdir -p "$TGT_DIR"
    VERIFY_DIR="$TGT_DIR"
elif [ -d "$SRC_DIR" ]; then
    VERIFY_DIR="$SRC_DIR"
elif [ -d "$TGT_DIR" ]; then
    # An earlier attempt of this Job already renamed the tree.
    echo "Source absent, target present — already renamed by an earlier attempt"
    VERIFY_DIR="$TGT_DIR"
elif [ "$EXPECTED" -eq 0 ]; then
    # Genuinely nothing to copy: the database references no stored files.
    # This is the ONLY case where an absent filestore is correct.
    echo "No filestore present and no attachments referenced — nothing to do"
    mkdir -p "$TGT_DIR"
    VERIFY_DIR="$TGT_DIR"
else
    echo "ERROR: neither $SRC_DIR nor $TGT_DIR exists, but $DB_NAME references" >&2
    echo "       $EXPECTED attachment file(s). The snapshot clone has not landed" >&2
    echo "       on this mount. Failing so a fresh pod retries with a fresh mount." >&2
    exit 1
fi

# Verify BEFORE mutating, so a partially-populated tree is never renamed.
verify_filestore "$VERIFY_DIR"

if [ "$VERIFY_DIR" = "$SRC_DIR" ]; then
    if [ ! -e "$TGT_DIR" ]; then
        # Fast path: atomic rename within the same filesystem.
        mv "$SRC_DIR" "$TGT_DIR"
    else
        # Target already exists (e.g. Odoo regenerated a few asset bundles
        # against the staging db before the rename).  Merge source into target
        # — filestore entries are content-addressable (sha1 filenames), so any
        # name collision refers to identical content by construction — then drop
        # the now-redundant source dir.  shutil.copytree(dirs_exist_ok=True)
        # matches the rsync -a semantics used elsewhere; rsync isn't in the odoo
        # image.
        python3 -c "
import shutil, sys
shutil.copytree(sys.argv[1], sys.argv[2], dirs_exist_ok=True)
" "$SRC_DIR" "$TGT_DIR"
        rm -rf "$SRC_DIR"
    fi
fi

finish
