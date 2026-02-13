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

```
├── Cargo.toml
├── Dockerfile
├── Makefile
├── charts/odoo-operator/          # Helm chart
├── scripts/                       # Shell scripts embedded into Jobs
│   ├── backup.sh
│   ├── restore.sh
│   ├── s3-download.sh
│   ├── s3-upload.sh
│   └── odoo-download.sh
├── tests/
│   ├── integration/               # envtest integration tests (12 tests)
│   │   ├── main.rs
│   │   ├── common.rs              # Shared harness, helpers, TestContext
│   │   ├── bootstrap.rs
│   │   ├── child_resources.rs
│   │   ├── scaling.rs
│   │   ├── degraded.rs
│   │   ├── init_job.rs
│   │   ├── backup_job.rs
│   │   ├── upgrade_job.rs
│   │   ├── restore_job.rs
│   │   └── finalizer.rs
│   ├── controller_helpers_test.rs  # Unit tests for OdooJobBuilder, helpers
│   ├── helpers_test.rs             # Unit tests for odoo.conf, crypto
│   └── webhook_test.rs             # Admission webhook tests
└── src/
    ├── main.rs                     # Entry point (clap, tokio::select!)
    ├── lib.rs
    ├── error.rs                    # Error enum (thiserror)
    ├── helpers.rs                  # OperatorDefaults, odoo.conf builder, crypto
    ├── notify.rs                   # Webhook notifications + S3 credentials
    ├── postgres.rs                 # PostgresManager async trait
    ├── webhook.rs                  # Validating admission webhook (warp)
    ├── crd/                        # Custom Resource Definitions
    │   ├── odoo_instance.rs
    │   ├── odoo_init_job.rs
    │   ├── odoo_backup_job.rs
    │   ├── odoo_restore_job.rs
    │   └── odoo_upgrade_job.rs
    └── controller/
        ├── odoo_instance.rs        # Main reconciler, finalizer, events
        ├── state_machine.rs        # ReconcileSnapshot, transitions, guards
        ├── states/                 # One file per OdooInstancePhase
        ├── child_resources.rs      # ensure_* functions for K8s resources
        └── helpers.rs              # OdooJobBuilder, shared constants
```

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
