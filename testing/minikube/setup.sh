#!/usr/bin/env bash
# Build the local minikube test cluster for odoo-operator.  See README.md.
#
#   ./setup.sh [--version vX.Y.Z] all          # every step except smoke, in order
#   ./setup.sh [--version vX.Y.Z] <step> [...] # one or more steps
#
# Steps:
#   cluster      start the minikube profile, enable volumesnapshots + csi-hostpath-driver
#   snapclass    mark the hostpath VolumeSnapshotClass as the default
#   certmanager  install cert-manager (the chart's webhook TLS needs it)
#   cnpg         install the CloudNativePG operator
#   pgcluster    create the pg/pg-main cluster + the operator's postgres-clusters Secret
#   mailpit      deploy Mailpit, the SMTP sink for staging instances
#   operator     install a RELEASED odoo-operator (chart from `git archive <tag>`,
#                image ghcr.io/bemade/odoo-operator:<tag>); also the rollback path
#   demo         create demo/prod (Production) and demo/staging (Staging)
#   smoke        run ./smoke.sh (default sequence)
#
# --version (or ODOO_OPERATOR_VERSION) picks the release for the operator
# step; default is the newest vX.Y.Z tag in the local clone.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

K8S_VERSION="${MINIKUBE_K8S_VERSION:-v1.32.8}"
MINIKUBE_DRIVER="${MINIKUBE_DRIVER:-docker}"
MINIKUBE_CPUS="${MINIKUBE_CPUS:-8}"
MINIKUBE_MEMORY="${MINIKUBE_MEMORY:-16g}"
MINIKUBE_DISK="${MINIKUBE_DISK:-60g}"
CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-v1.21.1}"
CNPG_CHART_VERSION="${CNPG_CHART_VERSION:-0.29.0}"
OPERATOR_IMAGE_REPO="${OPERATOR_IMAGE_REPO:-ghcr.io/bemade/odoo-operator}"
VERSION="${ODOO_OPERATOR_VERSION:-}"

step_cluster() {
  assert_dedicated_kubeconfig
  mkdir -p "$(dirname "$KUBECONFIG")"
  log "minikube start -p $PROFILE (kubeconfig $KUBECONFIG)"
  minikube start -p "$PROFILE" --driver="$MINIKUBE_DRIVER" --cpus="$MINIKUBE_CPUS" \
    --memory="$MINIKUBE_MEMORY" --disk-size="$MINIKUBE_DISK" \
    --kubernetes-version="$K8S_VERSION"
  assert_minikube
  minikube -p "$PROFILE" addons enable volumesnapshots
  minikube -p "$PROFILE" addons enable csi-hostpath-driver
  kubectl -n kube-system rollout status daemonset/csi-hostpathplugin --timeout=300s
}

step_snapclass() {
  assert_minikube
  # A snapshot-path refresh creates its VolumeSnapshot without a
  # volumeSnapshotClassName, so the class must be the cluster default.
  kubectl annotate volumesnapshotclass csi-hostpath-snapclass \
    snapshot.storage.kubernetes.io/is-default-class=true --overwrite
  kubectl get volumesnapshotclass
  kubectl get storageclass
}

step_certmanager() {
  assert_minikube
  helm upgrade --install cert-manager cert-manager \
    --repo https://charts.jetstack.io --version "$CERT_MANAGER_VERSION" \
    -n cert-manager --create-namespace --set crds.enabled=true --wait --timeout 10m
}

step_cnpg() {
  assert_minikube
  helm upgrade --install cnpg cloudnative-pg \
    --repo https://cloudnative-pg.github.io/charts --version "$CNPG_CHART_VERSION" \
    -n cnpg-system --create-namespace --wait --timeout 10m
}

step_pgcluster() {
  assert_minikube
  kubectl apply -f "$MINIKUBE_DIR/manifests/pg-cluster.yaml"
  log "waiting for CNPG cluster pg/pg-main"
  # The Cluster object can take a moment to get a status the wait understands.
  for _ in $(seq 1 30); do
    kubectl -n pg get cluster pg-main >/dev/null 2>&1 && break
    sleep 2
  done
  kubectl -n pg wait cluster/pg-main --for=condition=Ready --timeout=600s
  # The operator's postgres-clusters Secret, built from the CNPG superuser
  # secret.  The password goes straight from one Secret into the other.
  local pw
  pw="$(kubectl -n pg get secret pg-main-superuser -o jsonpath='{.data.password}' | base64 -d)"
  kubectl create namespace odoo-operator --dry-run=client -o yaml | kubectl apply -f -
  kubectl -n odoo-operator create secret generic postgres-clusters \
    --from-file=clusters.yaml=/dev/stdin --dry-run=client -o yaml <<EOF | kubectl apply -f -
main:
  host: pg-main-rw.pg.svc.cluster.local
  port: 5432
  adminUser: postgres
  adminPassword: "$pw"
  default: true
EOF
}

step_mailpit() {
  assert_minikube
  kubectl apply -f "$MINIKUBE_DIR/manifests/mailpit.yaml"
  kubectl -n mailpit rollout status deploy/mailpit --timeout=300s
}

step_operator() {
  assert_minikube
  local tag chart_root image
  if [[ -n "$VERSION" ]]; then
    tag="$(normalise_version "$VERSION")"
  else
    tag="$(latest_release_tag)"
    [[ -n "$tag" ]] || die "no vX.Y.Z tag in $REPO_ROOT; run 'git fetch --tags' or pass --version"
  fi
  git -C "$REPO_ROOT" rev-parse -q --verify "refs/tags/$tag" >/dev/null ||
    die "tag $tag not found locally; run 'git fetch --tags'"
  # The chart's default image has no tag, and GHCR tags carry the "v".
  image="$OPERATOR_IMAGE_REPO:$tag"
  docker manifest inspect "$image" >/dev/null 2>&1 ||
    log "WARNING: could not confirm $image exists in the registry; continuing"

  chart_root="$WORKDIR/chart-$tag"
  rm -rf "$chart_root" && mkdir -p "$chart_root"
  git -C "$REPO_ROOT" archive "$tag" charts/odoo-operator | tar -x -C "$chart_root"

  log "installing released odoo-operator $tag"
  helm upgrade --install odoo-operator "$chart_root/charts/odoo-operator" \
    -n odoo-operator --create-namespace -f "$MINIKUBE_DIR/values.yaml" \
    --set operator.image="$image" --set operator.imagePullPolicy=IfNotPresent \
    --wait --timeout 10m
  kubectl -n odoo-operator rollout status deploy/odoo-operator --timeout=300s
  kubectl -n odoo-operator get deploy odoo-operator \
    -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
}

step_demo() {
  assert_minikube
  kubectl apply -f "$MINIKUBE_DIR/manifests/demo-instances.yaml"
  local i phase
  for i in prod staging; do
    log "waiting for demo/$i to be Running"
    for _ in $(seq 1 180); do
      phase="$(kubectl -n "$NS" get odooinstance "$i" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
      [[ "$phase" == Running ]] && break
      sleep 5
    done
    [[ "$phase" == Running ]] || die "demo/$i is '$phase' after 15 min"
    log "demo/$i Running"
  done
}

step_smoke() {
  assert_minikube
  "$MINIKUBE_DIR/smoke.sh"
}

ALL=(cluster snapclass certmanager cnpg pgcluster mailpit operator demo)
usage() { echo "usage: $0 [--version vX.Y.Z] all | ${ALL[*]} smoke" >&2; exit 2; }

steps=()
while (($#)); do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    -h | --help) usage_from_header; exit 0 ;;
    all) steps+=("${ALL[@]}"); shift ;;
    *)
      declare -F "step_$1" >/dev/null || { echo "unknown step: $1" >&2; usage; }
      steps+=("$1"); shift ;;
  esac
done
((${#steps[@]})) || usage
require_tools minikube kubectl helm docker git base64
for s in "${steps[@]}"; do "step_$s"; done
