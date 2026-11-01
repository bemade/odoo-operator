# shellcheck shell=bash
# Shared helpers for the testing/minikube scripts.  Source it, don't run it.
#
# Everything here exists to make one guarantee: these scripts only ever talk
# to the throwaway minikube profile, never to whatever cluster the caller's
# default kubeconfig happens to point at (which may well be production).
#
#   ODOO_OP_PROFILE      minikube profile name               (default: odoo-op)
#   KUBECONFIG_ODOO_OP   kubeconfig dedicated to that profile
#                        (default: ~/.kube/minikube-odoo-op.yaml)
#   ODOO_OP_WORKDIR      scratch dir for chart / source exports
#                        (default: ${XDG_CACHE_HOME:-~/.cache}/odoo-operator-minikube)

MINIKUBE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$MINIKUBE_DIR" rev-parse --show-toplevel)"
PROFILE="${ODOO_OP_PROFILE:-odoo-op}"
WORKDIR="${ODOO_OP_WORKDIR:-${XDG_CACHE_HOME:-$HOME/.cache}/odoo-operator-minikube}"
NS=demo # the namespace manifests/demo-instances.yaml creates
export MINIKUBE_DIR REPO_ROOT PROFILE WORKDIR NS

# A dedicated kubeconfig, never the default one: `minikube start` writes its
# context into $KUBECONFIG *and makes it the current context*, so starting the
# profile against ~/.kube/config would silently retarget every later kubectl
# call the developer makes.
export KUBECONFIG="${KUBECONFIG_ODOO_OP:-$HOME/.kube/minikube-odoo-op.yaml}"

log() { echo "[$(basename "$0") $(date +%H:%M:%S)] $*" >&2; }
die() { echo "ERROR: $*" >&2; exit 1; }

# Print the calling script's header comment (its usage text).
usage_from_header() {
  awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
}

require_tools() {
  local t missing=()
  for t in "$@"; do command -v "$t" >/dev/null 2>&1 || missing+=("$t"); done
  ((${#missing[@]} == 0)) || die "missing required tools: ${missing[*]}"
}

# Refuse a kubeconfig that is (or includes) the default one.
assert_dedicated_kubeconfig() {
  [[ "$KUBECONFIG" != *:* ]] || die "KUBECONFIG_ODOO_OP must be a single file, got '$KUBECONFIG'"
  local default_cfg="$HOME/.kube/config" real
  real="$(realpath -m "$KUBECONFIG")"
  [[ "$real" != "$(realpath -m "$default_cfg")" ]] ||
    die "KUBECONFIG_ODOO_OP points at $default_cfg; use a file dedicated to the minikube profile"
}

# Pin every kubectl / helm call in the sourcing script to the profile's
# context, on top of the dedicated kubeconfig.
kubectl() { command kubectl --context "$PROFILE" "$@"; }
helm() { command helm --kube-context "$PROFILE" "$@"; }

# Refuse to continue unless the kubeconfig's current context is the minikube
# profile's AND its API server is that profile's node.  Call before touching
# the cluster.
assert_minikube() {
  assert_dedicated_kubeconfig
  local ctx server ip host_port
  ctx="$(command kubectl config current-context 2>/dev/null || true)"
  [[ "$ctx" == "$PROFILE" ]] ||
    die "REFUSING: current context in $KUBECONFIG is '$ctx', expected '$PROFILE'"
  server="$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
  ip="$(minikube -p "$PROFILE" ip 2>/dev/null)" ||
    die "REFUSING: minikube profile '$PROFILE' is not running (minikube -p $PROFILE ip failed)"
  [[ -n "$ip" ]] || die "REFUSING: minikube -p $PROFILE ip returned nothing"
  if [[ "$server" == "https://$ip:"* ]]; then
    return 0
  fi
  # Docker driver on macOS/Windows: the API server is published on a
  # localhost port of the profile's node container instead.
  if [[ "$server" =~ ^https://(127\.0\.0\.1|localhost):([0-9]+)$ ]] &&
    host_port="$(docker port "$PROFILE" 8443/tcp 2>/dev/null)" &&
    grep -qE ":${BASH_REMATCH[2]}\$" <<<"$host_port"; then
    return 0
  fi
  die "REFUSING: context '$ctx' points at '$server', not minikube profile '$PROFILE' ($ip)"
}

# Latest released version, from local tags (run `git fetch --tags` first).
latest_release_tag() {
  git -C "$REPO_ROOT" tag -l 'v*' --sort=-v:refname |
    grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1
}

# Normalise "2.8.5" / "v2.8.5" to the git / GHCR tag form "v2.8.5".
normalise_version() {
  local v="${1#v}"
  [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a release version: '$1' (want vX.Y.Z)"
  echo "v$v"
}

# Primary pod of the demo's CNPG cluster.
pg_primary_pod() {
  local pod
  pod="$(kubectl -n pg get pod -l cnpg.io/cluster=pg-main,cnpg.io/instanceRole=primary \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)"
  [[ -n "$pod" ]] || die "no primary pod found for CNPG cluster pg/pg-main"
  echo "$pod"
}

# Database an instance's Odoo is configured for (db_name in its odoo.conf).
instance_db() {
  kubectl -n "$NS" exec "deploy/$1" -- sh -c "sed -n 's/^db_name *= *//p' /etc/odoo/odoo.conf"
}
