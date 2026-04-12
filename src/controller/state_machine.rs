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

use k8s_openapi::api::{
    apps::v1::Deployment,
    batch::v1::{Job, JobSpec},
    core::v1::{
        Container, PersistentVolumeClaim, PersistentVolumeClaimSpec,
        PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Volume, VolumeMount,
    },
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::controller::Action;
use kube::Client;
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use crate::crd::odoo_restore_job::OdooRestoreJob;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::crd::shared::Phase;
use crate::error::Result;

use super::helpers::{cron_depl_name, FIELD_MANAGER};
use super::odoo_instance::Context;

// ── JobStatus ────────────────────────────────────────────────────────────────

/// Observed status of a job CR + its underlying batch/v1 Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// No active CR for this job type (none exists, or all are Completed/Failed).
    Absent,
    /// An active CR exists but the K8s Job hasn't finished yet.
    Active,
    /// The K8s Job succeeded.
    Succeeded,
    /// The K8s Job failed (or was deleted / is being deleted).
    Failed,
}

impl JobStatus {
    /// True when a CR is present and not yet finalised (Active, Succeeded, or Failed).
    pub fn is_present(self) -> bool {
        self != Self::Absent
    }
}

/// Returns true if `scheduled_time` is `None` (run immediately) or in the past.
fn scheduled_time_reached(scheduled: Option<&str>) -> bool {
    match scheduled {
        None => true,
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t <= chrono::Utc::now())
            .unwrap_or(true), // unparseable → run immediately rather than block forever
    }
}

// ── ReconcileSnapshot ───────────────────────────────────────────────────────

/// A point-in-time snapshot of the observed world, gathered once per reconcile.
/// Guards are pure functions over the summary fields.
/// State ensure() methods use the full CR objects to read specs.
pub struct ReconcileSnapshot {
    // ── Deployment ────────────────────────────────────────────────────────
    pub ready_replicas: i32,
    pub deployment_replicas: i32,
    pub cron_ready_replicas: i32,
    pub cron_deployment_replicas: i32,
    pub db_initialized: bool,

    // ── Job CR status (combines presence + K8s Job outcome) ──────────────
    pub init_job: JobStatus,
    pub restore_job: JobStatus,
    pub upgrade_job: JobStatus,
    pub backup_job: JobStatus,

    // ── Active job CR objects (for ensure() to read specs) ────────────────
    pub active_init_job: Option<OdooInitJob>,
    pub active_restore_job: Option<OdooRestoreJob>,
    pub active_upgrade_job: Option<OdooUpgradeJob>,
    pub active_backup_job: Option<OdooBackupJob>,

    // ── Filestore migration ─────────────────────────────────────────────
    pub storage_class_mismatch: bool,
    pub actual_storage_class: Option<String>,
    pub migration_job: JobStatus,
}

impl ReconcileSnapshot {
    /// True when an upgrade job CR is present AND its `scheduledTime` has been
    /// reached (or was omitted, meaning "run immediately").
    pub fn upgrade_job_ready(&self) -> bool {
        self.upgrade_job.is_present()
            && scheduled_time_reached(
                self.active_upgrade_job
                    .as_ref()
                    .and_then(|j| j.spec.scheduled_time.as_deref()),
            )
    }

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

        // Cron replicas (spec + ready).
        let (cron_deployment_replicas, cron_ready_replicas) = {
            let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
            match deps.get(cron_depl_name(instance).as_str()).await {
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

        // ── Resolve job statuses ─────────────────────────────────────────
        // Combine CR presence with K8s Job outcome into a single enum.
        let init_job = resolve_job_status(
            init_job_active,
            &jobs_api,
            active_init_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let restore_job = resolve_job_status(
            restore_job_active,
            &jobs_api,
            active_restore_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let upgrade_job = resolve_job_status(
            upgrade_job_active,
            &jobs_api,
            active_upgrade_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;
        let backup_job = resolve_job_status(
            backup_job_active,
            &jobs_api,
            active_backup_job
                .as_ref()
                .and_then(|j| j.status.as_ref())
                .and_then(|s| s.job_name.as_deref()),
        )
        .await;

        // ── Filestore PVC storage-class mismatch detection ────────────
        let (storage_class_mismatch, actual_storage_class) = {
            let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
            let pvc_name = format!("{instance_name}-filestore-pvc");
            match pvcs.get(&pvc_name).await {
                Ok(pvc) => {
                    let actual = pvc.spec.as_ref().and_then(|s| s.storage_class_name.clone());
                    let desired = instance
                        .spec
                        .filestore
                        .as_ref()
                        .and_then(|f| f.storage_class.clone());
                    let mismatch = match (&actual, &desired) {
                        (Some(a), Some(d)) => a != d,
                        _ => false,
                    };
                    (mismatch, actual)
                }
                Err(_) => (false, None),
            }
        };

        // ── Migration job status ────────────────────────────────────────
        let migration_job = {
            let job_name = instance
                .status
                .as_ref()
                .and_then(|s| s.migration_job_name.clone());
            match job_name {
                Some(ref name) => match jobs_api.get(name).await {
                    Ok(job) => {
                        let succeeded =
                            job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) > 0;
                        let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0;
                        if succeeded {
                            JobStatus::Succeeded
                        } else if failed {
                            JobStatus::Failed
                        } else {
                            JobStatus::Active
                        }
                    }
                    Err(kube::Error::Api(ref err)) if err.code == 404 => JobStatus::Absent,
                    Err(_) => JobStatus::Active,
                },
                None => JobStatus::Absent,
            }
        };

        Ok(Self {
            ready_replicas,
            deployment_replicas,
            cron_ready_replicas,
            cron_deployment_replicas,
            db_initialized: db_init_from_jobs,
            init_job,
            restore_job,
            upgrade_job,
            backup_job,
            active_init_job,
            active_restore_job,
            active_upgrade_job,
            active_backup_job,
            storage_class_mismatch,
            actual_storage_class,
            migration_job,
        })
    }
}

/// Combine CR presence with the K8s batch/v1 Job outcome.
///
/// If no CR is active, returns `Absent`.  Otherwise looks up the K8s Job:
/// - succeeded > 0 → `Succeeded`
/// - failed > 0, or Job has deletionTimestamp, or Job is 404 → `Failed`
/// - Job not yet created (no jobName) or transient API error → `Active`
async fn resolve_job_status(
    cr_active: bool,
    jobs_api: &Api<Job>,
    job_name: Option<&str>,
) -> JobStatus {
    if !cr_active {
        return JobStatus::Absent;
    }
    let Some(name) = job_name else {
        return JobStatus::Active;
    };
    match jobs_api.get(name).await {
        Ok(job) => {
            let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) > 0;
            let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0;
            if succeeded {
                JobStatus::Succeeded
            } else if failed {
                JobStatus::Failed
            } else if job.metadata.deletion_timestamp.is_some() {
                tracing::warn!(%name, "batch/v1 Job has deletionTimestamp — treating as failed");
                JobStatus::Failed
            } else {
                JobStatus::Active
            }
        }
        Err(kube::Error::Api(ref err)) if err.code == 404 => {
            tracing::warn!(%name, "batch/v1 Job not found — treating as failed");
            JobStatus::Failed
        }
        Err(_) => JobStatus::Active,
    }
}

// ── Transition actions ──────────────────────────────────────────────────────

/// One-shot actions that fire on specific edges (the "/" in UML state diagrams).
/// These handle CR status patching, events, and webhooks that belong to the
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
    BeginFilestoreMigration,
    CompleteFilestoreMigration,
    RollbackFilestoreMigration,
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
                // Restart the main deployment when an upgrade job completes
                // successfully. No need to restart on failure and no need to restart
                // the cron deployment since it gets scaled to 0 during upgrade anyway.
                if phase_enum == Phase::Completed {
                    let deps: Api<Deployment> = Api::namespaced(client.clone(), &ns);
                    deps.restart(instance.name_any().as_str()).await?;
                }
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
        BeginFilestoreMigration => {
            begin_filestore_migration(instance, ctx, snapshot).await?;
        }
        CompleteFilestoreMigration => {
            complete_filestore_migration(instance, ctx).await?;
        }
        RollbackFilestoreMigration => {
            rollback_filestore_migration(instance, ctx).await?;
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

use JobStatus::*;
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
        guard: |_, s| s.init_job.is_present(),
        guard_name: "init_job present",
        actions: &[],
    },
    // A restore can also bring us out of Uninitialized.
    Transition {
        from: Uninitialized,
        to: Restoring,
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
        actions: &[],
    },
    // ── Initializing ────────────────────────────────────────
    Transition {
        from: Initializing,
        to: Starting,
        guard: |_, s| s.init_job == Succeeded,
        guard_name: "init_job succeeded",
        actions: &[
            TransitionAction::CompleteInitJob,
            TransitionAction::MarkDbInitialized,
        ],
    },
    Transition {
        from: Initializing,
        to: InitFailed,
        guard: |_, s| s.init_job == Failed,
        guard_name: "init_job failed",
        actions: &[TransitionAction::FailInitJob],
    },
    // Orphaned: init job CR deleted while in Initializing.
    Transition {
        from: Initializing,
        to: Uninitialized,
        guard: |_, s| s.init_job == Absent,
        guard_name: "init_job absent",
        actions: &[],
    },
    // ── InitFailed ──────────────────────────────────────────
    // A new init job can retry.
    Transition {
        from: InitFailed,
        to: Initializing,
        guard: |_, s| s.init_job.is_present(),
        guard_name: "init_job present",
        actions: &[],
    },
    // A restore can also recover from InitFailed.
    Transition {
        from: InitFailed,
        to: Restoring,
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
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
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_ready(),
        guard_name: "upgrade_job ready",
        actions: &[],
    },
    Transition {
        from: Starting,
        to: BackingUp,
        guard: |_, s| s.backup_job.is_present(),
        guard_name: "backup_job present",
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
        to: MigratingFilestore,
        guard: |_, s| s.storage_class_mismatch,
        guard_name: "storage_class_mismatch",
        actions: &[TransitionAction::BeginFilestoreMigration],
    },
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
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
        actions: &[],
    },
    Transition {
        from: Running,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_ready(),
        guard_name: "upgrade_job ready",
        actions: &[],
    },
    Transition {
        from: Running,
        to: BackingUp,
        guard: |_, s| s.backup_job.is_present(),
        guard_name: "backup_job present",
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
        to: MigratingFilestore,
        guard: |_, s| s.storage_class_mismatch,
        guard_name: "storage_class_mismatch",
        actions: &[TransitionAction::BeginFilestoreMigration],
    },
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
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_ready(),
        guard_name: "upgrade_job ready",
        actions: &[],
    },
    Transition {
        from: Degraded,
        to: BackingUp,
        guard: |_, s| s.backup_job.is_present(),
        guard_name: "backup_job present",
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
        guard: |i, s| s.backup_job == Succeeded && i.spec.replicas == 0,
        guard_name: "backup succeeded && replicas == 0",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Stopped,
        guard: |i, s| s.backup_job == Failed && i.spec.replicas == 0,
        guard_name: "backup failed && replicas == 0",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Running,
        guard: |i, s| s.backup_job == Succeeded && s.ready_replicas >= i.spec.replicas,
        guard_name: "backup succeeded && ready >= desired",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Running,
        guard: |i, s| s.backup_job == Failed && s.ready_replicas >= i.spec.replicas,
        guard_name: "backup failed && ready >= desired",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Degraded,
        guard: |i, s| {
            s.backup_job == Succeeded && s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas
        },
        guard_name: "backup succeeded && 0 < ready < desired",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Degraded,
        guard: |i, s| {
            s.backup_job == Failed && s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas
        },
        guard_name: "backup failed && 0 < ready < desired",
        actions: &[TransitionAction::FailBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Starting,
        guard: |i, s| s.backup_job == Succeeded && s.ready_replicas == 0 && i.spec.replicas > 0,
        guard_name: "backup succeeded && ready == 0",
        actions: &[TransitionAction::CompleteBackupJob],
    },
    Transition {
        from: BackingUp,
        to: Starting,
        guard: |i, s| s.backup_job == Failed && s.ready_replicas == 0 && i.spec.replicas > 0,
        guard_name: "backup failed && ready == 0",
        actions: &[TransitionAction::FailBackupJob],
    },
    // Orphaned: backup job CR deleted while in BackingUp.
    Transition {
        from: BackingUp,
        to: Running,
        guard: |i, s| {
            s.backup_job == Absent && s.ready_replicas >= i.spec.replicas && i.spec.replicas > 0
        },
        guard_name: "backup absent && ready >= desired",
        actions: &[],
    },
    Transition {
        from: BackingUp,
        to: Degraded,
        guard: |i, s| {
            s.backup_job == Absent && s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas
        },
        guard_name: "backup absent && 0 < ready < desired",
        actions: &[],
    },
    Transition {
        from: BackingUp,
        to: Starting,
        guard: |i, s| s.backup_job == Absent && s.ready_replicas == 0 && i.spec.replicas > 0,
        guard_name: "backup absent && ready == 0",
        actions: &[],
    },
    Transition {
        from: BackingUp,
        to: Stopped,
        guard: |i, s| s.backup_job == Absent && i.spec.replicas == 0,
        guard_name: "backup absent && replicas == 0",
        actions: &[],
    },
    // ── Upgrading ───────────────────────────────────────────
    Transition {
        from: Upgrading,
        to: Starting,
        guard: |_, s| s.upgrade_job == Succeeded,
        guard_name: "upgrade_job succeeded",
        actions: &[TransitionAction::CompleteUpgradeJob],
    },
    Transition {
        from: Upgrading,
        to: Starting,
        guard: |_, s| s.upgrade_job == Failed,
        guard_name: "upgrade_job failed",
        actions: &[TransitionAction::FailUpgradeJob],
    },
    // Orphaned: upgrade job CR deleted while in Upgrading.
    Transition {
        from: Upgrading,
        to: Starting,
        guard: |_, s| s.upgrade_job == Absent,
        guard_name: "upgrade_job absent",
        actions: &[],
    },
    // ── Restoring ───────────────────────────────────────────
    Transition {
        from: Restoring,
        to: Starting,
        guard: |_, s| s.restore_job == Succeeded,
        guard_name: "restore_job succeeded",
        actions: &[
            TransitionAction::CompleteRestoreJob,
            TransitionAction::MarkDbInitialized,
        ],
    },
    Transition {
        from: Restoring,
        to: Starting,
        guard: |_, s| s.restore_job == Failed,
        guard_name: "restore_job failed",
        actions: &[TransitionAction::FailRestoreJob],
    },
    // Orphaned: restore job CR deleted while in Restoring.
    Transition {
        from: Restoring,
        to: Starting,
        guard: |_, s| s.restore_job == Absent,
        guard_name: "restore_job absent",
        actions: &[],
    },
    // ── Stopped ─────────────────────────────────────────────
    Transition {
        from: Stopped,
        to: MigratingFilestore,
        guard: |_, s| s.storage_class_mismatch,
        guard_name: "storage_class_mismatch",
        actions: &[TransitionAction::BeginFilestoreMigration],
    },
    Transition {
        from: Stopped,
        to: Restoring,
        guard: |_, s| s.restore_job.is_present(),
        guard_name: "restore_job present",
        actions: &[],
    },
    Transition {
        from: Stopped,
        to: Upgrading,
        guard: |_, s| s.upgrade_job_ready(),
        guard_name: "upgrade_job ready",
        actions: &[],
    },
    Transition {
        from: Stopped,
        to: Starting,
        guard: |i, _| i.spec.replicas > 0,
        guard_name: "replicas > 0",
        actions: &[],
    },
    // ── MigratingFilestore ───────────────────────────────────
    // Success: rsync job completed — rebind PVC and start up.
    Transition {
        from: MigratingFilestore,
        to: Starting,
        guard: |i, s| s.migration_job == Succeeded && i.spec.replicas > 0,
        guard_name: "migration_job succeeded && replicas > 0",
        actions: &[TransitionAction::CompleteFilestoreMigration],
    },
    Transition {
        from: MigratingFilestore,
        to: Stopped,
        guard: |i, s| s.migration_job == Succeeded && i.spec.replicas == 0,
        guard_name: "migration_job succeeded && replicas == 0",
        actions: &[TransitionAction::CompleteFilestoreMigration],
    },
    // Failure or lost job: rollback to previous StorageClass.
    Transition {
        from: MigratingFilestore,
        to: Starting,
        guard: |i, s| {
            (s.migration_job == Failed || s.migration_job == Absent) && i.spec.replicas > 0
        },
        guard_name: "migration_job failed/absent && replicas > 0",
        actions: &[TransitionAction::RollbackFilestoreMigration],
    },
    Transition {
        from: MigratingFilestore,
        to: Stopped,
        guard: |i, s| {
            (s.migration_job == Failed || s.migration_job == Absent) && i.spec.replicas == 0
        },
        guard_name: "migration_job failed/absent && replicas == 0",
        actions: &[TransitionAction::RollbackFilestoreMigration],
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
    Ok(requeue_for(&phase, snapshot))
}

/// Decide requeue strategy for phases that need periodic polling.
fn requeue_for(phase: &OdooInstancePhase, snapshot: &ReconcileSnapshot) -> Action {
    // If an upgrade job exists but its scheduled time hasn't arrived yet,
    // requeue so we wake up when it's due.
    if let Some(requeue) = scheduled_requeue(snapshot) {
        return requeue;
    }

    match phase {
        Starting | Initializing | Restoring | Upgrading | BackingUp | Degraded
        | MigratingFilestore => Action::requeue(Duration::from_secs(10)),
        _ => Action::await_change(),
    }
}

/// If an upgrade job CR is present but its `scheduledTime` is in the future,
/// return a requeue action that fires when the time arrives.
fn scheduled_requeue(snapshot: &ReconcileSnapshot) -> Option<Action> {
    if !snapshot.upgrade_job.is_present() {
        return None;
    }
    let scheduled = snapshot
        .active_upgrade_job
        .as_ref()
        .and_then(|j| j.spec.scheduled_time.as_deref())?;

    let target = chrono::DateTime::parse_from_rfc3339(scheduled).ok()?;
    let now = chrono::Utc::now();
    if target <= now {
        return None; // already due
    }
    let delay = (target.with_timezone(&chrono::Utc) - now)
        .to_std()
        .unwrap_or(Duration::from_secs(10));
    Some(Action::requeue(delay))
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

// ── Filestore migration actions ───────────────────────────────────────────

const MIGRATE_SCRIPT: &str = include_str!("../../scripts/migrate-filestore.sh");

/// Scale down deployments, create temp PVC, create rsync Job, store state.
async fn begin_filestore_migration(
    instance: &OdooInstance,
    ctx: &Context,
    snapshot: &ReconcileSnapshot,
) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    // Scale down both deployments.
    scale_deployment(client, &inst_name, &ns, 0).await?;
    scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

    // Determine storage sizes.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let orig_pvc_name = format!("{inst_name}-filestore-pvc");
    let storage_size = match pvcs.get(&orig_pvc_name).await {
        Ok(pvc) => pvc
            .spec
            .as_ref()
            .and_then(|s| s.resources.as_ref())
            .and_then(|r| r.requests.as_ref())
            .and_then(|r| r.get("storage"))
            .map(|q| q.0.clone())
            .unwrap_or_else(|| "2Gi".to_string()),
        Err(_) => "2Gi".to_string(),
    };

    let desired_class = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.clone())
        .unwrap_or_else(|| ctx.defaults.storage_class.clone());

    // Create temp PVC with new StorageClass.
    let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");
    let temp_pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(temp_pvc_name.clone()),
            namespace: Some(ns.clone()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
            storage_class_name: Some(desired_class),
            resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: Some(
                    [("storage".to_string(), Quantity(storage_size))]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    pvcs.create(&PostParams::default(), &temp_pvc).await?;
    info!(%inst_name, %temp_pvc_name, "created temp PVC for migration");

    // Create rsync Job.
    let job = build_rsync_job(&inst_name, &ns, instance);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let created = jobs.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();
    info!(%inst_name, %job_name, "created rsync migration job");

    // Store migration state.
    let prev_sc = snapshot
        .actual_storage_class
        .as_deref()
        .unwrap_or("unknown");
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    let patch = json!({
        "status": {
            "migrationJobName": &job_name,
            "migrationPreviousStorageClass": prev_sc,
            "message": format!("Migrating filestore from {prev_sc} to {}", instance.spec.filestore.as_ref().and_then(|f| f.storage_class.as_deref()).unwrap_or("?")),
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// Delete old PVC, rebind PV to new PVC with original name, clean up.
///
/// Idempotent: stores PV name in `status.migrationPvName` on first call
/// so retries don't depend on the temp PVC still existing.  Each step
/// checks current state before acting.
async fn complete_filestore_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);

    let orig_pvc_name = format!("{inst_name}-filestore-pvc");
    let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");

    // 1. Resolve PV name — from status (retry) or from temp PVC (first run).
    let pv_name = match instance
        .status
        .as_ref()
        .and_then(|s| s.migration_pv_name.clone())
    {
        Some(name) => name,
        None => {
            // First run: read from temp PVC and persist in status.
            let name = pvcs
                .get(&temp_pvc_name)
                .await?
                .spec
                .as_ref()
                .and_then(|s| s.volume_name.clone())
                .ok_or_else(|| {
                    crate::error::Error::Reconcile(
                        "temp PVC has no volumeName — not bound yet".into(),
                    )
                })?;
            let patch = json!({"status": {"migrationPvName": &name}});
            api.patch_status(
                &inst_name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await?;
            name
        }
    };

    // 2. Delete old PVC (idempotent — ignores 404).
    match pvcs.delete(&orig_pvc_name, &DeleteParams::default()).await {
        Ok(_) => info!(%inst_name, "deleted old filestore PVC"),
        Err(kube::Error::Api(ref e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }

    // 3. Wait for old PVC to be fully gone (not just Terminating).
    if pvcs.get(&orig_pvc_name).await.is_ok() {
        return Err(crate::error::Error::Reconcile(
            "old PVC still terminating — will retry".into(),
        ));
    }

    // 4. Patch PV: set Retain + clear claimRef so we can rebind.
    let pvs: Api<k8s_openapi::api::core::v1::PersistentVolume> = Api::all(client.clone());
    let pv_patch = json!({
        "spec": {
            "persistentVolumeReclaimPolicy": "Retain",
            "claimRef": null,
        }
    });
    pvs.patch(
        &pv_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&pv_patch),
    )
    .await?;

    // 5. Delete temp PVC (idempotent — ignores 404).
    match pvcs.delete(&temp_pvc_name, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(ref e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }

    // 6. Create final PVC with original name (idempotent — skip if exists).
    if pvcs.get(&orig_pvc_name).await.is_err() {
        let desired_class = instance
            .spec
            .filestore
            .as_ref()
            .and_then(|f| f.storage_class.clone())
            .unwrap_or_default();
        let storage_size = instance
            .spec
            .filestore
            .as_ref()
            .and_then(|f| f.storage_size.clone())
            .unwrap_or_else(|| "2Gi".to_string());
        let oref = super::helpers::controller_owner_ref(instance);

        let final_pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(orig_pvc_name.clone()),
                namespace: Some(ns.clone()),
                owner_references: Some(vec![oref]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteMany".to_string()]),
                storage_class_name: Some(desired_class),
                volume_name: Some(pv_name.clone()),
                resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                    requests: Some(
                        [("storage".to_string(), Quantity(storage_size))]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        pvcs.create(&PostParams::default(), &final_pvc).await?;
        info!(%inst_name, %pv_name, "created final PVC bound to migrated PV");
    }

    // 7. Clean up rsync job (idempotent).
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    if let Some(ref job_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_job_name.clone())
    {
        let _ = jobs.delete(job_name, &DeleteParams::background()).await;
    }

    // 8. Clear migration status.
    let patch = json!({
        "status": {
            "migrationJobName": null,
            "migrationPvName": null,
            "migrationPreviousStorageClass": null,
            "message": null,
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// Rollback: delete temp PVC, delete job, revert spec to previous SC, clear status.
async fn rollback_filestore_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    let prev_sc = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_previous_storage_class.clone())
        .unwrap_or_else(|| "unknown".to_string());

    warn!(%inst_name, %prev_sc, "rolling back filestore migration");

    // Delete temp PVC.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
    let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");
    let _ = pvcs.delete(&temp_pvc_name, &DeleteParams::default()).await;

    // Delete migration job.
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    if let Some(ref job_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_job_name.clone())
    {
        let _ = jobs.delete(job_name, &DeleteParams::background()).await;
    }

    // Revert spec.filestore.storageClass to previous value.
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    if prev_sc != "unknown" {
        let spec_patch = json!({"spec": {"filestore": {"storageClass": &prev_sc}}});
        api.patch(
            &inst_name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&spec_patch),
        )
        .await?;
    }

    // Clear migration status.
    let status_patch = json!({
        "status": {
            "migrationJobName": null,
            "migrationPvName": null,
            "migrationPreviousStorageClass": null,
            "message": format!("Filestore migration rolled back to {prev_sc}"),
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&status_patch),
    )
    .await?;
    Ok(())
}

/// Build the rsync batch/v1 Job that copies data between PVCs.
fn build_rsync_job(inst_name: &str, ns: &str, instance: &OdooInstance) -> Job {
    let old_pvc = format!("{inst_name}-filestore-pvc");
    let temp_pvc = format!("{inst_name}-filestore-pvc-temp");

    Job {
        metadata: ObjectMeta {
            generate_name: Some(format!("{inst_name}-migrate-")),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(3),
            active_deadline_seconds: Some(3600),
            ttl_seconds_after_finished: Some(300),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(
                        [("app".to_string(), inst_name.to_string())]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    security_context: Some(super::helpers::odoo_security_context()),
                    image_pull_secrets: super::helpers::image_pull_secrets(instance),
                    containers: vec![Container {
                        name: "rsync".to_string(),
                        image: Some("instrumentisto/rsync-ssh:latest".to_string()),
                        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
                        args: Some(vec![MIGRATE_SCRIPT.to_string()]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "old-filestore".to_string(),
                                mount_path: "/mnt/old".to_string(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "new-filestore".to_string(),
                                mount_path: "/mnt/new".to_string(),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "old-filestore".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: old_pvc,
                                read_only: Some(true),
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "new-filestore".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: temp_pvc,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
