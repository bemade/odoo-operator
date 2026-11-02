#!/usr/bin/env bash
# Make the next staging refresh fail in its neutralize step, deterministically,
# to exercise the operator's neutralize-failure path.
#
#   ./inject-neutralize-failure.sh apply    # install the trigger on demo/prod's DB
#   ./inject-neutralize-failure.sh undo     # remove it
#   ./inject-neutralize-failure.sh status   # is it installed?
#
# Targets the demo's SOURCE database only: the DB demo/prod's own odoo.conf
# names, and only if demo/prod is spec.environment Production and that DB is
# neither demo/staging's DB nor a refresh temp DB.  Needs `./smoke.sh seed`
# first (the trigger keeps the seeded smtp.example.com server active).
# See README.md, "Exercising the neutralize-failure path".
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

ACTION="${1:-}"
case "$ACTION" in
  apply) SQL="$MINIKUBE_DIR/sql/neutralize-failure-inject.sql" ;;
  undo) SQL="$MINIKUBE_DIR/sql/neutralize-failure-undo.sql" ;;
  status) SQL="" ;;
  *) usage_from_header; exit 2 ;;
esac
require_tools kubectl minikube
assert_minikube

environment="$(kubectl -n "$NS" get odooinstance prod -o jsonpath='{.spec.environment}')"
[[ "$environment" == Production ]] ||
  die "demo/prod has environment '$environment', expected Production; refusing"
db="$(instance_db prod)"
[[ "$db" =~ ^[A-Za-z0-9_]+$ ]] || die "could not resolve demo/prod's database (got '$db')"
[[ "$db" != *_refresh_* ]] || die "$db looks like a refresh temp DB; refusing"
staging_db="$(instance_db staging 2>/dev/null || true)"
[[ "$db" != "$staging_db" ]] || die "demo/prod and demo/staging resolve to the same DB $db; refusing"
pg="$(pg_primary_pod)"

if [[ "$ACTION" == status ]]; then
  n="$(kubectl -n pg exec "$pg" -c postgres -- psql -X -At -d "$db" -c \
    "select count(*) from pg_trigger where tgname = 'smoke_keep_mail_active'")"
  echo "demo/prod DB $db: trigger smoke_keep_mail_active $([[ "$n" == 0 ]] && echo absent || echo PRESENT)"
  exit 0
fi

log "$ACTION on demo/prod DB $db (pod pg/$pg)"
kubectl -n pg exec -i "$pg" -c postgres -- \
  psql -X -1 -v ON_ERROR_STOP=1 -v target_db="$db" -d "$db" <"$SQL"
