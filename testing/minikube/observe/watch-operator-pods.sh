#!/usr/bin/env bash
# Log, on change (polled every 1s), the operator pods with phase, readiness,
# image tag and deletionTimestamp.  Run alongside an upgrade to see whether
# old and new operator pods overlap.
#
#   ./watch-operator-pods.sh [seconds]   (default 900)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl jq minikube
assert_minikube
prev=""
end=$((SECONDS + ${1:-900}))
while ((SECONDS < end)); do
  cur="$(kubectl -n odoo-operator get pods -l app=odoo-operator -o json 2>/dev/null | jq -r '[.items[] | "\(.metadata.name) phase=\(.status.phase) ready=\([.status.containerStatuses[]?.ready] | all) image=\(.spec.containers[0].image | split(":") | last) del=\(.metadata.deletionTimestamp // "-")"] | join(" | ")' || true)"
  [[ "$cur" != "$prev" ]] && echo "$(date -u +%T.%3N) n=$(grep -o 'phase=' <<<"$cur" | wc -l) $cur"
  prev="$cur"
  sleep 1
done
