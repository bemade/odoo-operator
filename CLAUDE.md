# Odoo Operator

Rust Kubernetes operator for Odoo instance lifecycle management. Built with
kube-rs, deployed via Helm.

## Quick Reference

- **Language**: Rust (edition 2021)
- **K8s client**: kube-rs v0.98 + k8s-openapi v0.24
- **License**: LGPL-3.0-or-later
- **Registry**: ghcr.io/bemade/odoo-operator

## Architecture

### State Machine (PLC-style)

The OdooInstance lifecycle is a declarative state machine in
`src/controller/state_machine.rs`. Each phase has an idempotent `ensure()`
method (in `src/controller/states/<phase>.rs`) that runs every reconcile tick.
Transitions are a static table of `(from, to, guard, action)` tuples. Guards
are pure functions over `ReconcileSnapshot`.

**Phases**: Provisioning, Uninitialized, Initializing, InitFailed, Starting,
Running, Degraded, Stopped, Upgrading, Restoring, BackingUp, Error.

Auto-generate the Mermaid diagram: `make state-machine`

### Reconcile Loop

1. `ReconcileSnapshot::gather()` — single pass of all API reads
2. `ensure()` — current state's idempotent outputs (scale deployments, create jobs)
3. Evaluate guards — if one fires, patch phase and requeue

### Web/Cron Split

Each OdooInstance creates **two Deployments**:
- Web (`<name>`): `--max-cron-threads=0`, HTTP on 8069/8072
- Cron (`<name>-cron`): `--no-http --workers=0`, no service/ingress

Both must be scaled to 0 during Initializing, InitFailed, Restoring, Stopped.
Only cron scales to 0 during Upgrading (web stays up).

### Child Resources

`src/controller/child_resources.rs` builds: Deployment (x2), Service, Ingress,
PVC, ConfigMap, Secret, ServiceAccount, Role, RoleBinding. All use server-side
apply with `FIELD_MANAGER = "odoo-operator"`.

### Job CRs

OdooInitJob, OdooUpgradeJob, OdooBackupJob, OdooRestoreJob each reference a
target OdooInstance. The operator creates underlying batch/v1 Jobs with shell
scripts from `scripts/`. Job CRs track status (Pending, Running, Completed,
Failed) and the underlying Job name.

## Naming Conventions

- PostgreSQL user: `odoo.{namespace}.{instance_name}`
- Database name: `odoo_{sanitized_uid}`
- Cron deployment: `{instance_name}-cron` (helper: `cron_depl_name()`)
- ConfigMap: `{instance_name}-odoo-conf`
- PVC: `{instance_name}-filestore-pvc`
- Secret: `{instance_name}-db-password`

## Build & Test

```sh
cargo build                           # debug build
cargo test                            # all tests (unit + integration)
cargo test --test integration         # integration only (needs envtest)
cargo fmt --check && cargo clippy -- -D warnings  # lint
make helm-crds                        # generate CRDs + sync to Helm chart
make install                          # build + deploy to minikube
```

### Testing

Integration tests use envtest (real API server + etcd, no kubelet). Each test
gets a unique namespace. Deployment/Job status must be faked via status
subresource patches. Key helpers in `tests/integration/common.rs`:

- `TestContext::new()` — creates namespace + OdooInstance
- `wait_for_phase()` — poll until expected phase
- `fake_deployment_ready()` / `fake_job_succeeded()` — simulate kubelet
- `fast_track_to_running()` — shortcut from Uninitialized to Running
- `check_deployment_scale()` — assert deployment replica count

### CI

- PRs: fmt + clippy + unit tests + integration tests
- Release: release-please on master → Docker image + Helm chart to GHCR

## Key Files

| Path | What |
|---|---|
| `src/controller/state_machine.rs` | State machine transitions, guards, actions, snapshot |
| `src/controller/states/*.rs` | Per-phase ensure() implementations |
| `src/controller/child_resources.rs` | K8s object builders |
| `src/controller/odoo_instance.rs` | Main reconciler |
| `src/controller/helpers.rs` | Utility functions (volumes, env vars, job builder) |
| `src/crd/*.rs` | CRD type definitions |
| `src/webhook.rs` | Validating admission webhook |
| `src/postgres.rs` | PostgreSQL user/db management |
| `scripts/*.sh` | Shell scripts embedded in job pods |
| `charts/odoo-operator/` | Helm chart |
| `tests/integration/` | envtest integration tests |

## Scaling Rules by Phase

| Phase | Web | Cron | Notes |
|---|---|---|---|
| Restoring | 0 | 0 | Both down during DB restore |
| Initializing | 0 | 0 | Both down during DB init |
| InitFailed | 0 | 0 | Both stay down |
| Upgrading | unchanged | 0 | Web serves traffic, cron stops |
| Stopped | 0 | 0 | User set replicas=0 |
| Starting | spec.replicas | spec.cron.replicas | Scaling up |
| Running | spec.replicas | spec.cron.replicas | Steady state |
| BackingUp | unchanged | unchanged | Non-disruptive |
| Degraded | unchanged | unchanged | Partial readiness |
