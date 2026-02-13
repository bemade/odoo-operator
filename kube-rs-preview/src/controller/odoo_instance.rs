//! OdooInstance controller — the main reconciler.
//!
//! Mirrors the Go OdooInstanceReconciler: ensures child resources (Secret,
//! PVC, ConfigMap, Service, Ingress, Deployment), derives phase from observed
//! state, manages the postgres-cleanup finalizer, and fires webhooks on
//! phase transitions.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::{
    apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
    core::v1::{
        ConfigMap, Container, ContainerPort, HTTPGetAction,
        PersistentVolumeClaim, PersistentVolumeClaimSpec,
        PodSpec, PodTemplateSpec, Probe, VolumeResourceRequirements,
        Secret, Service, ServicePort, ServiceSpec,
    },
    networking::v1::{
        HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend,
        IngressRule, IngressServiceBackend, IngressSpec as K8sIngressSpec,
        IngressTLS, ServiceBackendPort,
    },
};
use k8s_openapi::apimachinery::pkg::{
    api::resource::Quantity,
    apis::meta::v1::{Condition, LabelSelector, OwnerReference, Time},
    util::intstr::IntOrString,
};
use k8s_openapi::api::core::v1::ObjectReference;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams, PostParams, ResourceExt},
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

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::{
    DeploymentStrategyType, OdooInstance, OdooInstancePhase,
};
use crate::crd::odoo_restore_job::OdooRestoreJob;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::error::{Error, Result};
use crate::helpers::{
    build_odoo_conf, db_name, generate_password, odoo_username, sha256_hex, OperatorDefaults,
};
use crate::postgres::{PostgresClusterConfig, PostgresManager};

use super::helpers::{
    controller_owner_ref, image_pull_secrets, odoo_security_context, odoo_volume_mounts,
    odoo_volumes, FIELD_MANAGER,
};

const FINALIZER: &str = "bemade.org/postgres-cleanup";

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

/// Build an OwnerReference pointing at the given OdooInstance.
pub fn owner_ref(instance: &OdooInstance) -> OwnerReference {
    controller_owner_ref(instance)
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

    Controller::new(instances, WatcherConfig::default())
        .owns(deployments, WatcherConfig::default())
        .owns(services, WatcherConfig::default())
        .owns(ingresses, WatcherConfig::default())
        .owns(configmaps, WatcherConfig::default())
        .owns(secrets, WatcherConfig::default())
        .owns(pvcs, WatcherConfig::default())
        // Watch job CRDs and map back to the owning OdooInstance.
        .watches(init_jobs, WatcherConfig::default(), map_init_job_to_instance)
        .watches(upgrade_jobs, WatcherConfig::default(), map_upgrade_job_to_instance)
        .watches(restore_jobs, WatcherConfig::default(), map_restore_job_to_instance)
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

fn map_init_job_to_instance(job: OdooInitJob) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job.spec.odoo_instance_ref.namespace.clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

fn map_upgrade_job_to_instance(job: OdooUpgradeJob) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job.spec.odoo_instance_ref.namespace.clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

fn map_restore_job_to_instance(job: OdooRestoreJob) -> Option<kube::runtime::reflector::ObjectRef<OdooInstance>> {
    let ns = job.spec.odoo_instance_ref.namespace.clone()
        .or_else(|| job.metadata.namespace.clone())?;
    Some(kube::runtime::reflector::ObjectRef::new(&job.spec.odoo_instance_ref.name).within(&ns))
}

// ── Reconcile ─────────────────────────────────────────────────────────────────

async fn reconcile(instance: Arc<OdooInstance>, ctx: Arc<Context>) -> Result<Action> {
    let ns = instance.namespace().unwrap_or_default();
    let _name = instance.name_any();
    let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);

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
    if apply_defaults(client, &ns, &name, instance, ctx).await? {
        info!(%name, "spec defaults applied from operator configuration");
        publish_event(
            ctx, instance, EventType::Normal, "DefaultsApplied", "Reconcile",
            Some("Operator defaults applied to spec".to_string()),
        ).await;
        // Re-fetch is handled by the requeue — the finalizer helper will
        // re-enter reconcile_instance with the updated spec.
        return Ok(Action::requeue(Duration::from_secs(0)));
    }

    // Load postgres cluster config.
    let (_cluster_name, pg_cluster) = load_postgres_cluster(ctx, instance).await?;

    // Ensure all child resources (phase-independent infrastructure).
    let oref = owner_ref(instance);
    ensure_odoo_user_secret(client, &ns, &name, &oref).await?;
    ensure_postgres_role(ctx, instance, &pg_cluster).await?;
    ensure_filestore_pvc(client, &ns, &name, instance, ctx, &oref).await?;
    ensure_config_map(client, &ns, &name, instance, &pg_cluster, &oref).await?;
    ensure_service(client, &ns, &name, &oref).await?;
    ensure_ingress(client, &ns, &name, instance, &oref).await?;
    ensure_deployment(client, &ns, &name, instance, ctx, &oref).await?;

    // Gather the observed world into a snapshot.
    let snapshot = super::state_machine::ReconcileSnapshot::gather(
        client, &ns, &name, instance,
    ).await?;

    // Patch non-phase status fields (readyReplicas, url, conditions, etc.).
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
    let condition = phase_to_condition(&current_phase, instance.metadata.generation.unwrap_or(0));

    let status_patch = json!({
        "status": {
            "readyReplicas": snapshot.ready_replicas,
            "ready": ready,
            "url": url,
            "targetReplicas": instance.spec.replicas,
            "dbInitialized": snapshot.db_initialized,
            "conditions": [condition],
        }
    });
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&status_patch))
        .await?;

    // Run the state machine: ensure phase outputs, evaluate transitions.
    let previous_phase = instance.status.as_ref().and_then(|s| s.phase.clone());
    let action = super::state_machine::run_state_machine(instance, ctx, &snapshot).await?;

    // Re-read phase after state machine may have patched it.
    let new_phase = api.get_status(&name).await?
        .status
        .and_then(|s| s.phase);

    // Fire event + webhook on phase transition.
    if new_phase != previous_phase {
        let phase_display = new_phase.as_ref().map(|p| format!("{p}")).unwrap_or_default();
        let prev_str = previous_phase.as_ref().map(|p| format!("{p}")).unwrap_or_default();
        info!(%name, from = %prev_str, to = %phase_display, "phase changed");
        publish_event(
            ctx, instance, EventType::Normal, "PhaseChanged", "Reconcile",
            Some(format!("Phase changed from {prev_str} to {phase_display}")),
        ).await;
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
        ctx, instance, EventType::Normal, "Cleanup", "Finalize",
        Some("Deleting postgres role".to_string()),
    ).await;

    if let Ok((_cluster_name, pg_cluster)) = load_postgres_cluster(ctx, instance).await {
        let username = odoo_username(&ns, &name);
        if let Err(e) = ctx.postgres.delete_role(&pg_cluster, &username).await {
            warn!(%name, %e, "failed to delete postgres role — removing finalizer anyway");
            publish_event(
                ctx, instance, EventType::Warning, "CleanupFailed", "Finalize",
                Some(format!("Failed to delete postgres role: {e}")),
            ).await;
        }
    }

    Ok(Action::await_change())
}

pub fn phase_to_condition(phase: &OdooInstancePhase, generation: i64) -> Condition {
    let status = if *phase == OdooInstancePhase::Running {
        "True"
    } else {
        "False"
    };
    Condition {
        type_: "Ready".to_string(),
        status: status.to_string(),
        reason: format!("{phase}"),
        message: format!("{phase}"),
        observed_generation: Some(generation),
        last_transition_time: Time(chrono::Utc::now()),
    }
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
    let secret = secrets.get(secret_name).await.map_err(|e| {
        Error::config(format!("reading {secret_name} secret: {e}"))
    })?;

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
                let cfg = clusters
                    .get(cluster_name)
                    .ok_or_else(|| Error::config(format!("postgres cluster {cluster_name:?} not found")))?;
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

// ── Child resource helpers ────────────────────────────────────────────────────

/// Write operator-level defaults into unset spec fields and persist via patch.
/// Returns true if the spec was changed (caller should requeue).
async fn apply_defaults(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
) -> Result<bool> {
    let mut patch = serde_json::Map::new();

    if instance.spec.image.is_none() {
        let img = if ctx.defaults.odoo_image.is_empty() {
            "odoo:18.0".to_string()
        } else {
            ctx.defaults.odoo_image.clone()
        };
        patch.insert("image".into(), json!(img));
    }

    // Filestore defaults.
    let fs = instance.spec.filestore.as_ref();
    let mut fs_patch = serde_json::Map::new();
    if fs.and_then(|f| f.storage_class.as_ref()).is_none() {
        let sc = if ctx.defaults.storage_class.is_empty() {
            "standard".to_string()
        } else {
            ctx.defaults.storage_class.clone()
        };
        fs_patch.insert("storageClass".into(), json!(sc));
    }
    if fs.and_then(|f| f.storage_size.as_ref()).is_none() {
        let sz = if ctx.defaults.storage_size.is_empty() {
            "2Gi".to_string()
        } else {
            ctx.defaults.storage_size.clone()
        };
        fs_patch.insert("storageSize".into(), json!(sz));
    }
    if !fs_patch.is_empty() {
        patch.insert("filestore".into(), json!(fs_patch));
    }

    // Ingress defaults.
    let mut ing_patch = serde_json::Map::new();
    if instance.spec.ingress.issuer.is_none() && !ctx.defaults.ingress_issuer.is_empty() {
        ing_patch.insert("issuer".into(), json!(ctx.defaults.ingress_issuer));
    }
    if instance.spec.ingress.class.is_none() && !ctx.defaults.ingress_class.is_empty() {
        ing_patch.insert("class".into(), json!(ctx.defaults.ingress_class));
    }
    if !ing_patch.is_empty() {
        patch.insert("ingress".into(), json!(ing_patch));
    }

    // Resources, affinity, tolerations defaults.
    if instance.spec.resources.is_none() && ctx.defaults.resources.is_some() {
        patch.insert("resources".into(), json!(ctx.defaults.resources));
    }
    if instance.spec.affinity.is_none() && ctx.defaults.affinity.is_some() {
        patch.insert("affinity".into(), json!(ctx.defaults.affinity));
    }
    if instance.spec.tolerations.is_empty() && !ctx.defaults.tolerations.is_empty() {
        patch.insert("tolerations".into(), json!(ctx.defaults.tolerations));
    }

    if patch.is_empty() {
        return Ok(false);
    }

    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let spec_patch = json!({"spec": patch});
    api.patch(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&spec_patch))
        .await?;
    Ok(true)
}

async fn ensure_odoo_user_secret(client: &Client, ns: &str, name: &str, oref: &OwnerReference) -> Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret_name = format!("{name}-odoo-user");

    // Only create if it doesn't exist (credentials are generated once).
    match secrets.get(&secret_name).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            let username = odoo_username(ns, name);
            let password = generate_password();
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(secret_name.clone()),
                    namespace: Some(ns.to_string()),
                    owner_references: Some(vec![oref.clone()]),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([
                    ("username".to_string(), username),
                    ("password".to_string(), password),
                ])),
                ..Default::default()
            };
            secrets.create(&PostParams::default(), &secret).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn ensure_postgres_role(
    ctx: &Context,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    let secret = secrets.get(&format!("{name}-odoo-user")).await?;

    let data = secret.data.unwrap_or_default();
    let username = String::from_utf8_lossy(data.get("username").map(|v| v.0.as_slice()).unwrap_or_default()).to_string();
    let password = String::from_utf8_lossy(data.get("password").map(|v| v.0.as_slice()).unwrap_or_default()).to_string();

    ctx.postgres.ensure_role(pg, &username, &password).await
}

async fn ensure_filestore_pvc(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
) -> Result<()> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let pvc_name = format!("{name}-filestore-pvc");

    let storage_size = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_size.as_deref())
        .unwrap_or(&ctx.defaults.storage_size);
    let storage_class = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.as_deref())
        .unwrap_or(&ctx.defaults.storage_class);

    // Only create if it doesn't exist (PVCs are immutable after creation).
    if pvcs.get(&pvc_name).await.is_ok() {
        return Ok(());
    }

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(storage_size.to_string()),
                )])),
                ..Default::default()
            }),
            storage_class_name: Some(storage_class.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    pvcs.create(&PostParams::default(), &pvc).await?;
    Ok(())
}

async fn ensure_config_map(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
    oref: &OwnerReference,
) -> Result<()> {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm_name = format!("{name}-odoo-conf");
    let username = odoo_username(ns, name);
    let db = db_name(instance);

    // Read password from the odoo-user secret.
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets.get(&format!("{name}-odoo-user")).await?;
    let data = secret.data.unwrap_or_default();
    let password = String::from_utf8_lossy(data.get("password").map(|v| v.0.as_slice()).unwrap_or_default()).to_string();

    let conf = build_odoo_conf(
        &username,
        &password,
        &instance.spec.admin_password,
        &pg.host,
        pg.port,
        &db,
        &instance.spec.config_options,
    );

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        data: Some(BTreeMap::from([
            ("odoo.conf".to_string(), conf),
            ("db_host".to_string(), pg.host.clone()),
            ("db_port".to_string(), pg.port.to_string()),
            ("db_name".to_string(), db),
            ("db_user".to_string(), username),
            ("db_password".to_string(), password),
        ])),
        ..Default::default()
    };

    cms.patch(
        &cm_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&cm),
    )
    .await?;
    Ok(())
}

async fn ensure_service(client: &Client, ns: &str, name: &str, oref: &OwnerReference) -> Result<()> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), ns);
    let svc = Service {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            type_: Some("ClusterIP".to_string()),
            ports: Some(vec![
                ServicePort {
                    name: Some("http".to_string()),
                    port: 8069,
                    target_port: Some(IntOrString::Int(8069)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                },
                ServicePort {
                    name: Some("websocket".to_string()),
                    port: 8072,
                    target_port: Some(IntOrString::Int(8072)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };

    svcs.patch(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&svc))
        .await?;
    Ok(())
}

async fn ensure_ingress(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    oref: &OwnerReference,
) -> Result<()> {
    let ingresses: Api<Ingress> = Api::namespaced(client.clone(), ns);

    let mut annotations = BTreeMap::new();
    if let Some(ref issuer) = instance.spec.ingress.issuer {
        annotations.insert("cert-manager.io/cluster-issuer".to_string(), issuer.clone());
    }

    let path_type = "Prefix".to_string();
    let rules: Vec<IngressRule> = instance
        .spec
        .ingress
        .hosts
        .iter()
        .map(|host| IngressRule {
            host: Some(host.clone()),
            http: Some(HTTPIngressRuleValue {
                paths: vec![
                    HTTPIngressPath {
                        path: Some("/websocket".to_string()),
                        path_type: path_type.clone(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: name.to_string(),
                                port: Some(ServiceBackendPort {
                                    number: Some(8072),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    },
                    HTTPIngressPath {
                        path: Some("/".to_string()),
                        path_type: path_type.clone(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: name.to_string(),
                                port: Some(ServiceBackendPort {
                                    number: Some(8069),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    },
                ],
            }),
        })
        .collect();

    let ing = Ingress {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            annotations: Some(annotations),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(K8sIngressSpec {
            ingress_class_name: instance.spec.ingress.class.clone(),
            rules: Some(rules),
            tls: Some(vec![IngressTLS {
                hosts: Some(instance.spec.ingress.hosts.clone()),
                secret_name: Some(format!("{name}-tls")),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    ingresses
        .patch(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&ing))
        .await?;
    Ok(())
}

async fn ensure_deployment(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
) -> Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);

    // Replicas are managed by the state machine via scale_deployment().
    // Here we only ensure the Deployment spec (image, probes, volumes, etc.)
    // exists.  We read the current replica count so we don't clobber it.
    let current_replicas = match deployments.get(name).await {
        Ok(dep) => dep.spec.and_then(|s| s.replicas).unwrap_or(0),
        Err(_) => 0,
    };
    let replicas = current_replicas;
    let image = instance
        .spec
        .image
        .as_deref()
        .unwrap_or(&ctx.defaults.odoo_image);

    let strategy_type = instance
        .spec
        .strategy
        .as_ref()
        .map(|s| &s.strategy_type)
        .unwrap_or(&DeploymentStrategyType::Recreate);
    let k8s_strategy = match strategy_type {
        DeploymentStrategyType::Recreate => "Recreate",
        DeploymentStrategyType::RollingUpdate => "RollingUpdate",
    };

    let probe_startup = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.startup_path.as_str())
        .unwrap_or("/web/health");
    let probe_liveness = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.liveness_path.as_str())
        .unwrap_or("/web/health");
    let probe_readiness = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.readiness_path.as_str())
        .unwrap_or("/web/health");

    // Hash odoo.conf for rollout trigger.
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm = cms.get(&format!("{name}-odoo-conf")).await?;
    let conf_content = cm
        .data
        .as_ref()
        .and_then(|d| d.get("odoo.conf"))
        .map(|s| s.as_str())
        .unwrap_or("");
    let conf_hash = sha256_hex(conf_content);

    let make_http_probe = |path: &str| -> Probe {
        Probe {
            http_get: Some(HTTPGetAction {
                path: Some(path.to_string()),
                port: IntOrString::Int(8069),
                ..Default::default()
            }),
            ..Default::default()
        }
    };

    let dep = Deployment {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some(k8s_strategy.to_string()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
                    annotations: Some(BTreeMap::from([(
                        "bemade.org/odoo-conf-hash".to_string(),
                        conf_hash,
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    image_pull_secrets: image_pull_secrets(instance),
                    affinity: instance.spec.affinity.clone(),
                    tolerations: if instance.spec.tolerations.is_empty() {
                        None
                    } else {
                        Some(instance.spec.tolerations.clone())
                    },
                    security_context: Some(odoo_security_context()),
                    volumes: Some(odoo_volumes(name)),
                    containers: vec![Container {
                        name: format!("odoo-{name}"),
                        image: Some(image.to_string()),
                        image_pull_policy: Some("IfNotPresent".to_string()),
                        command: Some(vec![
                            "/entrypoint.sh".to_string(),
                            "odoo".to_string(),
                        ]),
                        ports: Some(vec![
                            ContainerPort {
                                name: Some("http".to_string()),
                                container_port: 8069,
                                ..Default::default()
                            },
                            ContainerPort {
                                name: Some("websocket".to_string()),
                                container_port: 8072,
                                ..Default::default()
                            },
                        ]),
                        volume_mounts: Some(odoo_volume_mounts()),
                        resources: instance.spec.resources.clone(),
                        startup_probe: Some(Probe {
                            initial_delay_seconds: Some(5),
                            period_seconds: Some(10),
                            timeout_seconds: Some(5),
                            failure_threshold: Some(30),
                            ..make_http_probe(probe_startup)
                        }),
                        liveness_probe: Some(Probe {
                            period_seconds: Some(15),
                            timeout_seconds: Some(5),
                            failure_threshold: Some(3),
                            ..make_http_probe(probe_liveness)
                        }),
                        readiness_probe: Some(Probe {
                            period_seconds: Some(10),
                            timeout_seconds: Some(5),
                            failure_threshold: Some(3),
                            ..make_http_probe(probe_readiness)
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deployments
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&dep),
        )
        .await?;
    Ok(())
}

