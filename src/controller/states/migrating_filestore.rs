//! MigratingFilestore state — orchestrates filestore PVC migration to a new
//! StorageClass.  Uses internal sub-steps tracked in `status.migrationStep`.

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
use kube::api::{Api, DeleteParams, Patch, PatchParams, ResourceExt};
use kube::Client;
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::super::helpers::{cron_depl_name, FIELD_MANAGER};
use super::super::odoo_instance::Context;
use super::super::state_machine::{scale_deployment, JobStatus, ReconcileSnapshot};
use super::State;

const MIGRATE_SCRIPT: &str = include_str!("../../../scripts/migrate-filestore.sh");

pub struct MigratingFilestore;

#[async_trait]
impl State for MigratingFilestore {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Always keep deployments at 0 during migration.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        let step = instance
            .status
            .as_ref()
            .and_then(|s| s.migration_step.as_deref())
            .unwrap_or("ScalingDown");

        match step {
            "ScalingDown" => self.ensure_scaling_down(instance, client, snapshot).await,
            "CreatingTempPvc" => self.ensure_creating_temp_pvc(instance, client, ctx).await,
            "RsyncRunning" => {
                self.ensure_rsync_running(instance, client, ctx, snapshot)
                    .await
            }
            "DeletingOldPvc" => self.ensure_deleting_old_pvc(instance, client).await,
            "RebindingPv" => self.ensure_rebinding_pv(instance, client).await,
            "Complete" => Ok(()), // transition guard will fire
            _ => Ok(()),
        }
    }
}

impl MigratingFilestore {
    /// Wait for both deployments to be fully scaled down.
    async fn ensure_scaling_down(
        &self,
        instance: &OdooInstance,
        client: &Client,
        snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        if snapshot.ready_replicas == 0
            && snapshot.cron_ready_replicas == 0
            && snapshot.deployment_replicas == 0
            && snapshot.cron_deployment_replicas == 0
        {
            advance_step(client, instance, "CreatingTempPvc").await?;
        }
        Ok(())
    }

    /// Create the temporary PVC with the new StorageClass.
    async fn ensure_creating_temp_pvc(
        &self,
        instance: &OdooInstance,
        client: &Client,
        ctx: &Context,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");

        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);

        // Idempotent: check if temp PVC already exists.
        if pvcs.get(&temp_pvc_name).await.is_ok() {
            info!(%inst_name, "temp PVC already exists, advancing to RsyncRunning");
            advance_step(client, instance, "RsyncRunning").await?;
            return Ok(());
        }

        let desired_class = instance
            .spec
            .filestore
            .as_ref()
            .and_then(|f| f.storage_class.clone())
            .unwrap_or_else(|| ctx.defaults.storage_class.clone());

        // Read size from the existing PVC (not the spec, since the PVC may have
        // been expanded beyond the original spec size).
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

        let pvc = PersistentVolumeClaim {
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

        pvcs.create(&Default::default(), &pvc).await?;
        info!(%inst_name, %temp_pvc_name, "created temp PVC for migration");
        advance_step(client, instance, "RsyncRunning").await?;
        Ok(())
    }

    /// Create the rsync Job (if not yet created) and wait for it to succeed.
    async fn ensure_rsync_running(
        &self,
        instance: &OdooInstance,
        client: &Client,
        ctx: &Context,
        snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let migration_job_name = instance
            .status
            .as_ref()
            .and_then(|s| s.migration_job_name.clone());

        // If no job yet, create one.
        if migration_job_name.is_none() {
            let job = build_rsync_job(&inst_name, &ns, instance, ctx);
            let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
            let created = jobs.create(&Default::default(), &job).await?;
            let job_name = created.name_any();
            info!(%inst_name, %job_name, "created rsync migration job");

            // Store the job name in status.
            let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
            let patch = json!({"status": {"migrationJobName": &job_name}});
            api.patch_status(
                &inst_name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await?;
            return Ok(());
        }

        // Job exists — check status.
        match snapshot.migration_job {
            JobStatus::Succeeded => {
                info!(%inst_name, "migration rsync succeeded");
                // Clean up the completed Job.
                let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
                if let Some(ref name) = migration_job_name {
                    let _ = jobs.delete(name, &DeleteParams::background()).await;
                }
                advance_step(client, instance, "DeletingOldPvc").await?;
            }
            JobStatus::Failed => {
                warn!(%inst_name, "migration rsync failed — will retry");
                // Clear the job name so a new one is created on next reconcile.
                let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
                if let Some(ref name) = migration_job_name {
                    let _ = jobs.delete(name, &DeleteParams::background()).await;
                }
                let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
                let patch = json!({"status": {"migrationJobName": null}});
                api.patch_status(
                    &inst_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch),
                )
                .await?;
            }
            _ => {} // Active or Absent — wait.
        }
        Ok(())
    }

    /// Delete the original PVC.
    async fn ensure_deleting_old_pvc(
        &self,
        instance: &OdooInstance,
        client: &Client,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let pvc_name = format!("{inst_name}-filestore-pvc");

        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
        match pvcs.get(&pvc_name).await {
            Ok(_) => {
                pvcs.delete(&pvc_name, &DeleteParams::default()).await?;
                info!(%inst_name, "deleted old filestore PVC");
                // Verify deletion completed (may still have deletionTimestamp).
                match pvcs.get(&pvc_name).await {
                    Err(kube::Error::Api(ref err)) if err.code == 404 => {
                        advance_step(client, instance, "RebindingPv").await?;
                    }
                    _ => {} // Still exists (finalizer?) — will retry next reconcile.
                }
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                // Already gone — advance.
                advance_step(client, instance, "RebindingPv").await?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Rebind: get PV from temp PVC → set Retain + clear claimRef → delete temp
    /// PVC → create final PVC with original name bound to that PV.
    async fn ensure_rebinding_pv(&self, instance: &OdooInstance, client: &Client) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");
        let final_pvc_name = format!("{inst_name}-filestore-pvc");

        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
        let pvs: Api<k8s_openapi::api::core::v1::PersistentVolume> = Api::all(client.clone());

        let stored_pv = instance
            .status
            .as_ref()
            .and_then(|s| s.migration_pv_name.clone());

        // Phase A: temp PVC still exists — extract PV, patch it, delete temp PVC.
        if let Ok(temp_pvc) = pvcs.get(&temp_pvc_name).await {
            let pv_name = temp_pvc
                .spec
                .as_ref()
                .and_then(|s| s.volume_name.clone())
                .or(stored_pv.clone());

            let Some(ref pv_name) = pv_name else {
                // PVC not yet bound — wait for provisioner.
                return Ok(());
            };

            // Store PV name in status if not already done.
            if stored_pv.is_none() {
                let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
                let patch = json!({"status": {"migrationPvName": pv_name}});
                api.patch_status(
                    &inst_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch),
                )
                .await?;
            }

            // Patch PV: set Retain policy and clear claimRef.
            let pv_patch = json!({
                "spec": {
                    "persistentVolumeReclaimPolicy": "Retain",
                    "claimRef": null,
                }
            });
            pvs.patch(
                pv_name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&pv_patch),
            )
            .await?;

            // Delete temp PVC.
            pvcs.delete(&temp_pvc_name, &DeleteParams::default())
                .await?;
            info!(%inst_name, %pv_name, "patched PV and deleted temp PVC");
            return Ok(());
        }

        // Phase B: temp PVC is gone — create final PVC bound to the PV.
        let Some(ref pv_name) = stored_pv else {
            warn!(%inst_name, "no temp PVC and no stored PV name — cannot rebind");
            return Ok(());
        };

        match pvcs.get(&final_pvc_name).await {
            Ok(pvc) => {
                // PVC exists — check if bound.
                let phase = pvc
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("");
                if phase == "Bound" {
                    info!(%inst_name, "final PVC bound — migration complete");
                    advance_step(client, instance, "Complete").await?;
                }
                // else: still pending, wait.
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                // Create the final PVC.
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

                // Build ownerReference from the instance.
                let oref = super::super::helpers::controller_owner_ref(instance);

                let pvc = PersistentVolumeClaim {
                    metadata: ObjectMeta {
                        name: Some(final_pvc_name.clone()),
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

                pvcs.create(&Default::default(), &pvc).await?;
                info!(%inst_name, %pv_name, "created final PVC bound to migrated PV");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

/// Advance the migration sub-step by patching `status.migrationStep`.
async fn advance_step(client: &Client, instance: &OdooInstance, step: &str) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    let patch = json!({"status": {"migrationStep": step}});
    api.patch_status(
        &name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    info!(%name, %step, "migration step advanced");
    Ok(())
}

/// Build the rsync batch/v1 Job that copies data between PVCs.
fn build_rsync_job(inst_name: &str, ns: &str, instance: &OdooInstance, _ctx: &Context) -> Job {
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
