#!/usr/bin/env bash
# Log, on change (polled every 2s), the odoo* databases on pg/pg-main: shows
# a refresh's temporary DB appearing and being renamed or dropped.
#
#   ./watch-dbs.sh [seconds]   (default 600)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl minikube
assert_minikube
pg="$(pg_primary_pod)"
prev=""
end=$((SECONDS + ${1:-600}))
while ((SECONDS < end)); do
  cur="$(kubectl -n pg exec "$pg" -c postgres -- psql -X -Atc \
    "select string_agg(datname, ' ' order by datname) from pg_database where datname like 'odoo%'" 2>/dev/null || true)"
  [[ -n "$cur" && "$cur" != "$prev" ]] && echo "$(date -u +%T) $cur"
  [[ -n "$cur" ]] && prev="$cur"
  sleep 2
done
