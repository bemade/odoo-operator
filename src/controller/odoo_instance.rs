//! OdooInstance controller — the main reconciler.
//!
//! Orchestrates the reconcile loop: applies defaults, ensures child resources
//! (via `child_resources`), gathers the snapshot, runs the state machine,
//! manages the postgres-cleanup finalizer, and fires webhooks on phase
//! transitions.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, ObjectReference, PersistentVolumeClaim, Secret, Service},
    networking::v1::Ingress,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{
    api::{Api, Patch, PatchParams, ResourceExt},
    runtime::{
        controller::{Action, Controller},
        events::{Event as KubeEvent, EventType, Recorder, Reporter},
        finalizer::{finalizer, Event as FinalizerEvent},
        watcher::Config as WatcherConfig,
    },
    Client, Resource,
};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use crate::crd::odoo_restore_job::OdooRestoreJob;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::error::{Error, Result};
use crate::helpers::{odoo_username, OperatorDefaults};
use crate::postgres::{PostgresClusterConfig, PostgresManager};

use super::child_resources;
use super::helpers::{controller_owner_ref, FIELD_MANAGER};

const FINALIZER: &str = "bemade.org/postgres-cleanup";
const KOPF_FINALIZER: &str = "kopf.zalando.org/KopfFinalizerMarker";

/// Build an ObjectReference from any kube Resource.
pub fn kube_object_ref<K: Resource<DynamicType = ()>>(obj: &K) -> ObjectReference {
    ObjectReference {
        api_version: Some(K::api_version(&()).to_string()),
        kind: Some(K::kind(&()).to_string()),
        name: Some(obj.name_any()),
        namespace: obj.namespace(),
        uid: obj.meta().uid.clone(),
        resource_version: obj.meta().resource_version.clone(),
        ..Default::default()
    }
}

/// Publish a Kubernetes event attached to the given resource.
/// Errors are logged but never block reconciliation.
pub async fn publish_event<K: Resource<DynamicType = ()>>(
    ctx: &Context,
    obj: &K,
    type_: EventType,
    reason: &str,
    action: &str,
    note: Option<String>,
) {
    let rec = Recorder::new(ctx.client.clone(), ctx.reporter.clone());
    let oref = kube_object_ref(obj);
    if let Err(e) = rec
        .publish(
            &KubeEvent {
                type_,
                reason: reason.to_string(),
                note,
                action: action.to_string(),
                secondary: None,
            },
            &oref,
        )
        .await
    {
        warn!(%e, "failed to publish event");
    }
}

// ── Shared context passed to every reconcile call ─────────────────────────────

pub struct Context {
    pub client: Client,
    pub defaults: OperatorDefaults,
    pub operator_namespace: String,
    pub postgres_clusters_secret: String,
    pub postgres: Arc<dyn PostgresManager>,
    pub http_client: reqwest::Client,
    pub reporter: Reporter,
}

// ── Controller entry point ────────────────────────────────────────────────────

/// Start the OdooInstance controller. Returns a future that runs forever.
pub async fn run(ctx: Arc<Context>) {
    let client = ctx.client.clone();
    let instances: Api<OdooInstance> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::all(client.clone());
    let services: Api<Service> = Api::all(client.clone());
    let ingresses: Api<Ingress> = Api::all(client.clone());
    let configmaps: Api<ConfigMap> = Api::all(client.clone());
    let secrets: Api<Secret> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let init_jobs: Api<OdooInitJob> = Api::all(client.clone());
    let upgrade_jobs: Api<OdooUpgradeJob> = Api::all(client.clone());
    let restore_jobs: Api<OdooRestoreJob> = Api::all(client.clone());
    let backup_jobs: Api<OdooBackupJob> = Api::all(client.clone());

    Controller::new(instances, WatcherConfig::default())
        .owns(deployments, WatcherConfig::default())
        .owns(services, WatcherConfig::default())
        .owns(ingresses, WatcherConfig::default())
        .owns(configmaps, WatcherConfig::default())
        .owns(secrets, WatcherConfig::default())
        .owns(pvcs, WatcherConfig::default())
        // Watch job CRDs and map back to the owning OdooInstance.
        .watches(
            init_jobs,
            WatcherConfig::default(),
            map_init_job_to_instance,
        )
        .watches(
            upgrade_jobs,
            WatcherConfig::default(),
            map_upgrade_job_to_instance,
        )
        .watches(
            restore_jobs,
            WatcherConfig::default(),
            map_restore_job_to_instance,
        )
        .watches(
            backup_jobs,
            WatcherConfig::default(),
            map_backup_job_to_instance,
        )
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((_obj, _action)) => {}
                Err(e) => {
                    let msg = format!("{e:?}");
                    if msg.contains("ObjectNotFound") {
                        debug!("reconcile: object already deleted");
                    } else {
                        warn!("reconcile failed: {msg}");
                    }
                }
            }
        })
        .await;
}

// ── Watch mappers (job → instance) ────────────────────────────────────────────

fn map_init_job_to_instance(
    job: OdooInitJob,
) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job
        .spec
        .odoo_instance_ref
        .namespace
        .clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

fn map_upgrade_job_to_instance(
    job: OdooUpgradeJob,
) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job
        .spec
        .odoo_instance_ref
        .namespace
        .clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

fn map_restore_job_to_instance(
    job: OdooRestoreJob,
) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job
        .spec
        .odoo_instance_ref
        .namespace
        .clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

fn map_backup_job_to_instance(
    job: OdooBackupJob,
) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job
        .spec
        .odoo_instance_ref
        .namespace
        .clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

// ── Reconcile ─────────────────────────────────────────────────────────────────

async fn reconcile(instance: Arc<OdooInstance>, ctx: Arc<Context>) -> Result<Action> {
    let ns = instance.namespace().unwrap_or_default();
    let _name = instance.name_any();
    let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);

    // Migration: strip the old Kopf finalizer so deletion isn't blocked.
    if instance
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|s| s == KOPF_FINALIZER))
    {
        info!(%_name, "removing stale Kopf finalizer");
        let patch = json!({
            "metadata": {
                "finalizers": instance
                    .metadata
                    .finalizers
                    .as_ref()
                    .unwrap()
                    .iter()
                    .filter(|s| s.as_str() != KOPF_FINALIZER)
                    .collect::<Vec<_>>()
            }
        });
        api.patch(&_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(Error::Kube)?;
        return Ok(Action::requeue(Duration::from_secs(0)));
    }

    // Use kube-rs finalizer helper — handles add/remove/apply lifecycle.
    finalizer(&api, FINALIZER, instance, |event| async {
        match event {
            FinalizerEvent::Apply(instance) => reconcile_instance(&instance, &ctx).await,
            FinalizerEvent::Cleanup(instance) => cleanup_instance(&instance, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

fn error_policy(instance: Arc<OdooInstance>, error: &Error, _ctx: Arc<Context>) -> Action {
    let name = instance.name_any();
    // Finalizer helper returns ObjectNotFound when the object was deleted between
    // the watch event and the reconcile — harmless, no need to requeue.
    if matches!(error, Error::Finalizer(e) if e.to_string().contains("ObjectNotFound")) {
        debug!(%name, "object already deleted, skipping requeue");
        return Action::await_change();
    }
    warn!(%name, %error, "reconcile error, requeuing in 30s");
    Action::requeue(Duration::from_secs(30))
}

// ── Core reconcile logic ──────────────────────────────────────────────────────

async fn reconcile_instance(instance: &OdooInstance, ctx: &Context) -> Result<Action> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let client = &ctx.client;

    debug!(%name, %ns, "reconciling OdooInstance");

    // Write operator-level defaults into any unset spec fields on the first
    // reconcile, then re-fetch so downstream logic works with the persisted copy.
    if child_resources::apply_defaults(client, &ns, &name, instance, ctx).await? {
        info!(%name, "spec defaults applied from operator configuration");
        publish_event(
            ctx,
            instance,
            EventType::Normal,
            "DefaultsApplied",
            "Reconcile",
            Some("Operator defaults applied to spec".to_string()),
        )
        .await;
        // Re-fetch is handled by the requeue — the finalizer helper will
        // re-enter reconcile_instance with the updated spec.
        return Ok(Action::requeue(Duration::from_secs(0)));
    }

    // Load postgres cluster config.
    let (_cluster_name, pg_cluster) = load_postgres_cluster(ctx, instance).await?;

    // Ensure all child resources (phase-independent infrastructure).
    let oref = controller_owner_ref(instance);
    child_resources::ensure_image_pull_secret(client, &ns, instance, &ctx.operator_namespace)
        .await?;
    child_resources::ensure_odoo_user_secret(client, &ns, &name, &oref).await?;
    child_resources::ensure_postgres_role(ctx, instance, &pg_cluster).await?;
    child_resources::ensure_filestore_pvc(client, &ns, &name, instance, ctx, &oref).await?;
    child_resources::ensure_config_map(client, &ns, &name, instance, &pg_cluster, &oref).await?;
    child_resources::ensure_service(client, &ns, &name, &oref).await?;
    child_resources::ensure_ingress(client, &ns, &name, instance, &oref).await?;
    child_resources::ensure_deployment(client, &ns, &name, instance, ctx, &oref).await?;

    // Gather the observed world into a snapshot.
    let snapshot =
        super::state_machine::ReconcileSnapshot::gather(client, &ns, &name, instance).await?;

    // Patch non-phase status fields (readyReplicas, url, conditions, etc.)
    // only if something actually changed — avoids spurious etcd writes and
    // watch-event hot loops from Merge patches.
    let url = instance
        .spec
        .ingress
        .hosts
        .first()
        .map(|h| format!("https://{h}"));
    let ready = snapshot.ready_replicas == instance.spec.replicas && instance.spec.replicas > 0;
    let current_phase = instance
        .status
        .as_ref()
        .and_then(|s| s.phase.clone())
        .unwrap_or(OdooInstancePhase::Provisioning);

    let cur = instance.status.as_ref();
    let status_changed = !cur.is_some_and(|s| {
        s.ready_replicas == snapshot.ready_replicas
            && s.ready == ready
            && s.url == url
            && s.target_replicas == Some(instance.spec.replicas)
            && s.db_initialized == snapshot.db_initialized
    });

    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    if status_changed {
        let conditions =
            phase_to_conditions(&current_phase, instance.metadata.generation.unwrap_or(0));
        let status_patch = json!({
            "status": {
                "readyReplicas": snapshot.ready_replicas,
                "ready": current_phase == OdooInstancePhase::Running,
                "url": url,
                "targetReplicas": instance.spec.replicas,
                "dbInitialized": snapshot.db_initialized,
                "conditions": conditions,
            }
        });
        api.patch_status(
            &name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&status_patch),
        )
        .await?;
    }

    // Run the state machine: ensure phase outputs, evaluate transitions.
    let previous_phase = instance.status.as_ref().and_then(|s| s.phase.clone());
    let action = super::state_machine::run_state_machine(instance, ctx, &snapshot).await?;

    // Re-read phase after state machine may have patched it.
    let new_phase = api.get_status(&name).await?.status.and_then(|s| s.phase);

    // Fire event + webhook on phase transition.
    if new_phase != previous_phase {
        let phase_display = new_phase
            .as_ref()
            .map(|p| format!("{p}"))
            .unwrap_or_default();
        let prev_str = previous_phase
            .as_ref()
            .map(|p| format!("{p}"))
            .unwrap_or_default();
        info!(%name, from = %prev_str, to = %phase_display, "phase changed");
        publish_event(
            ctx,
            instance,
            EventType::Normal,
            "PhaseChanged",
            "Reconcile",
            Some(format!("Phase changed from {prev_str} to {phase_display}")),
        )
        .await;
        if let Some(ref wh) = instance.spec.webhook {
            let payload = json!({
                "name": name,
                "namespace": ns,
                "phase": phase_display,
                "previousPhase": prev_str,
                "url": url,
                "timestamp": crate::helpers::utc_now_odoo(),
            });
            let http = ctx.http_client.clone();
            let wh_url = wh.url.clone();
            tokio::spawn(async move {
                if let Err(e) = http
                    .post(&wh_url)
                    .json(&payload)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
                {
                    warn!(%wh_url, %e, "webhook POST failed");
                }
            });
        }
    }

    Ok(action)
}

// ── Cleanup (finalizer) ──────────────────────────────────────────────────────

async fn cleanup_instance(instance: &OdooInstance, ctx: &Context) -> Result<Action> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    info!(%name, %ns, "cleaning up OdooInstance (deleting postgres role)");

    publish_event(
        ctx,
        instance,
        EventType::Normal,
        "Cleanup",
        "Finalize",
        Some("Deleting postgres role".to_string()),
    )
    .await;

    if let Ok((_cluster_name, pg_cluster)) = load_postgres_cluster(ctx, instance).await {
        let username = odoo_username(&ns, &name);
        if let Err(e) = ctx.postgres.delete_role(&pg_cluster, &username).await {
            warn!(%name, %e, "failed to delete postgres role — removing finalizer anyway");
            publish_event(
                ctx,
                instance,
                EventType::Warning,
                "CleanupFailed",
                "Finalize",
                Some(format!("Failed to delete postgres role: {e}")),
            )
            .await;
        }
    }

    Ok(Action::await_change())
}

/// Maps an OdooInstancePhase to a condition that will be well interpreted by UIs such as
/// Rancher and Lens
pub fn phase_to_conditions(phase: &OdooInstancePhase, generation: i64) -> Vec<Condition> {
    use OdooInstancePhase::*;

    let (ready_status, message) = match phase {
        Running => ("True", "Instance is running"),
        Degraded => ("False", "Ready replicas below desired count"),
        Stopped => ("False", "Instance is stopped (replicas=0)"),
        Provisioning => ("False", "Creating child resources"),
        Uninitialized => ("False", "Waiting for database initialization"),
        Initializing => ("False", "Database initialization in progress"),
        InitFailed => ("False", "Database initialization failed"),
        Starting => ("False", "Waiting for pods to become ready"),
        Upgrading => ("False", "Module upgrade in progress"),
        Restoring => ("False", "Database restore in progress"),
        BackingUp => ("False", "Backup in progress"),
        Error => ("False", "Reconciliation error"),
    };

    let progressing = matches!(
        phase,
        Provisioning | Initializing | Starting | Upgrading | Restoring | BackingUp
    );

    let now = Time(chrono::Utc::now());
    let reason = format!("{phase}");

    let mut conditions = vec![Condition {
        type_: "Ready".to_string(),
        status: ready_status.to_string(),
        reason: reason.clone(),
        message: message.to_string(),
        observed_generation: Some(generation),
        last_transition_time: now.clone(),
    }];

    conditions.push(Condition {
        type_: "Progressing".to_string(),
        status: if progressing { "True" } else { "False" }.to_string(),
        reason: reason.clone(),
        message: message.to_string(),
        observed_generation: Some(generation),
        last_transition_time: now,
    });

    conditions
}

// ── Postgres cluster config loading ───────────────────────────────────────────

async fn load_postgres_cluster(
    ctx: &Context,
    instance: &OdooInstance,
) -> Result<(String, PostgresClusterConfig)> {
    let secret_name = if ctx.postgres_clusters_secret.is_empty() {
        "postgres-clusters"
    } else {
        &ctx.postgres_clusters_secret
    };

    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.operator_namespace);
    let secret = secrets
        .get(secret_name)
        .await
        .map_err(|e| Error::config(format!("reading {secret_name} secret: {e}")))?;

    let data = secret.data.unwrap_or_default();
    let raw = data
        .get("clusters.yaml")
        .ok_or_else(|| Error::config("postgres-clusters secret missing clusters.yaml key"))?;
    let yaml_str = String::from_utf8_lossy(&raw.0);
    let clusters: BTreeMap<String, PostgresClusterConfig> = serde_yaml::from_str(&yaml_str)?;

    // If spec.database.cluster is set, use it directly.
    if let Some(ref db) = instance.spec.database {
        if let Some(ref cluster_name) = db.cluster {
            if !cluster_name.is_empty() {
                let cfg = clusters.get(cluster_name).ok_or_else(|| {
                    Error::config(format!("postgres cluster {cluster_name:?} not found"))
                })?;
                return Ok((cluster_name.clone(), cfg.clone()));
            }
        }
    }

    // Otherwise find the default.
    for (name, cfg) in &clusters {
        if cfg.default {
            return Ok((name.clone(), cfg.clone()));
        }
    }

    Err(Error::config(format!(
        "no default postgres cluster configured in {secret_name} secret"
    )))
}
