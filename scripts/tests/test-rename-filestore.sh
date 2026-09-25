#!/usr/bin/env bash
# Regression test for rename-filestore.sh.
#
# The snapshot-path staging refresh clones the source filestore PVC
# byte-for-byte, so the filestore lands under the *source* DB name.  This
# script renames it to the *target* DB name in place so Odoo can resolve
# filestore/<db_name>.  Before renaming, it verifies the tree against the
# database's ir_attachment rows.  The database is replaced here by a `psql`
# stub that answers the script's two queries from a fixture file, so the
# test runs entirely on the local filesystem (bash + python3) — unlike
# test-restore-extract.sh, no docker is needed except for case 5.
#
# Covered branches:
#   1. Fast path      — target absent → atomic mv src → tgt
#   2. Merge path     — target pre-exists → copytree merge + drop src
#   3. Identical names— SRC_DB == TGT_DB → no-op, verify in place
#   4. Nothing to do  — no dirs and no attachments referenced → create tgt
#   5. Ownership      — root-owned clone → uid 100 can write afterwards
#                       (docker; skipped when docker is unavailable)
#   6. Not landed     — no dirs but attachments referenced → fail, create nothing
#   7. Incomplete     — a referenced file missing → fail, source untouched
#   8. Truncated      — a referenced file has the wrong size → fail
#   9. Retried Job    — source gone, target present → verify target, succeed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/rename-filestore.sh"

[ -f "$SCRIPT" ] || { echo "missing $SCRIPT" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }

fail() { echo "FAIL: $1" >&2; exit 1; }

# Cases run unprivileged, so point the script's final chown at ourselves
# (a no-op for a non-root caller).  Case 5 exercises the real 100:101 contract.
export FILESTORE_OWNER="$(id -u):$(id -g)"

# ── psql stub ────────────────────────────────────────────────────────────
# The script asks the database two things: how many ir_attachment rows have a
# store_fname, and the "store_fname|file_size" list it verifies the tree
# against.  The stub answers both from $ATTACHMENTS_FILE, one
# "store_fname|file_size" line per attachment.
STUB_DIR=$(mktemp -d)
trap 'rm -rf "$STUB_DIR"' EXIT
cat > "$STUB_DIR/psql" <<'EOF'
#!/usr/bin/env bash
query="${*: -1}"
case "$query" in
    *"count(*)"*) grep -c . "$ATTACHMENTS_FILE" || true ;;
    *) cat "$ATTACHMENTS_FILE" ;;
esac
EOF
chmod +x "$STUB_DIR/psql"
export PATH="$STUB_DIR:$PATH"
export HOST=stub PORT=5432 USER=stub PASSWORD=stub DB_NAME=odoo_tgt

# Each case gets a fresh filestore root under a temp dir, and an empty
# attachment list (a database with no stored files) until it says otherwise.
setup_case() {
    ROOT=$(mktemp -d)
    FS="$ROOT/filestore"
    mkdir -p "$FS"
    # Outside $ROOT: case 5's fixture chowns that whole tree to root.
    ATTACHMENTS_FILE=$(mktemp)
    export ATTACHMENTS_FILE
}
teardown_case() { rm -rf "$ROOT" "$ATTACHMENTS_FILE"; }

# Record every file under $1 as an attachment the database references, with
# its real size, so the tree verifies as complete.
reference_files_in() {
    (cd "$1" && find . -type f | sed 's|^\./||' | while read -r f; do
        echo "$f|$(wc -c < "$f" | tr -d ' ')"
    done) >> "$ATTACHMENTS_FILE"
}

# ── Case 1: fast path (mv) ───────────────────────────────────────────────
setup_case
mkdir -p "$FS/odoo_src"
echo "content-a" > "$FS/odoo_src/aa"
echo "content-b" > "$FS/odoo_src/bb"
reference_files_in "$FS/odoo_src"
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
reference_files_in "$FS/odoo_src"
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
reference_files_in "$FS/odoo_same"
SRC_DB=odoo_same TGT_DB=odoo_same FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ -f "$FS/odoo_same/keep" ]                || fail "case3: identical-name run disturbed files"
[ "$(cat "$FS/odoo_same/keep")" = keep ]   || fail "case3: identical-name content changed"
teardown_case
echo "ok: case3 identical-names no-op"

# ── Case 4: nothing to do (no dirs, no attachments referenced) ───────────
# The only case where an absent filestore is correct.
setup_case
SRC_DB=odoo_missing TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ -d "$FS/odoo_tgt" ]        || fail "case4: target dir not created for an attachment-free db"
[ ! -e "$FS/odoo_missing" ] || fail "case4: phantom source dir created"
teardown_case
echo "ok: case4 no-attachments no-op"

# ── Case 5: ownership contract (docker, runs the script as root) ─────────
# A JuiceFS CSI snapshot clone (driver < v0.31.6 runs `juicefs clone` without
# -p) hands back the whole tree root:root, and kubelet never applies fsGroup
# on that RWX mount (fsGroupPolicy=ReadWriteOnceWithFSType).  The Job
# therefore runs as root, and its contract is: after it returns, uid 100
# (odoo) can create entries both under filestore/<tgt> AND at the mount root
# (Odoo lazily mkdirs sessions/ there — issue #156).  Case 1 already proves
# the rename itself; this case proves the rename does not die on EACCES and
# leaves a usable tree behind.
if command -v docker >/dev/null 2>&1; then
    setup_case
    cleanup_case5() {
        docker run --rm -u 0 -v "$ROOT:/w" alpine:3.20 \
            sh -c "chown -R $(id -u):$(id -g) /w" 2>/dev/null || true
        teardown_case
    }
    # Build the root-owned 0755 fixture the way the clone hands it to us.
    docker run --rm -u 0 -v "$ROOT:/var/lib/odoo" alpine:3.20 sh -c '
        mkdir -p /var/lib/odoo/filestore/odoo_src/aa &&
        echo content-a > /var/lib/odoo/filestore/odoo_src/aa/f1 &&
        chown -R 0:0 /var/lib/odoo && chmod -R u=rwX,go=rX /var/lib/odoo'
    echo "aa/f1|10" > "$ATTACHMENTS_FILE"
    docker run --rm -u 0 \
        -v "$ROOT:/var/lib/odoo" \
        -v "$SCRIPT:/rename-filestore.sh:ro" \
        -v "$STUB_DIR:/stub:ro" \
        -v "$ATTACHMENTS_FILE:/attachments:ro" \
        -e PATH=/stub:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
        -e ATTACHMENTS_FILE=/attachments \
        -e HOST -e PORT -e USER -e PASSWORD -e DB_NAME \
        -e SRC_DB=odoo_src -e TGT_DB=odoo_tgt -e FILESTORE=/var/lib/odoo \
        alpine:3.20 sh -c 'apk add -q bash python3 && bash /rename-filestore.sh' >/dev/null \
        || { cleanup_case5; fail "case5: script failed on a root-owned tree"; }
    # Explicitly no fsGroup — the script's contract, not kubelet's.
    docker run --rm -u 100:101 -v "$ROOT:/var/lib/odoo" alpine:3.20 sh -c '
        [ "$(cat /var/lib/odoo/filestore/odoo_tgt/aa/f1)" = content-a ] || exit 10
        touch /var/lib/odoo/filestore/odoo_tgt/aa/f2 || exit 11
        mkdir /var/lib/odoo/sessions || exit 12
        mkdir /var/lib/odoo/filestore/odoo_tgt/bb || exit 13' \
        || { rc=$?; cleanup_case5; fail "case5: uid 100 cannot use the tree after rename (rc=$rc)"; }
    cleanup_case5
    echo "ok: case5 root-owned clone is usable by uid 100 afterwards"
else
    echo "skip: case5 (docker not available)"
fi

# ── Case 6: clone not landed (no dirs, attachments referenced) ───────────
# The mount a JuiceFS restore rmdir()ed before cloning into it: empty, while
# the database references files.  Must fail so a fresh pod retries — never
# create an empty target that would make the refresh look complete.
setup_case
echo "aa/f1|10" > "$ATTACHMENTS_FILE"
if SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case6: succeeded although the referenced files are absent"
fi
[ ! -e "$FS/odoo_tgt" ] || fail "case6: empty target dir created for an unlanded clone"
teardown_case
echo "ok: case6 unlanded clone fails without creating the target"

# ── Case 7: incomplete tree (a referenced file missing) ──────────────────
setup_case
mkdir -p "$FS/odoo_src"
echo "content-a" > "$FS/odoo_src/aa"
reference_files_in "$FS/odoo_src"
echo "bb|10" >> "$ATTACHMENTS_FILE"          # referenced, never cloned
if SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case7: succeeded with a referenced file missing"
fi
[ -f "$FS/odoo_src/aa" ] || fail "case7: incomplete source mutated before verification"
[ ! -e "$FS/odoo_tgt" ]  || fail "case7: incomplete source renamed"
teardown_case
echo "ok: case7 missing file fails, source untouched"

# ── Case 8: truncated file (wrong size) ──────────────────────────────────
setup_case
mkdir -p "$FS/odoo_src"
echo "content-a" > "$FS/odoo_src/aa"
echo "aa|999" > "$ATTACHMENTS_FILE"          # half-written copy of a larger file
if SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null 2>&1; then
    fail "case8: succeeded with a wrong-sized file"
fi
[ -f "$FS/odoo_src/aa" ] || fail "case8: source mutated before verification"
teardown_case
echo "ok: case8 wrong-sized file fails"

# ── Case 9: retried Job (already renamed by an earlier attempt) ──────────
setup_case
mkdir -p "$FS/odoo_tgt"
echo "content-a" > "$FS/odoo_tgt/aa"
reference_files_in "$FS/odoo_tgt"
SRC_DB=odoo_src TGT_DB=odoo_tgt FILESTORE="$ROOT" bash "$SCRIPT" >/dev/null
[ "$(cat "$FS/odoo_tgt/aa")" = content-a ] || fail "case9: renamed tree disturbed on retry"
[ ! -e "$FS/odoo_src" ]                    || fail "case9: phantom source dir created"
teardown_case
echo "ok: case9 retry after rename verifies the target"

echo "PASS: all rename-filestore.sh cases"
