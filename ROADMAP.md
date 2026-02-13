# Odoo Operator Roadmap

## What's Built (v1alpha1)

- **OdooInstance controller**: full lifecycle management (provisioning, init, running, stopped, degraded, upgrading, restoring, error)
- **OdooInitJob controller**: one-shot database initialization via `odoo --init`
- **OdooUpgradeJob controller**: runs `odoo -u all` then rolls the deployment
- **OdooBackupJob / OdooRestoreJob controllers**: database + filestore backup and restore
- **Spec defaulting**: image, storageClass, storageSize, ingressClass, ingressIssuer, resources, affinity, tolerations — all written to spec on first reconcile from operator flags; `database.cluster` resolved from the pg-clusters secret `default: true` entry
- **Validating webhook**: rejects storage size decreases, storage class changes, and database cluster changes after initial set
- **Kubernetes Events**: emitted on phase transitions, spec defaulting, child resource errors
- **Filestore expansion**: PVC storage request increased automatically when `spec.filestore.storageSize` is raised (requires `allowVolumeExpansion: true` on the StorageClass)
- **CRD migration**: v1 legacy shim + post-install hook migrates objects from old Kopf operator storage; webhook infrastructure from the previous Python operator cleaned up automatically on install/upgrade

---

## High Priority

### Split Web/Cron Deployments

Currently a single Deployment handles both web workers and cron threads. Splitting into two Deployments would:

- Allow cron to be scaled to 0 during upgrades, eliminating serialization errors (`ir_module_module` lock contention that plagued Odoo.sh)
- Enable independent resource allocation and scaling for web vs cron
- Give cleaner logs and metrics per workload type

**Proposed spec addition**:
```yaml
spec:
  replicas: 3        # web workers
  cron:
    replicas: 1
    resources: ...   # defaults to spec.resources if unset
```

The operator would always create both deployments; `spec.cron.replicas: 0` disables cron.

---

### Auto-Upgrade on Image Change

When `spec.image` changes, automatically:
1. Scale cron to 0
2. Run `OdooUpgradeJob` (`odoo -u all --stop-after-init`)
3. On success: roll out new image to both deployments, scale cron back up
4. On failure: keep old image running, emit warning event, set phase to `UpgradeFailed`

PostgreSQL transaction semantics give automatic rollback on failure — zero risk to the database.

Controlled by `spec.autoUpgrade.enabled` (default: `true`).

---

### Database Cluster Migration

Changing `spec.database.cluster` is currently blocked by the validating webhook. A proper migration path requires:
1. Stopping the instance
2. Copying the database and filestore to the target cluster
3. Updating the spec and restarting

This needs design work around atomicity and failure recovery before the webhook block can be lifted.

---

## Medium Priority

### Observability

- Expose Prometheus metrics for reconcile duration, phase transitions, upgrade outcomes
- ~~`status.conditions[]` following standard Kubernetes conventions~~ ✅ Done — `Ready` and `Progressing` conditions emitted
- `status.observedGeneration` for drift detection

### Pre-flight Validation

Before creating/updating a deployment:
- Verify the image is pullable
- Verify database connectivity
- Validate `configOptions` keys against known `odoo.conf` parameters

### Webhook Callbacks

`spec.webhook.url` is already in the type. Wire it up: POST a JSON payload to the configured URL on every phase transition (useful for external monitoring and CI/CD pipelines).

---

## Low Priority / Future

### Automatic Rollback

On detected post-upgrade failures (readiness probe failures, high error rate in logs), automatically revert to the previous image. Database migrations are never rolled back — only the deployment image.

### Multi-Cluster / Multi-Tenant

- Network policies between instances
- Resource quotas per namespace
- Cross-cluster backup restore

### Grafana Dashboards

Pre-built dashboards for OdooInstance health, upgrade history, backup status.

---

## Decision Log

### 2026-02-11: Exploratory rewrite from Python/Kopf to Go/kubebuilder

**Decision**: Replace the Python Kopf operator entirely with a Go kubebuilder operator.

**Reasoning**:
- Kopf's async model made multi-step workflows (init → deploy → upgrade) awkward; controller-runtime's reconcile loop handles them naturally
- Typed Go APIs catch schema mistakes at compile time; Python dicts do not
- `envtest` + Ginkgo gives fast, hermetic controller tests without a real cluster
- kubebuilder generates CRD schemas, RBAC, and webhook scaffolding from code annotations
- The Go binary is a single statically-linked ~40MB container vs a Python image with many dependencies

**Result**: 67 passing controller tests, full feature parity, deployed in ~5 hours.

---

### 2026-02-13: Exploratory rewrite from Go/kubebuilder to Rust/kube-rs

**Decision**: Replace the Go kubebuilder operator with a Rust kube-rs operator.

**Reasoning**:
- Zero codegen: `#[derive(CustomResource)]` replaces `make manifests` / `make generate` / magic comments
- No "never edit" files (CRD bases, `zz_generated.deepcopy.go`, scaffold markers)
- `thiserror` enums force exhaustive error handling at compile time
- ~47 MB image vs ~247 MB with Go, ~10 MB runtime memory vs ~30 MB
- Event-driven state machine with static transition table is cleaner in Rust's type system
- State and transition tables are defined in code, not in comments or separate files,
  and easily audited by human or AI eyes.
- `tracing` gives async-aware structured logging with spans
- envtest is available in a "Rustified" version with the envtest crate, closing the testing
  gap vs. Go.

**Architecture changes**:
- PLC-inspired state machine: `ensure()` called every tick (idempotent outputs), static `TRANSITIONS` table with guards and one-shot actions
- Shared `envtest` server across all integration tests via `std::sync::OnceLock`, isolated by Kubernetes namespaces — 12 tests in ~30s parallel
- Validating admission webhook via `warp` HTTPS server with cert-manager TLS
- Automatic migration: strips legacy `kopf.zalando.org/KopfFinalizerMarker` finalizers
- Standard Kubernetes `Ready` and `Progressing` conditions on status

**Result**: 12 integration tests + unit tests, full feature parity, ~15 MB distroless container.

---

### 2026-02-11: Postgres cluster as secret contract, not helm value

**Decision**: Remove `defaults.postgresCluster` helm value. The pg-clusters secret is the single source of truth; one cluster must be marked `default: true`.

**Reasoning**: The secret is already required. A duplicate helm value creates two places to configure the same thing and divergence bugs. The operator reads `default: true` from the secret at runtime and writes the resolved name into `spec.database.cluster` on first reconcile.

---

### 2026-02-11: Spec self-description via applyDefaults

**Decision**: On first reconcile, write all operator defaults (image, storageClass, storageSize, ingressClass, ingressIssuer, resources, affinity, tolerations, database.cluster) into the OdooInstance spec.

**Reasoning**: Makes the spec self-describing — `kubectl get odooinstance -o yaml` shows exactly what the instance is running, without needing to know the operator's flag values. All downstream controllers (BackupJob, UpgradeJob, etc.) can read config from the spec rather than the operator's runtime configuration.

---

### 2026-02-11: Validating webhook over reconcile-loop revert

**Decision**: Reject invalid spec changes (storage size decrease, storage class change, database cluster change) at admission time via a validating webhook rather than silently reverting in the reconcile loop.

**Reasoning**: Admission-time rejection gives immediate, clear feedback to the user (`kubectl apply` fails with an error). Silent revert in the reconcile loop is confusing — the apply succeeds but the spec silently changes back. The only tradeoff is the webhook infrastructure requirement (cert-manager), which is already a prerequisite for ingress TLS.
