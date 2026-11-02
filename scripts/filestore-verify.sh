# shellcheck shell=bash
# Filestore verification shared by rename-filestore.sh (snapshot path) and
# clone-filestore.sh (copy path).  A library, not a Job script: it only
# defines functions.  In a Job pod the operator prepends this file to the
# script's text, so both run as one `bash -c` command; run from a checkout
# (scripts/tests/), each script sources it from its own directory.
#
# The database is the authority on what a filestore must contain: every
# ir_attachment row with a store_fname must resolve to a file of the recorded
# file_size.  store_fname is content-addressed ("<2-char prefix>/<sha1>"), so
# this is an exact statement of what Odoo will try to read, not a proxy for
# it.  Size is checked too: a half-written file exists but is short.
#
# ── Point in time ───────────────────────────────────────────────────────────
#
# The filestore and the database are captured at different moments.  On the
# snapshot path the VolumeSnapshot is requested before the DB dump's MVCC
# snapshot is taken, and on the copy path the copy walks the live tree while
# the dump runs.  On a busy instance, attachments created in that gap are in
# the database but legitimately absent from the filestore copy — and they are
# absent identically on every retry, so a strict check can never pass.
#
# FILESTORE_PIT is the moment the filestore copy began (RFC 3339, UTC).  A
# file is REQUIRED only if every row referencing it was last written at or
# before FILESTORE_PIT minus FILESTORE_PIT_MARGIN_SECONDS (default 600).
# Anything newer is reported as an informational count and never fails.
#
# Why the last write and not only create_date: rewriting an attachment's
# content gives the row a new store_fname but keeps its create_date, so an
# old row can point at a brand-new file.  GREATEST(create_date, write_date)
# is when the row last took the store_fname it has now.
#
# Why a margin: Odoo stamps create_date/write_date with the *transaction
# start* time but writes the file when the ORM call runs inside that
# transaction, so a request that began before the snapshot can write its
# file after it.  HTTP workers are killed at limit_time_real (120 s by
# default) and cron jobs default to the same limit; 600 s is five times
# that, and also absorbs clock skew between the PostgreSQL server (which
# stamps the rows) and the Kubernetes control plane (which stamps the PIT).
# The margin only ever loosens the check for attachments younger than ten
# minutes before the copy: an empty or unpopulated mount still fails on the
# first older one, which any database with history has.
#
# Odoo stores create_date/write_date as naive `timestamp` values in UTC.
# EXTRACT(EPOCH FROM <timestamp without time zone>) reads them "without regard
# to timezone", i.e. as UTC, whatever the session TimeZone — so the epoch the
# query returns is the true one and is compared to the PIT in Python, in UTC.
#
# With FILESTORE_PIT unset every referenced file is required (strict mode).
#
# Required env vars:
#   HOST, PORT, USER, PASSWORD — PostgreSQL connection
#   DB_NAME                    — database whose ir_attachment is the authority
# Optional:
#   FILESTORE_PIT                — RFC 3339 UTC time the filestore copy began
#   FILESTORE_PIT_MARGIN_SECONDS — safety margin before FILESTORE_PIT (600)

# One row per referenced file: store_fname | expected size | epoch (UTC) of
# the OLDEST row referencing it — Odoo deduplicates by checksum, so several
# rows can share a file, and the file must exist if any of them is old.  A
# row without timestamps counts as old (epoch 0): required.
filestore_attachment_listing() {
    PGPASSWORD="$PASSWORD" psql -h "$HOST" -p "$PORT" -U "$USER" -d "$DB_NAME" \
        -v ON_ERROR_STOP=1 -At -F '|' -c "
        SELECT store_fname,
               COALESCE(MAX(file_size), -1),
               EXTRACT(EPOCH FROM MIN(COALESCE(GREATEST(create_date, write_date),
                                               'epoch'::timestamp)))::bigint
        FROM ir_attachment
        WHERE store_fname IS NOT NULL
        GROUP BY store_fname"
}

# verify_filestore <root>
# Returns 0 when every REQUIRED file is present under <root> with its recorded
# size; 1 otherwise, or when the database cannot be read.  <root> need not
# exist: an absent tree is complete only when nothing is required.
verify_filestore() {
    local root="$1" listing rc=0
    listing="$(mktemp)"
    if ! filestore_attachment_listing > "$listing"; then
        echo "ERROR: could not list ir_attachment in database $DB_NAME" >&2
        rm -f "$listing"
        return 1
    fi
    python3 -u -c '
import os, sys
from datetime import datetime, timezone

root, listing = sys.argv[1], sys.argv[2]

cutoff = None
pit = os.environ.get("FILESTORE_PIT", "").strip()
if pit:
    try:
        dt = datetime.fromisoformat(pit.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        margin = int(os.environ.get("FILESTORE_PIT_MARGIN_SECONDS") or 600)
        cutoff = dt.timestamp() - margin
        print(f"point in time {pit}, margin {margin}s: files last written after "
              f"{datetime.fromtimestamp(cutoff, timezone.utc).isoformat()} are not required")
    except ValueError as e:
        # Strict is the safe direction: it can only reject a tree, never
        # accept an incomplete one.
        sys.stderr.write(f"WARNING: ignoring unparseable FILESTORE_PIT={pit!r} ({e}); "
                         "verifying strictly\n")
        cutoff = None
else:
    print("no point in time given: every referenced file is required")

required = missing = short = late = late_absent = 0
examples = []
with open(listing) as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        name, size, epoch = line.rsplit("|", 2)
        try:
            expected = int(size)
        except ValueError:
            expected = -1
        try:
            written = float(epoch)
        except ValueError:
            written = 0.0
        is_required = cutoff is None or written <= cutoff
        path = os.path.join(root, name)
        try:
            actual = os.stat(path).st_size
            problem = None
            if expected >= 0 and actual != expected:
                problem = f"{name} is {actual}B, expected {expected}B"
        except OSError:
            problem = "missing " + name
            actual = None
        if not is_required:
            late += 1
            if problem:
                late_absent += 1
            continue
        required += 1
        if problem:
            if actual is None:
                missing += 1
            else:
                short += 1
            if len(examples) < 5:
                examples.append(problem)

print(f"verified {required - missing - short}/{required} required attachment files under {root}")
if late:
    print(f"{late} attachment file(s) newer than the point in time, "
          f"{late_absent} of them absent or wrong-sized: expected, not a failure")
if missing or short:
    sys.stderr.write(f"FILESTORE INCOMPLETE: {missing} missing, {short} wrong-sized "
                     f"(of {required} required).\n")
    for e in examples:
        sys.stderr.write("  - " + e + "\n")
    sys.exit(1)
' "$root" "$listing" || rc=$?
    rm -f "$listing"
    return "$rc"
}
