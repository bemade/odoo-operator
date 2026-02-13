//! Declarative state machine for OdooInstance lifecycle phases.
//!
//! Each phase has an `on_enter()` method on [`OdooInstancePhase`] that fires
//! once when the instance transitions into that state — one-shot outputs.
//!
//! Transitions are a static table of `(from, to, guard, action)`.  Guards are
//! pure functions over `(&OdooInstance, &ReconcileSnapshot)`.  The reconciler
//! evaluates guards each tick; when one fires, it patches the phase and calls
//! `new_phase.on_enter()` to flip the outputs.

use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
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
/// Guards are pure functions over the summary fields (phases, booleans).
/// Handlers use the full CRD objects to read specs and patch statuses.
pub struct ReconcileSnapshot {
    // Summary fields — used by guards (pure, sync).
    pub ready_replicas: i32,
    pub db_initialized: bool,
    pub init_job_phase: Option<Phase>,
    pub restore_job_phase: Option<Phase>,
    pub upgrade_job_phase: Option<Phase>,
    pub backup_job_active: bool,
    pub restore_job_active: bool,
    pub upgrade_job_active: bool,

    // Active job CRD objects — used by handlers to read specs and patch status.
    pub active_init_job: Option<OdooInitJob>,
    pub active_restore_job: Option<OdooRestoreJob>,
    pub active_upgrade_job: Option<OdooUpgradeJob>,
    pub active_backup_job: Option<OdooBackupJob>,
}

impl ReconcileSnapshot {
    /// Gather the snapshot from the cluster.  All List/Get calls happen here,
    /// so the rest of the reconcile loop is synchronous guard evaluation.
    pub async fn gather(client: &Client, ns: &str, instance_name: &str, instance: &OdooInstance) -> Result<Self> {
        let db_initialized = instance
            .status
            .as_ref()
            .map(|s| s.db_initialized)
            .unwrap_or(false);

        // Deployment ready replicas.
        let ready_replicas = {
            let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
            match deps.get(instance_name).await {
                Ok(dep) => dep.status.and_then(|s| s.ready_replicas).unwrap_or(0),
                Err(_) => 0,
            }
        };

        // Init jobs.
        let mut init_job_phase: Option<Phase> = None;
        let mut active_init_job: Option<OdooInitJob> = None;
        let mut db_init_from_jobs = db_initialized;
        {
            let inits: Api<OdooInitJob> = Api::namespaced(client.clone(), ns);
            for job in inits.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                if let Some(ref status) = job.status {
                    match status.phase.as_ref() {
                        Some(Phase::Running) | Some(Phase::Pending) => {
                            init_job_phase = Some(Phase::Running);
                            active_init_job = Some(job);
                            break; // Running takes priority
                        }
                        Some(Phase::Completed) => {
                            db_init_from_jobs = true;
                            init_job_phase = Some(Phase::Completed);
                        }
                        Some(Phase::Failed) if init_job_phase.is_none() => {
                            init_job_phase = Some(Phase::Failed);
                        }
                        _ => {
                            // Pending job with no phase yet — this is the active one.
                            if active_init_job.is_none() {
                                active_init_job = Some(job);
                            }
                        }
                    }
                } else {
                    // No status at all — brand new CRD, this is the active one.
                    if active_init_job.is_none() {
                        active_init_job = Some(job);
                    }
                }
            }
        }

        // Restore jobs.
        let mut restore_job_phase: Option<Phase> = None;
        let mut restore_job_active = false;
        let mut active_restore_job: Option<OdooRestoreJob> = None;
        {
            let restores: Api<OdooRestoreJob> = Api::namespaced(client.clone(), ns);
            for job in restores.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                if let Some(ref status) = job.status {
                    match status.phase.as_ref() {
                        Some(Phase::Running) | Some(Phase::Pending) => {
                            restore_job_active = true;
                            restore_job_phase = Some(Phase::Running);
                            active_restore_job = Some(job);
                        }
                        Some(Phase::Completed) => {
                            db_init_from_jobs = true;
                            if restore_job_phase.is_none() {
                                restore_job_phase = Some(Phase::Completed);
                            }
                        }
                        Some(Phase::Failed) if restore_job_phase.is_none() => {
                            restore_job_phase = Some(Phase::Failed);
                        }
                        _ => {
                            if active_restore_job.is_none() {
                                active_restore_job = Some(job);
                            }
                        }
                    }
                } else if active_restore_job.is_none() {
                    active_restore_job = Some(job);
                }
            }
        }

        // Upgrade jobs.
        let mut upgrade_job_phase: Option<Phase> = None;
        let mut upgrade_job_active = false;
        let mut active_upgrade_job: Option<OdooUpgradeJob> = None;
        {
            let upgrades: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), ns);
            for job in upgrades.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                if let Some(ref status) = job.status {
                    match status.phase.as_ref() {
                        Some(Phase::Running) | Some(Phase::Pending) => {
                            upgrade_job_active = true;
                            upgrade_job_phase = Some(Phase::Running);
                            active_upgrade_job = Some(job);
                        }
                        Some(Phase::Completed) if upgrade_job_phase.is_none() => {
                            upgrade_job_phase = Some(Phase::Completed);
                        }
                        Some(Phase::Failed) if upgrade_job_phase.is_none() => {
                            upgrade_job_phase = Some(Phase::Failed);
                        }
                        _ => {
                            if active_upgrade_job.is_none() {
                                active_upgrade_job = Some(job);
                            }
                        }
                    }
                } else if active_upgrade_job.is_none() {
                    active_upgrade_job = Some(job);
                }
            }
        }

        // Backup jobs.
        let mut backup_job_active = false;
        let mut active_backup_job: Option<OdooBackupJob> = None;
        {
            let backups: Api<OdooBackupJob> = Api::namespaced(client.clone(), ns);
            for job in backups.list(&ListParams::default()).await?.items {
                if job.spec.odoo_instance_ref.name != instance_name {
                    continue;
                }
                if let Some(ref status) = job.status {
                    match status.phase.as_ref() {
                        Some(Phase::Running) | Some(Phase::Pending) => {
                            backup_job_active = true;
                            active_backup_job = Some(job);
                        }
                        _ => {
                            if active_backup_job.is_none() {
                                active_backup_job = Some(job);
                            }
                        }
                    }
                } else if active_backup_job.is_none() {
                    active_backup_job = Some(job);
                }
            }
        }

        Ok(Self {
            ready_replicas,
            db_initialized: db_init_from_jobs,
            init_job_phase,
            restore_job_phase,
            upgrade_job_phase,
            backup_job_active,
            restore_job_active,
            upgrade_job_active,
            active_init_job,
            active_restore_job,
            active_upgrade_job,
            active_backup_job,
        })
    }
}


// ── Transition actions ──────────────────────────────────────────────────────

/// One-shot actions that fire on specific edges (the "/" in UML state diagrams).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionAction {
    MarkDbInitialized,
}

async fn execute_action(
    action: TransitionAction,
    instance: &OdooInstance,
    ctx: &Context,
) -> Result<()> {
    match action {
        TransitionAction::MarkDbInitialized => {
            let ns = instance.namespace().unwrap_or_default();
            let name = instance.name_any();
            let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);
            let patch = json!({"status": {"dbInitialized": true}});
            api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
                .await?;
            Ok(())
        }
    }
}

// ── Transition table ────────────────────────────────────────────────────────

/// A single row in the transition table.
pub struct Transition {
    pub from: OdooInstancePhase,
    pub to: OdooInstancePhase,
    pub guard: fn(&OdooInstance, &ReconcileSnapshot) -> bool,
    pub action: Option<TransitionAction>,
}

use OdooInstancePhase::*;

/// The complete lifecycle transition table.  First matching guard wins.
/// Order within a `from` group matters — more specific/urgent transitions first.
pub static TRANSITIONS: &[Transition] = &[
    // ── Provisioning ────────────────────────────────────────
    // Provisioning is the initial phase.  We transition out once the
    // instance controller has ensured all child resources.  For now the
    // ensure_* calls run before the state machine, so we always transition.
    Transition { from: Provisioning, to: Uninitialized,
        guard: |_, s| !s.db_initialized,
        action: None },
    Transition { from: Provisioning, to: Starting,
        guard: |_, s| s.db_initialized,
        action: None },

    // ── Uninitialized ───────────────────────────────────────
    Transition { from: Uninitialized, to: Initializing,
        guard: |_, s| matches!(s.init_job_phase, Some(Phase::Running)),
        action: None },
    // A restore can also bring us out of Uninitialized.
    Transition { from: Uninitialized, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },

    // ── Initializing ────────────────────────────────────────
    Transition { from: Initializing, to: Starting,
        guard: |_, s| matches!(s.init_job_phase, Some(Phase::Completed)),
        action: Some(TransitionAction::MarkDbInitialized) },
    Transition { from: Initializing, to: InitFailed,
        guard: |_, s| matches!(s.init_job_phase, Some(Phase::Failed)),
        action: None },

    // ── InitFailed ──────────────────────────────────────────
    // A new init job can retry.
    Transition { from: InitFailed, to: Initializing,
        guard: |_, s| matches!(s.init_job_phase, Some(Phase::Running)),
        action: None },
    // A restore can also recover from InitFailed.
    Transition { from: InitFailed, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },

    // ── Starting ────────────────────────────────────────────
    Transition { from: Starting, to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        action: None },
    Transition { from: Starting, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },
    Transition { from: Starting, to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        action: None },
    Transition { from: Starting, to: BackingUp,
        guard: |_, s| s.backup_job_active,
        action: None },
    Transition { from: Starting, to: Running,
        guard: |i, s| s.ready_replicas >= i.spec.replicas && i.spec.replicas > 0,
        action: None },

    // ── Running ─────────────────────────────────────────────
    Transition { from: Running, to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        action: None },
    Transition { from: Running, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },
    Transition { from: Running, to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        action: None },
    Transition { from: Running, to: BackingUp,
        guard: |_, s| s.backup_job_active,
        action: None },
    Transition { from: Running, to: Degraded,
        guard: |i, s| s.ready_replicas < i.spec.replicas && s.ready_replicas > 0,
        action: None },
    Transition { from: Running, to: Starting,
        guard: |i, s| s.ready_replicas < i.spec.replicas && s.ready_replicas == 0,
        action: None },

    // ── Degraded ────────────────────────────────────────────
    Transition { from: Degraded, to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        action: None },
    Transition { from: Degraded, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },
    Transition { from: Degraded, to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        action: None },
    Transition { from: Degraded, to: BackingUp,
        guard: |_, s| s.backup_job_active,
        action: None },
    Transition { from: Degraded, to: Running,
        guard: |i, s| s.ready_replicas >= i.spec.replicas,
        action: None },
    Transition { from: Degraded, to: Starting,
        guard: |_, s| s.ready_replicas == 0,
        action: None },

    // ── BackingUp ───────────────────────────────────────────
    Transition { from: BackingUp, to: Stopped,
        guard: |i, _| i.spec.replicas == 0,
        action: None },
    Transition { from: BackingUp, to: Running,
        guard: |i, s| !s.backup_job_active && s.ready_replicas >= i.spec.replicas,
        action: None },
    Transition { from: BackingUp, to: Degraded,
        guard: |i, s| s.ready_replicas > 0 && s.ready_replicas < i.spec.replicas,
        action: None },
    Transition { from: BackingUp, to: Starting,
        guard: |i, s| s.ready_replicas == 0 && i.spec.replicas > 0,
        action: None },

    // ── Upgrading ───────────────────────────────────────────
    Transition { from: Upgrading, to: Starting,
        guard: |_, s| matches!(s.upgrade_job_phase, Some(Phase::Completed) | Some(Phase::Failed)),
        action: None },

    // ── Restoring ───────────────────────────────────────────
    Transition { from: Restoring, to: Starting,
        guard: |_, s| matches!(s.restore_job_phase, Some(Phase::Completed)),
        action: Some(TransitionAction::MarkDbInitialized) },
    Transition { from: Restoring, to: Starting,
        guard: |_, s| matches!(s.restore_job_phase, Some(Phase::Failed)),
        action: None },

    // ── Stopped ─────────────────────────────────────────────
    Transition { from: Stopped, to: Restoring,
        guard: |_, s| s.restore_job_active,
        action: None },
    Transition { from: Stopped, to: Upgrading,
        guard: |_, s| s.upgrade_job_active,
        action: None },
    Transition { from: Stopped, to: Starting,
        guard: |i, _| i.spec.replicas > 0,
        action: None },

    // ── Error ───────────────────────────────────────────────
    Transition { from: Error, to: Starting,
        guard: |_, s| s.db_initialized,
        action: None },
    Transition { from: Error, to: Uninitialized,
        guard: |_, s| !s.db_initialized,
        action: None },
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

    // Evaluate transitions — first matching guard wins.
    for t in TRANSITIONS.iter().filter(|t| t.from == phase) {
        if (t.guard)(instance, snapshot) {
            info!(
                name = %instance.name_any(),
                from = %phase,
                to = %t.to,
                "phase transition"
            );

            // Fire edge action (UML "/").
            if let Some(action) = t.action {
                execute_action(action, instance, ctx).await?;
            }

            // Patch the phase.
            let ns = instance.namespace().unwrap_or_default();
            let name = instance.name_any();
            let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);
            let patch = json!({"status": {"phase": format!("{}", t.to)}});
            api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
                .await?;

            // Call on_enter() on the new state — one-shot outputs.
            super::states::state_for(&t.to).on_enter(instance, ctx, snapshot).await?;

            // Requeue to re-evaluate guards in the new state.
            return Ok(Action::requeue(Duration::ZERO));
        }
    }

    // No transition — stay in current state, poll guards periodically.
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
        .patch(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
        .await?;
    Ok(())
}
