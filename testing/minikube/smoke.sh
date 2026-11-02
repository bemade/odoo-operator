#!/usr/bin/env bash
# Smoke test of the staging-refresh path on the minikube test cluster.
# Exits non-zero if any check fails, after printing a pass/fail summary.
#
#   ./smoke.sh                      # the default sequence below
#   ./smoke.sh <action> [...]       # actions run in the order given
#
# Actions:
#   seed               create smoke attachments + a "real" SMTP server on demo/prod
#   check <instance>   filestore/attachment consistency, neutralization and mail
#                      servers (expectations follow the instance's environment)
#   refresh <method>   OdooStagingRefreshJob prod -> staging with filestoreMethod
#                      copy | snapshot | auto, then check staging
#   mail               send a mail from demo/staging, expect it in Mailpit
#   reap               delete finished pods in demo (see README, "Caveats")
#
# Default: seed  check prod  refresh copy  mail  reap  refresh snapshot  mail
#
#   SMOKE_REFRESH_TIMEOUT    seconds to wait for a refresh (default 1800)
#   SMOKE_STAGING_SMTP_HOST  expected staging SMTP host (default: read from
#                            the operator's --default-staging-smtp-host)
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

REFRESH_TIMEOUT="${SMOKE_REFRESH_TIMEOUT:-1800}"
RESULTS=()
FAILED=0
SOURCE_SMOKE_COUNT=""

record() { # record PASS|FAIL <name> [detail...]
  local status="$1" name="$2"
  shift 2
  RESULTS+=("$status $name${*:+  ($*)}")
  [[ "$status" == PASS ]] || FAILED=$((FAILED + 1))
  echo "  $status $name${*:+  ($*)}"
}

finish() {
  local rc=$? r passed=0
  trap - EXIT
  ((rc == 0)) || record FAIL aborted "exit status $rc before the sequence finished"
  echo
  echo "== smoke summary =="
  for r in "${RESULTS[@]}"; do
    echo "$r"
    [[ "$r" == PASS* ]] && passed=$((passed + 1))
  done
  echo "== $passed passed, $FAILED failed =="
  ((FAILED == 0)) || exit 1
  exit "$rc"
}

# oshell <instance> <script.py> [python prelude]: run a script in `odoo shell`.
oshell() {
  { [[ -n "${3:-}" ]] && printf '%s\n' "$3"; cat "$2"; } |
    kubectl -n "$NS" exec -i "deploy/$1" -- \
      odoo shell -c /etc/odoo/odoo.conf --no-http --log-level=warn 2>&1
}

wait_running() { # wait_running <instance>: phase Running and both rollouts done
  local i="$1" phase=""
  kubectl -n "$NS" get odooinstance "$i" >/dev/null 2>&1 || {
    echo "  no OdooInstance $NS/$i" >&2
    return 1
  }
  for _ in $(seq 1 120); do
    phase="$(kubectl -n "$NS" get odooinstance "$i" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    if [[ "$phase" == Running ]] &&
      kubectl -n "$NS" rollout status "deploy/$i" --timeout=300s >/dev/null &&
      kubectl -n "$NS" rollout status "deploy/$i-cron" --timeout=300s >/dev/null; then
      return 0
    fi
    sleep 5
  done
  echo "  demo/$i is '$phase', not Running, after 10 min" >&2
  return 1
}

staging_smtp_host() {
  if [[ -n "${SMOKE_STAGING_SMTP_HOST:-}" ]]; then
    echo "$SMOKE_STAGING_SMTP_HOST"
    return
  fi
  local h
  h="$(kubectl -n odoo-operator get deploy odoo-operator \
    -o jsonpath='{.spec.template.spec.containers[0].args}' |
    grep -oE -- '--default-staging-smtp-host=[A-Za-z0-9.-]+' | cut -d= -f2 || true)"
  echo "${h:-mailpit.mailpit.svc.cluster.local}"
}

act_seed() {
  echo "-- seed demo/prod"
  wait_running prod || { record FAIL seed "demo/prod not Running"; return; }
  local out
  out="$(oshell prod "$MINIKUBE_DIR/odoo-shell/seed_attachments.py")" || true
  if grep -q '^SEED_DONE attachments' <<<"$out"; then
    record PASS seed_attachments
  else
    record FAIL seed_attachments
    tail -20 <<<"$out"
  fi
  out="$(oshell prod "$MINIKUBE_DIR/odoo-shell/seed_mailserver.py")" || true
  if grep -q '^SEED_DONE mailserver' <<<"$out"; then
    record PASS seed_mailserver
  else
    record FAIL seed_mailserver
    tail -20 <<<"$out"
  fi
}

act_check() { # act_check <instance>
  local i="${1:?check needs an instance name}" environment role host out line st name detail
  echo "-- check demo/$i"
  wait_running "$i" || { record FAIL "$i:running"; return; }
  environment="$(kubectl -n "$NS" get odooinstance "$i" -o jsonpath='{.spec.environment}')"
  if [[ "$environment" == Production ]]; then role=source; else role=target; fi
  host="$(staging_smtp_host)"
  [[ "$host" =~ ^[A-Za-z0-9.-]+$ ]] || { record FAIL "$i:check" "bad SMTP host '$host'"; return; }
  out="$(oshell "$i" "$MINIKUBE_DIR/odoo-shell/check_instance.py" \
    "ROLE = '$role'"$'\n'"STAGING_SMTP_HOST = '$host'")" || true
  grep '^INFO ' <<<"$out" | sed 's/^INFO /    /' || true
  while read -r line; do
    read -r _ st name detail <<<"$line"
    record "$st" "$i:$name" "$detail"
  done < <(grep -E '^CHECK (PASS|FAIL) ' <<<"$out" || true)
  if ! grep -q '^CHECK_DONE' <<<"$out"; then
    record FAIL "$i:check_instance" "script did not finish"
    tail -20 <<<"$out"
  fi
  local n
  n="$(sed -n 's/^INFO smoke_attachments //p' <<<"$out")"
  if [[ "$role" == source ]]; then
    SOURCE_SMOKE_COUNT="$n"
  elif [[ -n "$SOURCE_SMOKE_COUNT" ]]; then
    if [[ "$n" == "$SOURCE_SMOKE_COUNT" ]]; then
      record PASS "$i:smoke_attachments_match_source" "$n"
    else
      record FAIL "$i:smoke_attachments_match_source" "target=$n source=$SOURCE_SMOKE_COUNT"
    fi
  fi
}

refresh_running() {
  kubectl -n "$NS" get odoostagingrefreshjob \
    -o jsonpath='{range .items[*]}{.metadata.name}={.status.phase}{"\n"}{end}' |
    grep -vE '=(Completed|Failed)$' | grep . || true
}

act_refresh() { # act_refresh <copy|snapshot|auto>
  local m="${1:?refresh needs a method: copy | snapshot | auto}" name phase="" busy
  [[ "$m" =~ ^(copy|snapshot|auto)$ ]] || die "unknown filestore method: $m"
  name="smoke-$m-$(date +%H%M%S)"
  echo "-- refresh $name (prod -> staging, filestoreMethod $m)"
  if ! wait_running prod || ! wait_running staging; then
    record FAIL "refresh-$m" "instances not Running"
    return
  fi
  busy="$(refresh_running)"
  [[ -z "$busy" ]] || { record FAIL "refresh-$m" "another refresh is in progress: $busy"; return; }

  kubectl create -f - <<YAML
apiVersion: bemade.org/v1alpha1
kind: OdooStagingRefreshJob
metadata: {name: $name, namespace: $NS}
spec:
  odooInstanceRef: {name: staging}
  source: {instanceName: prod}
  filestoreMethod: $m
YAML
  local deadline=$((SECONDS + REFRESH_TIMEOUT))
  while ((SECONDS < deadline)); do
    phase="$(kubectl -n "$NS" get odoostagingrefreshjob "$name" -o jsonpath='{.status.phase}')"
    [[ "$phase" == Completed || "$phase" == Failed ]] && break
    sleep 5
  done
  kubectl -n "$NS" get odoostagingrefreshjob "$name" -o jsonpath='{.status}{"\n"}' | sed 's/^/    /'
  if [[ "$phase" != Completed ]]; then
    record FAIL "refresh-$m" "phase '$phase' after up to ${REFRESH_TIMEOUT}s"
    if [[ "$phase" != Failed ]]; then
      echo "    A snapshot-path refresh can stall while finished Job pods still hold the" >&2
      echo "    target PVC (pvc-protection); see README 'Caveats' and './smoke.sh reap'." >&2
      kubectl -n "$NS" get pvc,pods 2>/dev/null | sed 's/^/    /' >&2 || true
    fi
    return
  fi
  record PASS "refresh-$m" "$name Completed"

  local snap
  snap="$(kubectl -n "$NS" get odoostagingrefreshjob "$name" -o jsonpath='{.status.sourceSnapshot}')"
  case "$m" in
    snapshot)
      if [[ -n "$snap" ]]; then
        record PASS "refresh-$m:snapshot_path" "$snap"
      else
        record FAIL "refresh-$m:snapshot_path" "no status.sourceSnapshot"
      fi ;;
    copy)
      if [[ -z "$snap" ]]; then
        record PASS "refresh-$m:copy_path"
      else
        record FAIL "refresh-$m:copy_path" "status.sourceSnapshot=$snap"
      fi ;;
    auto) echo "    auto chose: ${snap:+snapshot ($snap)}${snap:-copy}" ;;
  esac

  if ! wait_running staging; then
    record FAIL "refresh-$m:staging_running"
    return
  fi
  local db pg left
  db="$(instance_db staging)"
  pg="$(pg_primary_pod)"
  left="$(kubectl -n pg exec "$pg" -c postgres -- psql -Atc \
    "select string_agg(datname, ' ') from pg_database where datname like '${db}\_refresh\_%'")"
  if [[ -z "$left" ]]; then
    record PASS "refresh-$m:temp_db_dropped"
  else
    record FAIL "refresh-$m:temp_db_dropped" "left behind: $left"
  fi
  act_check staging
}

act_mail() {
  local token out
  token="smoke-$(date +%s)-$RANDOM"
  echo "-- mail from demo/staging ($token)"
  wait_running staging || { record FAIL mail "demo/staging not Running"; return; }
  out="$(oshell staging "$MINIKUBE_DIR/odoo-shell/send_test_mail.py" "SUBJECT = 'odoo-op smoke $token'")" || true
  if ! grep -q '^SENT ' <<<"$out"; then
    record FAIL mail_sent
    tail -20 <<<"$out"
    return
  fi
  record PASS mail_sent
  for _ in $(seq 1 15); do
    if kubectl -n mailpit exec deploy/mailpit -- \
      wget -qO- 'http://localhost:8025/api/v1/messages?limit=50' 2>/dev/null | grep -qF "$token"; then
      record PASS mail_in_mailpit
      return
    fi
    sleep 2
  done
  record FAIL mail_in_mailpit "subject token $token not found in Mailpit"
}

act_reap() {
  echo "-- reap finished pods in $NS"
  local busy
  busy="$(refresh_running)"
  [[ -z "$busy" ]] || { record FAIL reap "refusing: a refresh is in progress: $busy"; return; }
  kubectl -n "$NS" delete pod --field-selector=status.phase==Succeeded --ignore-not-found
  kubectl -n "$NS" delete pod --field-selector=status.phase==Failed --ignore-not-found
}

case "${1:-}" in -h | --help) usage_from_header; exit 0 ;; esac
(($#)) || set -- seed check prod refresh copy mail reap refresh snapshot mail

# Parse the whole sequence up front, so a typo fails before anything runs.
actions=()
while (($#)); do
  case "$1" in
    seed | mail | reap) actions+=("act_$1"); shift ;;
    check | refresh)
      (($# >= 2)) || die "$1 needs an argument (see --help)"
      [[ "$1" != refresh || "$2" =~ ^(copy|snapshot|auto)$ ]] ||
        die "unknown filestore method: $2 (copy | snapshot | auto)"
      actions+=("act_$1 $2"); shift 2 ;;
    *) die "unknown action: $1 (see --help)" ;;
  esac
done

require_tools kubectl minikube
assert_minikube
trap finish EXIT
for a in "${actions[@]}"; do
  read -r fn arg <<<"$a"
  "$fn" ${arg:+"$arg"}
done
