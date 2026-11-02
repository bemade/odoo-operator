#!/usr/bin/env bash
# Count connection-exhaustion and role-reset lines in the operator and
# Postgres logs over a recent window: two reconcilers fighting over the same
# instances show up here first.
#
#   ./grep-pg-errors.sh [since]   (kubectl --since syntax, default 10m)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl minikube
assert_minikube
S="${1:-10m}"
pg="$(pg_primary_pod)"
op="$(kubectl -n odoo-operator logs -l app=odoo-operator --since="$S" --tail=-1 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' || true)"
pglog="$(kubectl -n pg logs "$pg" -c postgres --since="$S" 2>/dev/null || true)"
count() { grep -c -- "$1" <<<"$2" || true; }
echo "operator: too_many_clients=$(count 'too many clients' "$op") reserved=$(count 'reserved for roles' "$op") reset_role_password=$(count 'reset postgres role password' "$op") tuple_concurrently=$(count 'tuple concurrently updated' "$op")"
echo "postgres: too_many_clients=$(count 'too many clients' "$pglog") reserved=$(count 'reserved for roles' "$pglog") auth_failed=$(count 'password authentication failed' "$pglog")"
