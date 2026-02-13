//! Declarative state machine for OdooInstance lifecycle phases.
//!
//! Each phase has an `ensure()` method (via the [`State`] trait in
//! [`super::states`]) that runs every reconcile tick — idempotent outputs
//! that correct drift (PLC-style).
//!
//! Transitions are a static table of `(from, to, guard, action)`.  Guards are
//! pure functions over `(&OdooInstance, &ReconcileSnapshot)`.  The reconciler
//! calls `ensure()`, then evaluates guards; when one fires, it patches the
//! phase and requeues so the new state's `ensure()` runs next tick.

use std::time::Duration;

use k8s_openapi::api::{apps::v1::Deployment, batch::v1::Job};
use kube::api::{Api, ListParams, Patch, PatchParams, ResourceExt};
use kube::runtime::controller::Action;
use kube::Client;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use crate::crd::odoo_restore_job::OdooRestoreJob;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::crd::shared::Phase;
use crate::error::Result;

use super::helpers::FIELD_MANAGER;
use super::odoo_instance::Context;

// ── ReconcileSnapshot ───────────────────────────────────────────────────────

/// A point-in-time snapshot of the observed world, gathered once per reconcile.
/// Guards are pure functions over the summary fields.
/// State ensure() methods use the full CRD objects to read specs.
pub struct ReconcileSnapshot {
    // ── Deployment ────────────────────────────────────────────────────────
    pub ready_replicas: i32,
    pub deployment_replicas: i32,
    pub db_initialized: bool,

    // ── Job CRD presence (is there an active CRD for this job type?) ─────
    pub init_job_active: bool,
    pub restore_job_active: bool,
    pub upgrade_job_active: bool,
    pub backup_job_active: bool,

    // ── K8s batch/v1 Job outcomes (from the actual Job, not the CRD) ─────
    pub init_job_succeeded: bool,
    pub init_job_failed: bool,
    pub restore_job_succeeded: bool,
    pub restore_job_failed: bool,
    pub upgrade_job_succeeded: bool,
    pub upgrade_job_failed: bool,
    pub backup_job_succeeded: bool,
    pub backup_job_failed: bool,

    // ── Active job CRD objects (for ensure() to read specs) ───────────────
    pub active_init_job: Option<OdooInitJob>,
    pub active_restore_job: Option<OdooRestoreJob>,
    pub active_upgrade_job: Option<OdooUpgradeJob>,
    pub active_backup_job: Option<OdooBackupJob>,
}

impl ReconcileSnapshot {
    /// Gather the snapshot from the cluster.  All List/Get calls happen here,
    /// so the rest of the reconcile loop is synchronous guard evaluation.
    pub async fn gather(
        client: &Client,
        ns: &str,
        instance_name: &str,
        instance: &OdooInstance,
    ) -> Result<Self> {
        let db_initialized = instance
            .status
            .as_ref()
            .map(|s| s.db_initialized)
            .unwrap_or(false);

        // Deployment replicas (spec + ready).
        let (deployment_replicas, ready_replicas) = {
            let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
            match deps.get(instance_name).await {
                Ok(dep) => (
                    dep.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0),
                    dep.status.and_then(|s| s.ready_replicas).unwrap_or(0),
                ),
                Err(_) => (0, 0),
            }
        };

        let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);

        // ── Init jobs ───────────────────────────────────────────────────
        let mut active_init_job: Option<OdooInitJob> = None;
        let mut init_job_active = false;
        let mut db_init_from_jobs = db_initialized;
        {
            let inits: Api<OdooInitJob> = Api::namespaced(client.clone(), ns);
            for job in inits.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                let phase = job.status.as_ref().and_then(|s| s.phase.as_ref());
                match phase {
                    Some(Phase::Completed) => {
                        db_init_from_jobs = true;
                    }
                    Some(Phase::Failed) => {}
                    _ => {
                        // Running, Pending, or no status — this is the active one.
                        if active_init_job.is_none() {
                            init_job_active = true;
                            active_init_job = Some(job);
                        }
                    }
                }
            }
        }

        // ── Restore jobs ────────────────────────────────────────────────
        let mut active_restore_job: Option<OdooRestoreJob> = None;
        let mut restore_job_active = false;
        {
            let restores: Api<OdooRestoreJob> = Api::namespaced(client.clone(), ns);
            for job in restores.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                let phase = job.status.as_ref().and_then(|s| s.phase.as_ref());
                match phase {
                    Some(Phase::Completed) => {
                        db_init_from_jobs = true;
                    }
                    Some(Phase::Failed) => {}
                    _ => {
                        if active_restore_job.is_none() {
                            restore_job_active = true;
                            active_restore_job = Some(job);
                        }
                    }
                }
            }
        }

        // ── Upgrade jobs ────────────────────────────────────────────────
        let mut active_upgrade_job: Option<OdooUpgradeJob> = None;
        let mut upgrade_job_active = false;
        {
            let upgrades: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), ns);
            for job in upgrades.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                let phase = job.status.as_ref().and_then(|s| s.phase.as_ref());
                match phase {
                    Some(Phase::Completed) | Some(Phase::Failed) => {}
                    _ => {
                        if active_upgrade_job.is_none() {
                            upgrade_job_active = true;
                            active_upgrade_job = Some(job);
                        }
                    }
                }
            }
        }

        // ── Backup jobs ─────────────────────────────────────────────────
        let mut active_backup_job: Option<OdooBackupJob> = None;
        let mut backup_job_active = false;
        {
            let backups: Api<OdooBackupJob> = Api::namespaced(client.clone(), ns);
            for job in backups.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                let phase = job.status.as_ref().and_then(|s| s.phase.as_ref());
                match phase {
                    Some(Phase::Completed) | Some(Phase::Failed) => {}
                    _ => {
                        if active_backup_job.is_none() {
                            backup_job_active = true;
                            active_backup_job = Some(job);
                        }
                    }
                }
            }
        }

        // ── K8s Job outcomes ────────────────────────────────────────────
        // For each active CRD that has a jobName, look up the actual batch/v1
        // Job and check its succeeded/failed counts.
        let (init_job_succeeded, init_job_failed) = job_outcome(
            &jobs_api,
            active_init_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let (restore_job_succeeded, restore_job_failed) = job_outcome(
            &jobs_api,
            active_restore_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let (upgrade_job_succeeded, upgrade_job_failed) = job_outcome(
            &jobs_api,
            active_upgrade_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let (backup_job_succeeded, backup_job_failed) = job_outcome(
            &jobs_api,
            active_backup_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;

        Ok(Self {
            ready_replicas,
            deployment_replicas,
            db_initialized: db_init_from_jobs,
            init_job_active,
            restore_job_active,
            upgrade_job_active,
            backup_job_active,
            init_job_succeeded,
            init_job_failed,
            restore_job_succeeded,
            restore_job_failed,
            upgrade_job_succeeded,
            upgrade_job_failed,
            backup_job_succeeded,
            backup_job_failed,
            active_init_job,
            active_restore_job,
            active_upgrade_job,
            active_backup_job,
        })
    }
}

/// Look up a batch/v1 Job by name and return (succeeded, failed).
async fn job_outcome(jobs_api: &Api<Job>, job_name: Option<&str>) -> (bool, bool) {
    let Some(name) = job_name else {
        return (false, false);
    };
    match jobs_api.get(name).await {
        Ok(job) => {
            let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) > 0;
            let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0;
            (succeeded, failed)
        }
        Err(_) => (false, false),
    }
}

// ── Transition actions ──────────────────────────────────────────────────────

/// One-shot actions that fire on specific edges (the "/" in UML state diagrams).
/// These handle CRD status patching, events, and webhooks that belong to the
/// *transition*, not to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionAction {
    MarkDbInitialized,
    CompleteInitJob,
    FailInitJob,
    CompleteRestoreJob,
    FailRestoreJob,
    CompleteUpgradeJob,
    FailUpgradeJob,
    CompleteBackupJob,
    FailBackupJob,
}

pub async fn execute_action(
    action: TransitionAction,
    instance: &OdooInstance,
    ctx: &Context,
    snapshot: &ReconcileSnapshot,
) -> Result<()> {
    use TransitionAction::*;
    let ns = instance.namespace().unwrap_or_default();
    let client = &ctx.client;

    match action {
        MarkDbInitialized => {
            let name = instance.name_any();
            let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
            let patch = json!({"status": {"dbInitialized": true}});
            api.patch_status(
                &name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await?;
        }
        CompleteInitJob | FailInitJob => {
            if let Some(ref job) = snapshot.active_init_job {
                let crd_name = job.name_any();
                let now = crate::helpers::utc_now_odoo();
                let (phase_str, msg) = if matches!(action, CompleteInitJob) {
                    ("Completed", None)
                } else {
                    ("Failed", Some("init job failed"))
                };
                let mut patch_val = json!({"status": {"phase": phase_str, "completionTime": &now}});
                if let Some(m) = msg {
                    patch_val["status"]["message"] = json!(m);
                }
                let api: Api<OdooInitJob> = Api::namespaced(client.clone(), &ns);
                api.patch_status(
                    &crd_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch_val),
                )
                .await?;
            }
        }
        CompleteRestoreJob | FailRestoreJob => {
            if let Some(ref job) = snapshot.active_restore_job {
                let crd_name = job.name_any();
                let now = crate::helpers::utc_now_odoo();
                let (phase_str, phase_enum, msg) = if matches!(action, CompleteRestoreJob) {
                    ("Completed", Phase::Completed, None)
                } else {
                    ("Failed", Phase::Failed, Some("restore job failed"))
                };
                let mut patch_val = json!({"status": {"phase": phase_str, "completionTime": &now}});
                if let Some(m) = msg {
                    patch_val["status"]["message"] = json!(m);
                }
                let api: Api<OdooRestoreJob> = Api::namespaced(client.clone(), &ns);
                api.patch_status(
                    &crd_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch_val),
                )
                .await?;
                if let Some(ref wh) = job.spec.webhook {
                    crate::notify::notify_job_webhook(
                        client,
                        &ctx.http_client,
                        &ns,
                        wh,
                        &phase_enum,
                        job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                        msg,
                        Some(&now),
                    )
                    .await;
                }
            }
        }
        CompleteUpgradeJob | FailUpgradeJob => {
            if let Some(ref job) = snapshot.active_upgrade_job {
                let crd_name = job.name_any();
                let now = crate::helpers::utc_now_odoo();
                let (phase_str, phase_enum, msg) = if matches!(action, CompleteUpgradeJob) {
                    ("Completed", Phase::Completed, None)
                } else {
                    ("Failed", Phase::Failed, Some("upgrade job failed"))
                };
                let mut patch_val = json!({"status": {"phase": phase_str, "completionTime": &now}});
                if let Some(m) = msg {
                    patch_val["status"]["message"] = json!(m);
                }
                let api: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), &ns);
                api.patch_status(
                    &crd_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch_val),
                )
                .await?;
                if let Some(ref wh) = job.spec.webhook {
                    crate::notify::notify_job_webhook(
                        client,
                        &ctx.http_client,
                        &ns,
                        wh,
                        &phase_enum,
                        job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                        msg,
                        Some(&now),
                    )
                    .await;
                }
            }
        }
        CompleteBackupJob | FailBackupJob => {
            if let Some(ref job) = snapshot.active_backup_job {
                let crd_name = job.name_any();
                let now = crate::helpers::utc_now_odoo();
                let (phase_str, phase_enum, msg) = if matches!(action, CompleteBackupJob) {
                    ("Completed", Phase::Completed, None)
                } else {
                    ("Failed", Phase::Failed, Some("backup job failed"))
                };
                let mut patch_val = json!({"status": {"phase": phase_str, "completionTime": &now}});
                if let Some(m) = msg {
                    patch_val["status"]["message"] = json!(m);
                }
                let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), &ns);
                api.patch_status(
                    &crd_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch_val),
                )
                .await?;
                if let Some(ref wh) = job.spec.webhook {
                    crate::notify::notify_job_webhook(
                        client,
                        &ctx.http_client,
                        &ns,
                        wh,
                        &phase_enum,
                        job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                        msg,
                        Some(&now),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

// ── Transition table ────────────────────────────────────────────────────────

/// A single row in the transition table.
pub struct Transition {
    pub from: OdooInstancePhase,
    pub to: OdooInstancePhase,
    pub guard: fn(&OdooInstance, &ReconcileSnapshot) -> bool,
    pub guard_name: &'static str,
    pub actions: &'static [TransitionAction],
}

use OdooInstancePhase::*;

/// The complete lifecycle transition table.  First matching guard wins.
/// Order within a `from` group matters — more specific/urgent transitions first.
pub static TRANSITIONS: &[Transition] = &[
    // ── Provisioning ────────────────────────────────────────
    // Provisioning is the initial phase.  We transition out once the
    // instance controller has ensured all child resources.  For now the
    // ensure_* calls run before the state machine, so we always transition.
    Transition {
        from: Provisioning,
        to: Uninitialized,
        guard: |_, s| !s.db_initialized,
        guard_name: "!db_initialized",
        actions: &[],
    },
    Transition {
        from: Provisioning,
        to: Starting,
        guard: |_, s| s.db_initialized,
        guard_name: "db_initialized",
        actions: &[],
    },
    // ── Uninitialized ───────────────────────────────────────
    Transition {
        from: Uninitialized,
        to: Initializing,
        guard: |_, s| s.init_job_active,
        guard_name: "init_job_active",
        actions: &[],
    },
    // A restore can also bring us out of Uninitialized.
    Transition {
        from: Uninitialized,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    // ── Initializing ────────────────────────────────────────
    Transition {
        from: Initializing,
        to: Starting,
        guard: |_, s| s.init_job_succeeded,
        guard_name: "init_job_succeeded",
        actions: &[
            TransitionAction::CompleteInitJob,
            TransitionAction::MarkDbInitialized,
        ],
    },
    Transition {
        from: Initializing,
        to: InitFailed,
        guard: |_, s| s.init_job_failed,
        guard_name: "init_job_failed",
        actions: &[TransitionAction::FailInitJob],
    },
    // ── InitFailed ──────────────────────────────────────────
    // A new init job can retry.
    Transition {
        from: InitFailed,
        to: Initializing,
        guard: |_, s| s.init_job_active,
        guard_name: "init_job_active",
        actions: &[],
    },
    // A restore can also recover from InitFailed.
    Transition {
        from: InitFailed,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    // ── Starting ────────────────────────────────────────────
    Transition {
        from: Starting,
        to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        guard_name: "replicas == 0",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        guard_name: "upgrade_job_active",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: BackingUp,
        guard: |_, s| s.backup_job_active,
        guard_name: "backup_job_active",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: Running,
        guard: |i, s| s.ready_replicas >= i.spec.replicas && i.spec.replicas > 0,
        guard_name: "ready >= desired",
        actions: &[],
    },
    // ── Running ─────────────────────────────────────────────
    Transition {
        from: Running,
        to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        guard_name: "replicas == 0",
        actions: &[],
    },
    Transition {
        from: Running,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    Transition {
        from: Running,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        guard_name: "upgrade_job_active",
        actions: &[],
    },
    Transition {
        from: Running,
        to: BackingUp,
        guard: |_, s| s.backup_job_active,
        guard_name: "backup_job_active",
        actions: &[],
    },
    Transition {
        from: Running,
        to: Degraded,
        guard: |i, s| s.ready_replicas < i.spec.replicas && s.ready_replicas > 0,
        guard_name: "ready < desired && ready > 0",
        actions: &[],
    },
    Transition {
        from: Running,
        to: Starting,
        guard: |i, s| s.ready_replicas < i.spec.replicas && s.ready_replicas == 0,
        guard_name: "ready == 0",
        actions: &[],
    },
    // ── Degraded ────────────────────────────────────────────
    Transition {
        from: Degraded,
        to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        guard_name: "replicas == 0",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        guard_name: "upgrade_job_active",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: BackingUp,
        guard: |_, s| s.backup_job_active,
        guard_name: "backup_job_active",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: Running,
        guard: |i, s| s.ready_replicas >= i.spec.replicas,
        guard_name: "ready >= desired",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: Starting,
        guard: |_, s| s.ready_replicas == 0,
        guard_name: "ready == 0",
        actions: &[],
    },
    // ── BackingUp ───────────────────────────────────────────
    Transition {
        from: BackingUp,
        to: Stopped,
        guard: |i, s| s.backup_job_succeeded && i.spec.replicas == 0,
        guard_name: "backup_succeeded && replicas == 0",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Stopped,
        guard: |i, s| s.backup_job_failed && i.spec.replicas == 0,
        guard_name: "backup_failed && replicas == 0",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Running,
        guard: |i, s| s.backup_job_succeeded && s.ready_replicas >= i.spec.replicas,
        guard_name: "backup_succeeded && ready >= desired",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Running,
        guard: |i, s| s.backup_job_failed && s.ready_replicas >= i.spec.replicas,
        guard_name: "backup_failed && ready >= desired",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Degraded,
        guard: |i, s| {
            s.backup_job_succeeded && s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas
        },
        guard_name: "backup_succeeded && 0 < ready < desired",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Degraded,
        guard: |i, s| {
            s.backup_job_failed && s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas
        },
        guard_name: "backup_failed && 0 < ready < desired",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Starting,
        guard: |i, s| s.backup_job_succeeded && s.ready_replicas == 0 && i.spec.replicas > 0,
        guard_name: "backup_succeeded && ready == 0",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Starting,
        guard: |i, s| s.backup_job_failed && s.ready_replicas == 0 && i.spec.replicas > 0,
        guard_name: "backup_failed && ready == 0",
        actions: &[TransitionAction::FailBackupJob],
    },
    // ── Upgrading ───────────────────────────────────────────
    Transition {
        from: Upgrading,
        to: Starting,
        guard: |_, s| s.upgrade_job_succeeded,
        guard_name: "upgrade_job_succeeded",
        actions: &[TransitionAction::CompleteUpgradeJob],
    },
    Transition {
        from: Upgrading,
        to: Starting,
        guard: |_, s| s.upgrade_job_failed,
        guard_name: "upgrade_job_failed",
        actions: &[TransitionAction::FailUpgradeJob],
    },
    // ── Restoring ───────────────────────────────────────────
    Transition {
        from: Restoring,
        to: Starting,
        guard: |_, s| s.restore_job_succeeded,
        guard_name: "restore_job_succeeded",
        actions: &[
            TransitionAction::CompleteRestoreJob,
            TransitionAction::MarkDbInitialized,
        ],
    },
    Transition {
        from: Restoring,
        to: Starting,
        guard: |_, s| s.restore_job_failed,
        guard_name: "restore_job_failed",
        actions: &[TransitionAction::FailRestoreJob],
    },
    // ── Stopped ─────────────────────────────────────────────
    Transition {
        from: Stopped,
        to: Restoring,
        guard: |_, s| s.restore_job_active,
        guard_name: "restore_job_active",
        actions: &[],
    },
    Transition {
        from: Stopped,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        guard_name: "upgrade_job_active",
        actions: &[],
    },
    Transition {
        from: Stopped,
        to: Starting,
        guard: |i, _| i.spec.replicas > 0,
        guard_name: "replicas > 0",
        actions: &[],
    },
    // ── Error ───────────────────────────────────────────────
    Transition {
        from: Error,
        to: Starting,
        guard: |_, s| s.db_initialized,
        guard_name: "db_initialized",
        actions: &[],
    },
    Transition {
        from: Error,
        to: Uninitialized,
        guard: |_, s| !s.db_initialized,
        guard_name: "!db_initialized",
        actions: &[],
    },
];

// ── State machine runner ────────────────────────────────────────────────────

/// Run one cycle of the state machine.  Returns the Action for the kube-rs
/// controller runtime (requeue or await_change).
pub async fn run_state_machine(
    instance: &OdooInstance,
    ctx: &Context,
    snapshot: &ReconcileSnapshot,
) -> Result<Action> {
    let phase = instance
        .status
        .as_ref()
        .and_then(|s| s.phase.clone())
        .unwrap_or(Provisioning);

    // 1. State outputs — idempotent, corrects drift.
    let state = super::states::state_for(&phase);
    state.ensure(instance, ctx, snapshot).await?;

    // 2. Evaluate transitions — first matching guard wins.
    for t in TRANSITIONS.iter().filter(|t| t.from == phase) {
        if (t.guard)(instance, snapshot) {
            info!(
                name = %instance.name_any(),
                from = %phase,
                to = %t.to,
                "phase transition"
            );

            // Fire edge actions (UML "/").
            for action in t.actions {
                execute_action(*action, instance, ctx, snapshot).await?;
            }

            // Patch the phase.
            let ns = instance.namespace().unwrap_or_default();
            let name = instance.name_any();
            let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);
            let patch = json!({"status": {"phase": format!("{}", t.to)}});
            api.patch_status(
                &name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await?;

            // Requeue immediately so the new state's ensure() runs.
            return Ok(Action::requeue(Duration::ZERO));
        }
    }

    // 3. No transition — stay in current state, poll periodically.
    Ok(requeue_for(&phase))
}

/// Decide requeue strategy for phases that need periodic polling.
fn requeue_for(phase: &OdooInstancePhase) -> Action {
    match phase {
        Starting | Initializing | Restoring | Upgrading | BackingUp | Degraded => {
            Action::requeue(Duration::from_secs(10))
        }
        _ => Action::await_change(),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Scale a Deployment to the given replica count via merge patch.
/// Idempotent — safe to call every reconcile.
pub async fn scale_deployment(client: &Client, name: &str, ns: &str, replicas: i32) -> Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let patch = json!({"spec": {"replicas": replicas}});
    deployments
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await?;
    Ok(())
}
