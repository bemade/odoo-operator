#!/usr/bin/env bash
# Print a state snapshot of the demo namespace for before/after comparison
# around an upgrade.  No secret values are printed: only key names, owners,
# counts and a truncated hash of the admin_passwd line.
#
#   ./capture.sh
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl jq minikube sha256sum
assert_minikube
echo "## pods"
kubectl -n "$NS" get pods -o json | jq -r '.items[] | select(.status.phase == "Running") | "\(.metadata.name) uid=\(.metadata.uid) start=\(.status.startTime) ready=\([.status.containerStatuses[]?.ready] | all) conf-hash=\(.metadata.annotations["bemade.org/odoo-conf-hash"] // "-")"'
echo "## deployment generations"
kubectl -n "$NS" get deploy -o json | jq -r '.items[] | "\(.metadata.name) generation=\(.metadata.generation) strategy=\(.spec.strategy.type)"'
echo "## odoo-conf objects (keys only)"
for kind in configmap secret; do
  kubectl -n "$NS" get "$kind" -o json | jq -r --arg k "$kind" '.items[] | select(.metadata.name | test("odoo-conf")) | "\($k)/\(.metadata.name) keys=\((.data // {}) | keys | join(",")) owner=\(.metadata.ownerReferences[0].kind // "-")/\(.metadata.ownerReferences[0].name // "-")"'
done
echo "## admin_passwd line sha256 (whichever conf object exists)"
for i in $(kubectl -n "$NS" get odooinstance -o jsonpath='{.items[*].metadata.name}'); do
  src=secret
  conf="$(kubectl -n "$NS" get secret "$i-odoo-conf" -o jsonpath='{.data.odoo\.conf}' 2>/dev/null | base64 -d 2>/dev/null || true)"
  if [[ -z "$conf" ]]; then
    src=configmap
    conf="$(kubectl -n "$NS" get configmap "$i-odoo-conf" -o jsonpath='{.data.odoo\.conf}' 2>/dev/null || true)"
  fi
  echo "$i src=$src admin_passwd_sha256=$(grep '^admin_passwd' <<<"$conf" | sha256sum | cut -c1-16) db_name=$(sed -n 's/^db_name *= *//p' <<<"$conf")"
done
echo "## configmaps holding credentials (occurrence counts)"
for cm in $(kubectl -n "$NS" get configmap -o jsonpath='{.items[*].metadata.name}'); do
  d="$(kubectl -n "$NS" get configmap "$cm" -o json | jq -r '.data // {} | tostring')"
  echo "$cm db_password=$(grep -o db_password <<<"$d" | wc -l) admin_passwd=$(grep -o admin_passwd <<<"$d" | wc -l)"
done
echo "## databases"
kubectl -n pg exec "$(pg_primary_pod)" -c postgres -- psql -X -Atc "select datname from pg_database where datname like 'odoo%' order by 1"
