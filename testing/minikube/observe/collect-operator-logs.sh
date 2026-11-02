#!/usr/bin/env bash
# Every 2s, save each operator pod's full log to <dir>/<pod>.log, so the logs
# of pods that go away during an upgrade are kept.
#
#   ./collect-operator-logs.sh <seconds> <dir>
set -euo pipefail
# shellcheck source-path=SCRIPTDIR/.. source=lib.sh
source "$(dirname "$0")/../lib.sh"
require_tools kubectl minikube
DURATION="${1:?usage: $0 <seconds> <dir>}"
DIR="${2:?usage: $0 <seconds> <dir>}"
assert_minikube
mkdir -p "$DIR"
end=$((SECONDS + DURATION))
while ((SECONDS < end)); do
  for p in $(kubectl -n odoo-operator get pods -l app=odoo-operator -o jsonpath='{.items[*].metadata.name}' 2>/dev/null || true); do
    if kubectl -n odoo-operator logs "$p" --tail=-1 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' >"$DIR/$p.log.tmp" &&
      [[ -s "$DIR/$p.log.tmp" ]]; then
      mv "$DIR/$p.log.tmp" "$DIR/$p.log"
    fi
  done
  sleep 2
done
rm -f "$DIR"/*.log.tmp
