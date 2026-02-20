# odoo-operator

A Kubernetes operator for managing Odoo instances. Declaratively deploy, initialize,
upgrade, back up, and restore Odoo databases using custom resources.

Built with [kube-rs](https://kube.rs) in Rust. Deployed via Helm.

## Custom Resources

| Resource | Purpose |
|---|---|
| `OdooInstance` | Declares a running Odoo deployment: image, replicas, ingress, filestore, database |
| `OdooInitJob` | One-shot job to initialize a fresh database |
| `OdooUpgradeJob` | Runs `odoo -u` against an existing database, then rolls the deployment |
| `OdooBackupJob` | Dumps the database and filestore to object storage |
| `OdooRestoreJob` | Restores a backup into an OdooInstance |

## Prerequisites

- Kubernetes 1.26+
- cert-manager (for webhook TLS)
- A PostgreSQL cluster accessible from the operator namespace
- Helm 3

## Installation

### 1. Create the postgres clusters secret

```yaml
# clusters.yaml
main:
  host: postgres.postgres.svc.cluster.local
  port: 5432
  adminUser: postgres
  adminPassword: secret
  default: true
```

```sh
kubectl create namespace odoo-operator
kubectl create secret generic pg-clusters -n odoo-operator \
  --from-file=clusters.yaml=clusters.yaml
```

### 2. Install the chart

```sh
helm upgrade --install odoo-operator oci://ghcr.io/bemade/odoo-operator/charts/odoo-operator \
  --namespace odoo-operator \
  --set defaults.ingressClass=nginx \
  --set defaults.ingressIssuer=letsencrypt-prod
```

**NOTE**: If you have a previously installed version of the chart, you may need to
completely uninstall and reinstall it. This chart is still in early and active
development and breaking changes are still frequent. Uninstalling the chart does not
remove all your running OdooInstances. You may also need to clear the odoo-operator
ServiceAccount resource along with the ClusterRole and ClusterRoleBinding of the same
name. These previously had Helm hook values on them that break installation of later
versions starting at v0.13.3.

### 3. Deploy an Odoo instance

```yaml
apiVersion: bemade.org/v1alpha1
kind: OdooInstance
metadata:
  name: myodoo
  namespace: odoo
spec:
  image: odoo:18.0
  adminPassword: changeme
  replicas: 1
  ingress:
    hosts:
      - myodoo.example.com
```

```sh
kubectl apply -f odoo.yaml

# Initialize the database
kubectl apply -f - <<EOF
apiVersion: bemade.org/v1alpha1
kind: OdooInitJob
metadata:
  name: myodoo-init
  namespace: odoo
spec:
  odooInstanceRef:
    name: myodoo
EOF
```

## Configuration Reference

### OdooInstance spec

| Field | Default | Description |
|---|---|---|
| `image` | operator default | Odoo container image |
| `replicas` | `1` | Number of Odoo pods. Set to `0` to stop |
| `adminPassword` | — | Odoo master password |
| `imagePullSecret` | — | Name of a `kubernetes.io/dockerconfigjson` secret in the operator namespace (auto-copied to instance namespace) |
| `ingress.hosts` | — | Hostnames to expose the instance on |
| `ingress.issuer` | operator default | cert-manager ClusterIssuer for TLS |
| `ingress.class` | operator default | IngressClass name |
| `database.cluster` | secret default | Postgres cluster name from the pg-clusters secret |
| `filestore.storageSize` | `2Gi` | PVC size. Can only be increased, not decreased |
| `filestore.storageClass` | operator default | StorageClass for the filestore PVC. Immutable after creation |
| `resources` | operator default | CPU/memory requests and limits |
| `strategy.type` | `Recreate` | Deployment strategy (`Recreate` or `RollingUpdate`) |
| `probes.*` | `/web/health` | Liveness/readiness/startup probe paths |
| `configOptions` | — | Extra key-value pairs appended to `odoo.conf` |
| `affinity` | operator default | Pod affinity rules |
| `tolerations` | operator default | Pod tolerations |

### Status conditions

The operator sets standard Kubernetes conditions on each OdooInstance:

| Condition | Description |
|---|---|
| `Ready` | `True` when the instance is in the `Running` phase |
| `Progressing` | `True` during transient phases (Provisioning, Initializing, Starting, Upgrading, Restoring, BackingUp) |

### Webhook validation

The validating webhook rejects:
- `spec.filestore.storageSize` decreases
- `spec.filestore.storageClass` changes after initial set
- `spec.database.cluster` changes after initial set

### Operator flags

| Flag | Default | Description |
|---|---|---|
| `--postgres-clusters-secret` | `postgres-clusters` | Secret name in operator namespace |
| `--default-odoo-image` | `odoo:18.0` | Image used when `spec.image` is unset |
| `--default-storage-class` | `standard` | StorageClass when `spec.filestore.storageClass` is unset |
| `--default-storage-size` | `2Gi` | PVC size when `spec.filestore.storageSize` is unset |
| `--default-ingress-class` | — | IngressClass when `spec.ingress.class` is unset |
| `--default-ingress-issuer` | — | ClusterIssuer when `spec.ingress.issuer` is unset |
| `--default-resources` | — | JSON `ResourceRequirements` when `spec.resources` is unset |
| `--default-affinity` | — | JSON `Affinity` when `spec.affinity` is unset |
| `--default-tolerations` | — | JSON `[]Toleration` when `spec.tolerations` is unset |

## State Machine

The OdooInstance lifecycle is driven by a declarative state machine with transitions
defined in a static table in `src/controller/state_machine.rs`.

See [STATE_MACHINE.md](STATE_MACHINE.md) for the full diagram (auto-generated with `make state-machine`).

## Project Layout

| Directory | Contents |
|---|---|
| `src/crd/` | Custom Resource types (OdooInstance, OdooInitJob, etc.) |
| `src/controller/` | Reconciler, declarative state machine, child resource management |
| `src/controller/states/` | One file per OdooInstancePhase (12 states), each with an idempotent `ensure()` |
| `src/bin/` | CLI tools — CRD YAML generator, Mermaid state diagram generator |
| `scripts/` | Shell scripts embedded into backup/restore/init Jobs |
| `charts/odoo-operator/` | Helm chart |
| `tests/integration/` | envtest-based integration tests (17 tests, parallel via per-test namespaces) |
| `tests/` | Unit tests for helpers, job builder, and admission webhook |
| `testing/` | Local dev/test fixtures (pg-clusters secret, Helm overrides) |
| `.github/workflows/` | CI (lint + test on PRs) and release (image + Helm chart on tag push) |

## Development

```sh
# Run all tests (unit + integration)
cargo test

# Run integration tests only (requires envtest binaries)
cargo test --test integration

# Lint
cargo fmt --check && cargo clippy -- -D warnings

# Generate CRDs and sync to Helm chart
make helm-crds

# Build and deploy to minikube
make install
```

## License

LGPL-3.0-or-later — Copyright 2026 Marc Durepos, Bemade Inc.
