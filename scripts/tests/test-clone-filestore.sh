#!/usr/bin/env bash
# Regression test for clone-filestore.sh, the copy-path filestore step of a
# staging refresh.
#
# The script copies the source filestore into a sibling directory on the
# target volume, verifies that copy against the database's ir_attachment
# rows, and only then swaps it into place, so the target ends up an exact
# copy of the source (no stale target-only files) and is left untouched when
# anything fails.  As in test-rename-filestore.sh, the database is a `psql`
# stub answering the verification query from a fixture file, one
# "store_fname|file_size|epoch" line per referenced file.
#
# Covered branches:
#   1. Replace         — target-only files do not survive the refresh
#   2. Fresh target    — no target dir yet → created
#   3. Old file gone   — a referenced file older than the copy missing → fail,
#                        target untouched, no temp dir left behind
#   4. New file gone   — only files newer than the copy start missing → succeed
#   5. Empty source    — no source dir, old attachments referenced → fail,
#                        target untouched
#   6. Nothing needed  — no source dir, nothing referenced → empty target
#   7. Interrupted     — a swap interrupted between its two renames is
#                        rolled back before the next attempt copies
#   8. Unchanged files — reused from the old target (hard link), not copied,
#                        so the swap needs room for the delta only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/clone-filestore.sh"

[ -f "$SCRIPT" ] || { echo "missing $SCRIPT" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }

fail() { echo "FAIL: $1" >&2; exit 1; }

STUB_DIR=$(mktemp -d)
trap 'rm -rf "$STUB_DIR"' EXIT
cat > "$STUB_DIR/psql" <<'EOF'
#!/usr/bin/env bash
cat "$ATTACHMENTS_FILE"
EOF
chmod +x "$STUB_DIR/psql"
export PATH="$STUB_DIR:$PATH"
export HOST=stub PORT=5432 USER=stub PASSWORD=stub DB_NAME=odoo_tgt
export SRC_DB=odoo_src TGT_DB=odoo_tgt

NOW=$(date +%s)
OLD=$((NOW - 86400))     # long before the copy started
NEWER=$((NOW + 3600))    # written while (after) the copy ran

setup_case() {
    WORK=$(mktemp -d)
    export SRC_FILESTORE="$WORK/src" TGT_FILESTORE="$WORK/tgt"
    SRC="$SRC_FILESTORE/filestore/odoo_src"
    TGT="$TGT_FILESTORE/filestore/odoo_tgt"
    mkdir -p "$SRC_FILESTORE/filestore" "$TGT_FILESTORE/filestore"
    ATTACHMENTS_FILE=$(mktemp)
    export ATTACHMENTS_FILE
}
teardown_case() { rm -rf "$WORK" "$ATTACHMENTS_FILE"; }

# Reference every file under the source as old, with its real size.
reference_source() {
    (cd "$SRC" && find . -type f | sed 's|^\./||' | while read -r f; do
        echo "$f|$(wc -c < "$f" | tr -d ' ')|$OLD"
    done) >> "$ATTACHMENTS_FILE"
}

# Nothing but the target dir itself may be left in filestore/.
no_leftovers() {
    local extra
    extra=$(find "$TGT_FILESTORE/filestore" -mindepth 1 -maxdepth 1 ! -name odoo_tgt)
    [ -z "$extra" ] || fail "$1: left behind: $extra"
}

# ── Case 1: replace, not merge ───────────────────────────────────────────
setup_case
mkdir -p "$SRC/ab" "$TGT/ab" "$TGT/zz"
echo "from-src" > "$SRC/ab/f1"
echo "stale" > "$TGT/zz/stale"           # only on the target (an old staging file)
reference_source
bash "$SCRIPT" >/dev/null
[ "$(cat "$TGT/ab/f1")" = from-src ] || fail "case1: source file not copied"
[ ! -e "$TGT/zz/stale" ]              || fail "case1: stale target-only file survived"
no_leftovers case1
teardown_case
echo "ok: case1 target replaced, stale files dropped"

# ── Case 2: no target dir yet ────────────────────────────────────────────
setup_case
mkdir -p "$SRC/ab"
echo "from-src" > "$SRC/ab/f1"
reference_source
bash "$SCRIPT" >/dev/null
[ "$(cat "$TGT/ab/f1")" = from-src ] || fail "case2: target not created from source"
no_leftovers case2
teardown_case
echo "ok: case2 fresh target created"

# ── Case 3: an old referenced file missing from the source → fail ────────
setup_case
mkdir -p "$SRC/ab" "$TGT/ab"
echo "from-src" > "$SRC/ab/f1"
echo "previous" > "$TGT/ab/keep"
reference_source
echo "cd/gone|10|$OLD" >> "$ATTACHMENTS_FILE"
if bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case3: succeeded with an old referenced file missing"
fi
[ "$(cat "$TGT/ab/keep")" = previous ] || fail "case3: target modified on failure"
[ ! -e "$TGT/ab/f1" ]                  || fail "case3: partial copy swapped in"
no_leftovers case3
teardown_case
echo "ok: case3 incomplete copy fails, target untouched"

# ── Case 4: only files newer than the copy start missing → succeed ───────
setup_case
mkdir -p "$SRC/ab"
echo "from-src" > "$SRC/ab/f1"
reference_source
echo "cd/late|10|$NEWER" >> "$ATTACHMENTS_FILE"
OUT=$(bash "$SCRIPT" 2>&1) || { echo "$OUT" >&2; fail "case4: failed on a newer missing file"; }
echo "$OUT" | grep -q "1 attachment file(s) newer than the point in time, 1 of them absent" \
    || { echo "$OUT" >&2; fail "case4: newer missing file not reported"; }
[ "$(cat "$TGT/ab/f1")" = from-src ] || fail "case4: copy not swapped in"
teardown_case
echo "ok: case4 only newer files missing succeeds"

# ── Case 5: source absent while old attachments are referenced → fail ────
setup_case
mkdir -p "$TGT/ab"
echo "previous" > "$TGT/ab/keep"
echo "ab/f1|10|$OLD" > "$ATTACHMENTS_FILE"
if bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case5: empty source accepted although old attachments are referenced"
fi
[ "$(cat "$TGT/ab/keep")" = previous ] || fail "case5: target modified on failure"
no_leftovers case5
teardown_case
echo "ok: case5 empty source with old attachments fails, target untouched"

# ── Case 6: source absent, nothing referenced → empty target ─────────────
setup_case
mkdir -p "$TGT/ab"
echo "previous" > "$TGT/ab/keep"
bash "$SCRIPT" >/dev/null
[ -d "$TGT" ]          || fail "case6: target dir missing"
[ ! -e "$TGT/ab/keep" ] || fail "case6: target not replaced by the (empty) source"
no_leftovers case6
teardown_case
echo "ok: case6 attachment-free source yields an empty target"

# ── Case 7: swap interrupted between its two renames ─────────────────────
# A pod killed after moving the old target aside but before moving the new
# copy in leaves no target dir at all.  The retry must restore the old one
# first, so a second failure would still leave it in place.
setup_case
mkdir -p "$SRC/ab" "$TGT_FILESTORE/filestore/odoo_tgt.refresh-old/ab" \
         "$TGT_FILESTORE/filestore/odoo_tgt.refresh-new/ab"
echo "from-src" > "$SRC/ab/f1"
echo "previous" > "$TGT_FILESTORE/filestore/odoo_tgt.refresh-old/ab/keep"
echo "half"     > "$TGT_FILESTORE/filestore/odoo_tgt.refresh-new/ab/junk"
reference_source
echo "cd/gone|10|$OLD" >> "$ATTACHMENTS_FILE"   # this attempt fails too
if bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case7: succeeded with an old referenced file missing"
fi
[ "$(cat "$TGT/ab/keep" 2>/dev/null)" = previous ] \
    || fail "case7: interrupted swap not rolled back to the old target"
no_leftovers case7
teardown_case
echo "ok: case7 interrupted swap rolled back"

# ── Case 8: unchanged files reused from the old target ───────────────────
# Filestore names are content hashes, so a same-named, same-sized file in the
# old target is the same content.  Linking it instead of copying keeps the
# swap's peak usage at old + delta rather than twice the filestore.
setup_case
mkdir -p "$SRC/ab" "$TGT/ab"
echo "same" > "$SRC/ab/same"
echo "same" > "$TGT/ab/same"
echo "new-in-src" > "$SRC/ab/new"
reference_source
inode() { python3 -c 'import os, sys; print(os.stat(sys.argv[1]).st_ino)' "$1"; }
before=$(inode "$TGT/ab/same")
bash "$SCRIPT" >/dev/null
[ "$(inode "$TGT/ab/same")" = "$before" ] || fail "case8: unchanged file copied, not reused"
[ "$(cat "$TGT/ab/new")" = new-in-src ]       || fail "case8: new file not copied"
teardown_case
echo "ok: case8 unchanged files reused from the old target"

echo "PASS: all clone-filestore.sh cases"
