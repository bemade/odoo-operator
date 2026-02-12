# odoo-operator — kube-rs Preview

This directory contains a **preview / spike** of what the odoo-operator would look
like if implemented in Rust with [kube-rs](https://kube.rs) instead of Go with
Kubebuilder. It is a 1:1 translation of the Go operator's architecture and
reconcile logic, written in idiomatic Rust.

> **Status:** Feature-complete preview. All 6 must-have gaps from the initial
> spike have been closed. Not yet wired into CI or the Helm chart.
> The Go operator in `../operator/` remains the production implementation.

---

## Architecture Comparison

### What stays the same

| Concern | Go (Kubebuilder) | Rust (kube-rs) |
|---|---|---|
| **CRD schemas** | `api/v1alpha1/*_types.go` + kubebuilder markers | `src/crd/*.rs` + `#[derive(CustomResource)]` |
| **5 controllers** | OdooInstance, InitJob, BackupJob, RestoreJob, UpgradeJob | Same — one module per controller |
| **Reconcile pattern** | `Reconcile(ctx, req) → (Result, error)` | `async fn reconcile(Arc<T>, Arc<Ctx>) → Result<Action>` |
| **Owns / Watches** | `.Owns(&Deployment{}).Watches(&OdooInitJob{}, ...)` | `.owns(deployments, ...).watches(init_jobs, ..., mapper)` |
| **Finalizer** | Manual add/remove with `controllerutil` | `kube::runtime::finalizer` helper (Apply/Cleanup enum) |
| **Status patching** | `r.Status().Patch(ctx, &instance, patch)` | `api.patch_status(name, params, &Patch::Merge(...))` |
| **Owner references** | `controllerutil.SetControllerReference(...)` | Explicit `OwnerReference` in `ObjectMeta` |
| **Shell scripts** | `//go:embed scripts/backup.sh` | `include_str!("../../scripts/backup.sh")` |

### What changes

| Concern | Go | Rust | Notes |
|---|---|---|---|
| **Concurrency model** | goroutines + controller-runtime manager | `tokio::select!` over 5 controller futures | Both are M:N green-thread models |
| **Error handling** | `error` interface + `fmt.Errorf` wrapping | `thiserror` enum + `?` operator | Rust forces exhaustive handling at compile time |
| **Code generation** | `make manifests` / `make generate` (CRDs, RBAC, DeepCopy) | `#[derive(CustomResource)]` generates CRD JSON at build time; no separate codegen step | **Big win** — no Makefile dance |
| **Webhook validation** | Separate webhook server + cert injection | Same pattern available via `kube::runtime::admission` | Not shown in preview |
| **Type safety** | Moderate — untyped `client.Patch` with raw JSON | Stronger — `Patch::Apply(&typed_struct)` | SSA (server-side apply) is more natural in kube-rs |
| **Binary size** | ~50 MB (Go static binary) | ~15-20 MB (Rust release, stripped) | Rust wins on size |
| **Compile time** | ~10s incremental | ~30-60s incremental (first build ~3-5 min) | Go wins on compile speed |
| **Memory usage** | ~30-50 MB at runtime | ~10-20 MB at runtime | Rust wins on memory |
| **Postgres client** | `pgx` (sync under the hood) | `tokio-postgres` (fully async) | Both work fine |
| **HTTP client** | `net/http` (stdlib) | `reqwest` (async) | Equivalent |
| **CLI flags** | `flag` stdlib | `clap` derive | Clap is more ergonomic |
| **Structured logging** | `logr` via controller-runtime | `tracing` + `tracing-subscriber` | tracing is more powerful (spans, async-aware) |

### Key Rust/kube-rs idioms

1. **`Arc<Context>`** — Shared state (client, defaults, postgres manager) is wrapped
   in `Arc` and passed to every reconcile call. No struct embedding like Go.

2. **`async fn reconcile(...) → Result<Action>`** — Each reconcile is an async
   function. `Action::requeue(Duration)` or `Action::await_change()` replaces
   `ctrl.Result{RequeueAfter: ...}`.

3. **`finalizer()` helper** — kube-rs provides a higher-level `finalizer()` function
   that handles the add/check/remove lifecycle as an `Apply` vs `Cleanup` enum match.
   This replaces the manual `ContainsFinalizer` / `AddFinalizer` / `RemoveFinalizer` dance.

4. **Server-Side Apply** — `Patch::Apply(&typed_struct)` with `PatchParams::apply(FIELD_MANAGER)`
   is the idiomatic way to ensure child resources. This replaces `CreateOrPatch` and
   avoids read-modify-write races.

5. **`#[derive(CustomResource)]`** — The CRD JSON schema is derived at compile time
   from the Rust struct + serde attributes. No separate `make manifests` step needed.
   You can dump the CRD YAML with:
   ```rust
   println!("{}", serde_yaml::to_string(&OdooInstance::crd())?);
   ```

6. **Trait-based abstraction** — `PostgresManager` is an `async_trait` so tests can
   inject a `NoopPostgresManager`. Same pattern as the Go `PostgresManager` interface.

---

## Project Layout

```
kube-rs-preview/
├── Cargo.toml                      # Dependencies
├── README.md                       # This file
├── scripts/                        # Shell scripts (shared with Go operator)
│   ├── backup.sh
│   ├── odoo-download.sh
│   ├── restore.sh
│   ├── s3-download.sh
│   └── s3-upload.sh
├── tests/
│   ├── helpers_test.rs             # Unit tests for helpers (odoo.conf, crypto, etc.)
│   └── webhook_test.rs             # CRD schema generation tests
└── src/
    ├── main.rs                     # Entry point (clap args, tokio::select!)
    ├── lib.rs                      # Module declarations
    ├── error.rs                    # Error enum (thiserror)
    ├── helpers.rs                  # OperatorDefaults, odoo.conf builder, crypto
    ├── notify.rs                   # Shared webhook notification + S3 credential reading
    ├── postgres.rs                 # PostgresManager async trait + tokio-postgres impl
    ├── webhook.rs                  # Validating admission webhook (warp HTTPS server)
    ├── crd/
    │   ├── mod.rs
    │   ├── shared.rs               # BackupFormat, Phase, WebhookConfig, etc.
    │   ├── odoo_instance.rs        # OdooInstance CRD
    │   ├── odoo_init_job.rs        # OdooInitJob CRD
    │   ├── odoo_backup_job.rs      # OdooBackupJob CRD
    │   ├── odoo_restore_job.rs     # OdooRestoreJob CRD
    │   └── odoo_upgrade_job.rs     # OdooUpgradeJob CRD
    └── controller/
        ├── mod.rs
        ├── odoo_instance.rs        # Main reconciler + applyDefaults + owner refs
        ├── odoo_init_job.rs        # Init job controller + webhook notify
        ├── odoo_backup_job.rs      # Backup job controller + S3 creds + webhook notify
        ├── odoo_restore_job.rs     # Restore job controller + S3 creds + webhook notify
        └── odoo_upgrade_job.rs     # Upgrade job controller + webhook notify
```

## Verdict

| Factor | Winner | Why |
|---|---|---|
| **Ecosystem maturity** | Go | Kubebuilder scaffolding, envtest, established patterns |
| **Type safety** | Rust | Compile-time guarantees, exhaustive error handling |
| **Runtime efficiency** | Rust | ~2-3x less memory, smaller binary |
| **Developer velocity** | Go | Faster compile, more K8s examples, easier onboarding |
| **Long-term maintenance** | Rust | No codegen, no magic comments, no "never edit" files, exhaustive error handling |
| **Testing** | Go | envtest is battle-tested; kube-rs testing is more manual |

### Why Rust wins on maintainability

Kubebuilder relies on **4 categories of files you must never edit** (CRD bases, RBAC
role, webhook manifests, `zz_generated.deepcopy.go`) plus **scaffold markers you must
never delete** (`// +kubebuilder:scaffold:*`). The entire CRD schema and RBAC policy
is expressed as **magic comments** (`// +kubebuilder:rbac:...`,
`// +kubebuilder:validation:...`) that a separate codegen tool scrapes — typo one and
you get a silent failure at `make manifests` time.

In Rust:
- **Validation** is the type system: `enum`, `Option<T>`, serde attributes — the
  compiler checks them.
- **RBAC** lives in a manifest or config file, not magic comments.
- **DeepCopy** is `#[derive(Clone)]` — not a generated file you're told never to touch.
- **CRD generation** is `#[derive(CustomResource)]` with typed attributes — no
  comment-scraping tool producing YAML you're also told never to edit.
- **Error handling** via `thiserror` enums forces every error path to be handled. In Go,
  it's trivially easy to forget `if err != nil` or silently swallow errors with
  `_ = someFunc()`.

Zero generated files. Zero "never edit" files. Zero scaffold markers. Everything is
derived at compile time.

**Recommendation:** The kube-rs version is structurally identical to the Go operator and
now feature-complete. For teams comfortable with Rust, it is the stronger long-term
choice. For teams prioritizing onboarding speed and ecosystem familiarity, Go/Kubebuilder
remains pragmatic. The code is a 1:1 translation — switching between them is
straightforward.
