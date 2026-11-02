#!/usr/bin/env bash
# Log, on change (polled every 2s), which *-odoo-conf ConfigMaps and Secrets
# exist in the demo namespace and its non-completed pods: shows instance
# rollouts and config-object migrations during an operator upgrade.
#
#   ./watch-demo.sh [seconds]   (default 600)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl minikube
assert_minikube
prev=""
end=$((SECONDS + ${1:-600}))
while ((SECONDS < end)); do
  cm="$(kubectl -n "$NS" get cm -o name 2>/dev/null | grep odoo-conf | tr '\n' ' ' || true)"
  secret="$(kubectl -n "$NS" get secret -o name 2>/dev/null | grep odoo-conf | tr '\n' ' ' || true)"
  pods="$(kubectl -n "$NS" get pods --no-headers 2>/dev/null | awk '$3 != "Completed" {print $1":"$2":"$3}' | tr '\n' ' ' || true)"
  cur="cm=[$cm] secret=[$secret] pods=[$pods]"
  [[ "$cur" != "$prev" ]] && echo "$(date -u +%T) $cur"
  prev="$cur"
  sleep 2
done
