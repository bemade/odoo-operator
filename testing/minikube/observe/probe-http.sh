#!/usr/bin/env bash
# Probe /web/login on demo/prod and demo/staging about every second from
# inside the cluster (wget in the Mailpit pod) and log each UP/DOWN change:
# measures instance downtime during an operator upgrade.
#
#   ./probe-http.sh [seconds]   (default 900)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl minikube
assert_minikube
declare -A last
end=$((SECONDS + ${1:-900}))
while ((SECONDS < end)); do
  # shellcheck disable=SC2016  # $i expands in the pod's shell
  out="$(kubectl -n mailpit exec deploy/mailpit -- sh -c 'for i in prod staging; do
      if wget -q -T 2 -O /dev/null "http://$i.demo.svc:8069/web/login"; then echo "$i UP"; else echo "$i DOWN"; fi
    done' 2>/dev/null || true)"
  while read -r inst st; do
    [[ -z "$inst" ]] && continue
    [[ "${last[$inst]:-}" != "$st" ]] && echo "$(date -u +%T) $inst $st"
    last[$inst]="$st"
  done <<<"$out"
  sleep 1
done
