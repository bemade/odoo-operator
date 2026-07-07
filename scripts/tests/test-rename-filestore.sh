#!/usr/bin/env bash
# Regression test for rename-filestore.sh.
#
# The snapshot-path staging refresh clones the source filestore PVC
# byte-for-byte, so the filestore lands under the *source* DB name.  This
# script renames it to the *target* DB name in place so Odoo can resolve
# filestore/<db_name>.  It runs entirely on the local filesystem (bash +
# python3), so — unlike test-restore-extract.sh — no docker is needed.
#
# Covered branches:
#   1. Fast path      — target absent → atomic mv src → tgt
#   2. Merge path     — target pre-exists → copytree merge + drop src
#   3. Identical names— SRC_DB == TGT_DB → no-op, ensure tgt exists
#   4. Source absent  — src dir missing → no-op, ensure tgt exists

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/rename-filestore.sh"

[ -f "$SCRIPT" ] || { echo "missing $SCRIPT" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }

fail() { echo "FAIL: $1" >&2; exit 1; }

# Each case gets a fresh filestore root under a temp dir.
setup_case() {
    ROOT=$(mktemp -d)
    FS="$ROOT/filestore"
    mkdir -p "$FS"
}
teardown_case() { rm -rf "$ROOT"; }

# ── Case 1: fast path (mv) ───────────────────────────────────────────────
setup_case
mkdir -p "$FS/odoo_src"
echo "content-a" > "$FS/odoo_src/aa"
echo "content-b" > "$FS/odoo_src/bb"
SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ ! -e "$FS/odoo_src" ]           || fail "case1: source dir not removed after mv"
[ -f "$FS/odoo_tgt/aa" ]          || fail "case1: aa missing after rename"
[ -f "$FS/odoo_tgt/bb" ]          || fail "case1: bb missing after rename"
[ "$(cat "$FS/odoo_tgt/aa")" = content-a ] || fail "case1: aa content corrupted"
teardown_case
echo "ok: case1 fast-path mv"

# ── Case 2: merge path (target pre-exists) ───────────────────────────────
# Odoo regenerated a couple of asset bundles under the target name before the
# rename.  The synced source must merge in without clobbering target-only
# files; content-addressable collisions (same sha1 filename) are identical by
# construction, so overwriting is safe.
setup_case
mkdir -p "$FS/odoo_src" "$FS/odoo_tgt"
echo "shared" > "$FS/odoo_src/shared_sha"   # exists in both (identical content)
echo "shared" > "$FS/odoo_tgt/shared_sha"
echo "from-src-only" > "$FS/odoo_src/src_only"
echo "from-tgt-only" > "$FS/odoo_tgt/tgt_only"
SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ ! -e "$FS/odoo_src" ]                       || fail "case2: source dir not removed after merge"
[ -f "$FS/odoo_tgt/src_only" ]                || fail "case2: source-only file not merged in"
[ -f "$FS/odoo_tgt/tgt_only" ]                || fail "case2: target-only file clobbered"
[ "$(cat "$FS/odoo_tgt/src_only")" = from-src-only ] || fail "case2: src_only content wrong"
[ "$(cat "$FS/odoo_tgt/tgt_only")" = from-tgt-only ] || fail "case2: tgt_only content wrong"
[ "$(cat "$FS/odoo_tgt/shared_sha")" = shared ]      || fail "case2: shared file content wrong"
teardown_case
echo "ok: case2 merge-path copytree"

# ── Case 3: identical DB names (no-op) ───────────────────────────────────
setup_case
mkdir -p "$FS/odoo_same"
echo "keep" > "$FS/odoo_same/keep"
SRC_DB=odoo_same TGT_DB=odoo_same FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ -f "$FS/odoo_same/keep" ]                || fail "case3: identical-name run disturbed files"
[ "$(cat "$FS/odoo_same/keep")" = keep ]   || fail "case3: identical-name content changed"
teardown_case
echo "ok: case3 identical-names no-op"

# ── Case 4: source absent (no-op, ensure target exists) ──────────────────
setup_case
SRC_DB=odoo_missing TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ -d "$FS/odoo_tgt" ]        || fail "case4: target dir not created when source absent"
[ ! -e "$FS/odoo_missing" ] || fail "case4: phantom source dir created"
teardown_case
echo "ok: case4 source-absent no-op"

echo "PASS: all rename-filestore.sh cases"
