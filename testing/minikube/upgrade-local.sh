#!/usr/bin/env bash
# Upgrade the minikube test cluster's operator to a LOCALLY BUILT image of a
# git revision, using that revision's own chart (and therefore its CRDs).
#
#   ./upgrade-local.sh [<rev>] [--build-only | --deploy-only]
#
#   <rev>          any commit-ish in this clone (default HEAD)
#   --build-only   export + docker build + minikube image load, no helm
#   --deploy-only  helm upgrade only (image must already be loaded)
#
# The revision is exported with `git archive` into $ODOO_OP_WORKDIR and built
# from there, so:
#   - exactly the committed tree is built: uncommitted edits are NOT included
#     (commit first, even as a throwaway WIP commit);
#   - the build context is the source alone, not the clone's multi-GB target/
#     (the repo has no .dockerignore), and a cargo build running in the clone
#     is neither read nor disturbed.
#
# The image is tagged localhost/odoo-operator:local-<sha>, loaded straight
# into the minikube node (no registry), and deployed with imagePullPolicy
# Never.  Roll back to a release with: ./setup.sh --version vX.Y.Z operator
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$(dirname "$0")/lib.sh"

REV=HEAD
MODE=all
while (($#)); do
  case "$1" in
    --build-only | --deploy-only) MODE="$1"; shift ;;
    -h | --help) usage_from_header; exit 0 ;;
    -*) die "unknown option: $1" ;;
    *) REV="$1"; shift ;;
  esac
done
require_tools git docker minikube kubectl helm tar

SHA="$(git -C "$REPO_ROOT" rev-parse --short --verify "$REV^{commit}")" ||
  die "not a commit in $REPO_ROOT: $REV"
TAG="local-$SHA"
IMG="localhost/odoo-operator:$TAG"
EXPORT="$WORKDIR/src-$SHA"

if [[ "$REV" == HEAD ]] && [[ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ]]; then
  log "WARNING: the working tree has uncommitted changes; building committed $SHA only"
fi

export_source() {
  rm -rf "$EXPORT" && mkdir -p "$EXPORT"
  git -C "$REPO_ROOT" archive "$SHA" | tar -x -C "$EXPORT"
}

if [[ "$MODE" != --deploy-only ]]; then
  export_source
  log "building $IMG from $EXPORT (multi-stage Rust build; slow when cold)"
  docker build -t "$IMG" "$EXPORT"
  assert_minikube
  minikube -p "$PROFILE" image load --overwrite=true "$IMG"
  minikube -p "$PROFILE" image ls | grep -F "odoo-operator:$TAG" >/dev/null ||
    die "$IMG not visible in minikube after image load"
  log "loaded $IMG into minikube profile $PROFILE"
fi
[[ "$MODE" == --build-only ]] && exit 0

assert_minikube
[[ -f "$EXPORT/charts/odoo-operator/Chart.yaml" ]] || export_source
minikube -p "$PROFILE" image ls | grep -F "odoo-operator:$TAG" >/dev/null ||
  die "$IMG is not loaded in minikube; run without --deploy-only first"

# Same values as the released baseline; only the chart and the image change.
log "helm upgrade odoo-operator -> $IMG"
helm upgrade --install odoo-operator "$EXPORT/charts/odoo-operator" \
  -n odoo-operator --create-namespace -f "$MINIKUBE_DIR/values.yaml" \
  --set operator.image="$IMG" --set operator.imagePullPolicy=Never \
  --wait --timeout 10m
kubectl -n odoo-operator rollout status deploy/odoo-operator --timeout=300s
kubectl -n odoo-operator get deploy odoo-operator \
  -o jsonpath='{.spec.template.spec.containers[0].image} {.spec.template.spec.containers[0].imagePullPolicy}{"\n"}'
log "done; re-run the smoke on the new operator: ./smoke.sh"
