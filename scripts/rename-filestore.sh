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
# Required env vars:
#   SRC_DB, TGT_DB  — database names used as subdirectories under the PVC
#   FILESTORE       — mount path of the filestore PVC (e.g. /var/lib/odoo)

set -euo pipefail

FS="${FILESTORE:-/var/lib/odoo}/filestore"
SRC_DIR="${FS}/${SRC_DB}"
TGT_DIR="${FS}/${TGT_DB}"

echo "=== Rename filestore: $SRC_DIR -> $TGT_DIR ==="

if [ "$SRC_DB" = "$TGT_DB" ]; then
    # Source and target share a db name (e.g. spec.database.name pinned to
    # the same value on both instances).  The snapshot already placed the
    # filestore under the right name — nothing to do.
    echo "Source and target db names identical — nothing to rename"
    mkdir -p "$TGT_DIR"
    exit 0
fi

if [ ! -d "$SRC_DIR" ]; then
    # Source filestore absent (brand-new prod with zero attachments, or an
    # already-renamed PVC from a retried Job).  Ensure the target exists.
    echo "Source filestore directory absent — ensuring target exists"
    mkdir -p "$TGT_DIR"
    exit 0
fi

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

echo "=== Filestore rename complete ==="
du -sh "$TGT_DIR" 2>/dev/null || true
