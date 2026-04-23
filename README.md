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
```

By default, the operator automatically initializes the database with the `base`
module. To skip auto-init (e.g. when restoring from a backup), set
`spec.init.enabled: false`.

## Configuration Reference

### OdooInstance spec

| Field | Default | Description |
|---|---|---|
| `image` | operator default | Odoo container image |
| `replicas` | `1` | Number of web pods. Set to `0` to stop the instance |
| `adminPassword` | — | Odoo master password |
| `imagePullSecret` | — | Name of a `kubernetes.io/dockerconfigjson` secret in the operator namespace (auto-copied to instance namespace) |
| `ingress.hosts` | — | Hostnames to expose the instance on |
| `ingress.issuer` | operator default | cert-manager ClusterIssuer for TLS (ignored when `gatewayRef` is set) |
| `ingress.class` | operator default | IngressClass name |
| `ingress.gatewayRef.name` | operator default | Gateway name for HTTPRoute (creates HTTPRoute instead of Ingress) |
| `ingress.gatewayRef.namespace` | operator default | Gateway namespace for HTTPRoute |
| `database.cluster` | secret default | Postgres cluster name from the pg-clusters secret |
| `database.name` | auto-generated | Database name. Defaults to `odoo_<uid>` if unset |
| `init.enabled` | `true` | Automatically initialize the database when the instance is first created (skipped when `productionInstanceRef` is set) |
| `init.modules` | `["base"]` | Odoo modules to install during auto-initialization |
| `init.webhook` | — | Webhook to notify on init job status changes |
| `productionInstanceRef.name` | — | Name of a source production `OdooInstance` to clone from on first init. When set, the operator creates an `OdooStagingRefreshJob` instead of an `OdooInitJob`. Forbidden on `environment: Production`. See [Staging from production](#staging-from-production) |
| `productionInstanceRef.namespace` | same as target | Reserved for a future cross-namespace phase; must equal (or omit) the target's namespace in v1 |
| `filestore.storageSize` | `2Gi` | PVC size. Can only be increased, not decreased |
| `filestore.storageClass` | operator default | StorageClass for the filestore PVC. Immutable after creation |
| `resources` | operator default | CPU/memory requests and limits for web pods |
| `cron.replicas` | `1` | Number of cron pods (see [Web/Cron Split](#webcron-split) below) |
| `cron.resources` | same as `resources` | CPU/memory requests and limits for cron pods |
| `strategy.type` | `Recreate` | Deployment strategy (`Recreate` or `RollingUpdate`) |
| `strategy.rollingUpdate.maxUnavailable` | `25%` | Max unavailable pods during rolling update |
| `strategy.rollingUpdate.maxSurge` | `25%` | Max extra pods during rolling update |
| `probes.startupPath` | `/web/health` | Startup probe path |
| `probes.livenessPath` | `/web/health` | Liveness probe path |
| `probes.readinessPath` | `/web/health` | Readiness probe path |
| `configOptions` | — | Extra key-value pairs appended to `odoo.conf` |
| `webhook.url` | — | URL to receive status change callbacks |
| `affinity` | operator default | Pod affinity rules |
| `tolerations` | operator default | Pod tolerations |

### Web/Cron Split

Each OdooInstance creates two Deployments:

- **Web** (`<name>`) — runs with `--max-cron-threads=0`, serves HTTP traffic on
  ports 8069 and 8072 (websocket). Scaled by `spec.replicas`.
- **Cron** (`<name>-cron`) — runs with `--no-http`, processes scheduled actions
  only. Scaled by `spec.cron.replicas`.

This separation means you can scale web workers independently of cron processing.
Cron pods don't need an HTTP port, so they have no service or ingress routing. During
upgrades and restores, the cron deployment is automatically scaled to zero to avoid
stale connections.

You don't need to set `workers` or `max_cron_threads` in `configOptions` — the
operator handles this automatically via the command-line flags on each deployment.

### Staging from production

Set `spec.productionInstanceRef` on a staging `OdooInstance` to declaratively
tie it to a source-of-truth production instance. On first reconcile, the
operator creates an `OdooStagingRefreshJob` (named `<instance>-auto-refresh`)
that clones the prod DB + filestore into the new instance and runs
`odoo neutralize` — in place of the normal `OdooInitJob` path:

```yaml
apiVersion: bemade.org/v1alpha1
kind: OdooInstance
metadata:
  name: client-staging
  namespace: client
spec:
  adminPassword: admin
  image: odoo:18.0
  ingress:
    hosts: [client-staging.example.com]
  filestore:
    storageSize: 50Gi
  environment: Staging
  productionInstanceRef:
    name: client-prod
```

Requirements and limits:

- Forbidden on `environment: Production` (rejected at `kubectl apply` time
  by a CRD CEL rule).
- Same-namespace only in v1. `productionInstanceRef.namespace` is reserved
  for a future cross-namespace phase.
- Auto-refresh fires only on first initialization (when
  `status.dbInitialized` is false). A user-created `OdooStagingRefreshJob`
  pre-empts the auto-create — useful for tuning `filestoreMethod` or
  `skipFilestore`, which the auto-create path leaves at CRD defaults
  (`Auto` / `false`).

### Gateway API Support

By default the operator creates a `networking.v1/Ingress` for each instance. If your
cluster uses Istio or another Gateway API implementation, set `ingress.gatewayRef` to
create an `HTTPRoute` instead:

```yaml
spec:
  ingress:
    hosts:
      - myodoo.example.com
    gatewayRef:
      name: my-gateway
      namespace: istio-system
```

TLS is not managed by the operator in Gateway API mode — configure it on your Gateway
resource (wildcard cert, cert-manager Gateway integration, etc.). The `issuer` field is
ignored when `gatewayRef` is set.

To make all instances use Gateway API by default, set the operator-level defaults:

```sh
helm upgrade odoo-operator ... \
  --set defaults.gatewayRefName=my-gateway \
  --set defaults.gatewayRefNamespace=istio-system
```

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
| `--default-gateway-ref-name` | — | Gateway name; when both name and namespace are set, creates HTTPRoute instead of Ingress |
| `--default-gateway-ref-namespace` | — | Gateway namespace for the default HTTPRoute parentRef |
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
