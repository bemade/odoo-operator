//! MigratingFilestore state — waits for the rsync Job to complete.
//!
//! All orchestration (job creation, PVC setup, rollback) is handled by
//! transition actions.  The `ensure()` method only keeps both deployments
//! scaled to zero.

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{
        Container, PersistentVolumeClaim, PersistentVolumeClaimSpec,
        PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Volume, VolumeMount,
    },
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::super::helpers::{cron_depl_name, FIELD_MANAGER};
use super::super::odoo_instance::Context;
use super::super::state_machine::{scale_deployment, ReconcileSnapshot};
use super::State;

const MIGRATE_SCRIPT: &str = include_str!("../../../scripts/migrate-filestore.sh");

pub struct MigratingFilestore;

#[async_trait]
impl State for MigratingFilestore {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0 during migration.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        Ok(())
    }
}

/// Scale down deployments, create temp PVC, create rsync Job, store state.
/// Called by the `BeginFilestoreMigration` transition action.
pub async fn begin_filestore_migration(
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

/// Rollback: delete temp PVC, delete job, revert spec to previous SC, clear status.
/// Called by the `RollbackFilestoreMigration` transition action.
pub async fn rollback_filestore_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
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
                    security_context: Some(super::super::helpers::odoo_security_context()),
                    image_pull_secrets: super::super::helpers::image_pull_secrets(instance),
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
