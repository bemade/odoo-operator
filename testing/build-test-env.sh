#!/usr/bin/env bash
set -euo pipefail

#########################
# Configuration
#########################

MINIKUBE_DRIVER="${MINIKUBE_DRIVER:-docker}"   # Change to kvm2 if you prefer

TRAEFIK_NAMESPACE="${TRAEFIK_NAMESPACE:-traefik}"
TRAEFIK_RELEASE="${TRAEFIK_RELEASE:-traefik}"

CERT_MANAGER_NAMESPACE="${CERT_MANAGER_NAMESPACE:-cert-manager}"
CERT_MANAGER_RELEASE="${CERT_MANAGER_RELEASE:-cert-manager}"

POSTGRES_OPERATOR_NAMESPACE="${POSTGRES_OPERATOR_NAMESPACE:-postgres-operator}"
POSTGRES_OPERATOR_RELEASE="${POSTGRES_OPERATOR_RELEASE:-postgres-operator}"

POSTGRES_YAML="${POSTGRES_YAML:-postgres.yaml}"

SELF_SIGNED_CLUSTERISSUER_NAME="${SELF_SIGNED_CLUSTERISSUER_NAME:-selfsigned-cluster-issuer}"

#########################
# Helpers
#########################

log() {
  echo "[setup-k8s-dev] $*"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1
}

#########################
# Package prerequisites
#########################

install_base_packages() {
  log "Installing base packages via apt..."
  sudo apt-get update -y
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    curl \
    ca-certificates \
    apt-transport-https \
    gnupg \
    lsb-release \
    software-properties-common
}

#########################
# Install kubectl
#########################

install_kubectl() {
  if need_cmd kubectl; then
    log "kubectl already installed: $(command -v kubectl)"
    return
  fi

  log "Installing kubectl..."
  KUBECTL_VERSION="$(curl -L -s https://dl.k8s.io/release/stable.txt)"
  curl -L "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" -o /tmp/kubectl
  sudo install -o root -g root -m 0755 /tmp/kubectl /usr/local/bin/kubectl
  rm /tmp/kubectl
  log "kubectl installed: $(kubectl version --client --output=yaml | head -n 5 || true)"
}

#########################
# Install helm
#########################

install_helm() {
  if need_cmd helm; then
    log "helm already installed: $(command -v helm)"
    return
  fi

  log "Installing helm..."
  curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
  log "helm installed: $(helm version --short || true)"
}

#########################
# Install minikube
#########################

install_minikube() {
  if need_cmd minikube; then
    log "minikube already installed: $(command -v minikube)"
    return
  fi

  log "Installing minikube..."
  curl -L https://storage.googleapis.com/minikube/releases/latest/minikube-linux-amd64 -o /tmp/minikube
  sudo install -o root -g root -m 0755 /tmp/minikube /usr/local/bin/minikube
  rm /tmp/minikube
  log "minikube installed: $(minikube version || true)"
}

#########################
# Start / ensure Minikube (default profile)
#########################

ensure_minikube_running() {
  log "Ensuring default minikube profile is running..."

  if minikube status >/dev/null 2>&1; then
    log "Default minikube profile already running."
  else
    log "Starting default minikube profile with driver='${MINIKUBE_DRIVER}'..."
    minikube start --driver="${MINIKUBE_DRIVER}"
  fi

  log "Setting kubectl context to 'minikube'..."
  kubectl config use-context "minikube"
}

#########################
# Install Traefik via Helm
#########################

install_traefik() {
  log "Installing Traefik in namespace '${TRAEFIK_NAMESPACE}'..."

  if ! helm repo list | grep -q '^traefik'; then
    helm repo add traefik https://traefik.github.io/charts
  fi
  helm repo update

  kubectl get namespace "${TRAEFIK_NAMESPACE}" >/dev/null 2>&1 || \
    kubectl create namespace "${TRAEFIK_NAMESPACE}"

  helm upgrade --install "${TRAEFIK_RELEASE}" traefik/traefik \
    --namespace "${TRAEFIK_NAMESPACE}" \
    --set deployment.kind=Deployment \
    --set service.type=LoadBalancer \
    --set providers.kubernetesCRD.allowCrossNamespace=true
    --set service.spec.loadBalancerIp=

  log "Traefik installed."
}

#########################
# Install cert-manager
#########################

install_cert_manager() {
  log "Installing cert-manager in namespace '${CERT_MANAGER_NAMESPACE}'..."

  if ! helm repo list | grep -q '^jetstack'; then
    helm repo add jetstack https://charts.jetstack.io
  fi
  helm repo update

  kubectl get namespace "${CERT_MANAGER_NAMESPACE}" >/dev/null 2>&1 || \
    kubectl create namespace "${CERT_MANAGER_NAMESPACE}"

  helm upgrade --install "${CERT_MANAGER_RELEASE}" jetstack/cert-manager \
    --namespace "${CERT_MANAGER_NAMESPACE}" \
    --set installCRDs=true

  log "cert-manager installed."
}

#########################
# Create self-signed ClusterIssuer
#########################

create_selfsigned_clusterissuer() {
  log "Creating self-signed ClusterIssuer '${SELF_SIGNED_CLUSTERISSUER_NAME}'..."

  if kubectl get clusterissuer "${SELF_SIGNED_CLUSTERISSUER_NAME}" >/dev/null 2>&1; then
    log "ClusterIssuer '${SELF_SIGNED_CLUSTERISSUER_NAME}' already exists. Skipping."
    return
  fi

  cat <<EOF | kubectl apply -f -
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: ${SELF_SIGNED_CLUSTERISSUER_NAME}
spec:
  selfSigned: {}
EOF

  log "Self-signed ClusterIssuer '${SELF_SIGNED_CLUSTERISSUER_NAME}' created."
}

#########################
# Install Zalando Postgres Operator
#########################

install_postgres_operator() {
  log "Installing Zalando Postgres Operator in namespace '${POSTGRES_OPERATOR_NAMESPACE}'..."

  if ! helm repo list | grep -q '^zalando'; then
    helm repo add zalando https://opensource.zalando.com/postgres-operator/charts/postgres-operator
  fi
  helm repo update

  kubectl get namespace "${POSTGRES_OPERATOR_NAMESPACE}" >/dev/null 2>&1 || \
    kubectl create namespace "${POSTGRES_OPERATOR_NAMESPACE}"

  helm upgrade --install "${POSTGRES_OPERATOR_RELEASE}" zalando/postgres-operator \
    --namespace "${POSTGRES_OPERATOR_NAMESPACE}"

  log "Zalando Postgres Operator installed."
}

#########################
# Install simple Postgres
#########################

install_simple_postgres() {
  log "Installing simple Postgres from '${POSTGRES_YAML}'..."

  kubectl apply -f "${POSTGRES_YAML}"

  log "Simple Postgres installed."
}

#########################
# Main
#########################

main() {
  install_base_packages
  install_kubectl
  install_helm
  install_minikube
  ensure_minikube_running
  install_traefik
  install_cert_manager
  create_selfsigned_clusterissuer
  install_postgres_operator
  install_simple_postgres

  log "Setup complete. Summary:"
  kubectl get nodes
  echo
  kubectl get pods --all-namespaces
  echo
  helm list -A
  echo
  log "If using Traefik LoadBalancer on Minikube, you may want to run: 'minikube tunnel' in a separate terminal."
}

main "$@"
