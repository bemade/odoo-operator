//! FinalizingFilestoreMigration state — rsync is done, PVC rebind in progress.
//!
//! The `ensure()` method keeps deployments at zero and drives the idempotent
//! PVC rebind sequence (delete old PVC, patch PV, create final PVC).
//! Once the final PVC exists with the correct StorageClass, the transition
//! guard fires and clears migration status.

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec},
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::super::helpers::{cron_depl_name, FIELD_MANAGER};
use super::super::odoo_instance::Context;
use super::super::state_machine::{scale_deployment, ReconcileSnapshot};
use super::State;

pub struct FinalizingFilestoreMigration;

#[async_trait]
impl State for FinalizingFilestoreMigration {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        // Drive the PVC rebind — idempotent, retries on each reconcile tick.
        finalize_filestore_pvc_rebind(instance, ctx).await?;

        Ok(())
    }
}

/// Transition action: store PV name and delete the rsync Job.
/// Called by `CompleteFilestoreMigration` when entering this phase.
pub async fn complete_filestore_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    // 1. Read PV name from temp PVC and persist in status.
    if instance
        .status
        .as_ref()
        .and_then(|s| s.migration_pv_name.as_ref())
        .is_none()
    {
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
        let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");
        let pv_name = pvcs
            .get(&temp_pvc_name)
            .await?
            .spec
            .as_ref()
            .and_then(|s| s.volume_name.clone())
            .ok_or_else(|| {
                crate::error::Error::Reconcile("temp PVC has no volumeName — not bound yet".into())
            })?;
        let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
        let patch = json!({"status": {"migrationPvName": &pv_name}});
        api.patch_status(
            &inst_name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await?;
        info!(%inst_name, %pv_name, "stored migration PV name");
    }

    // 2. Delete the rsync Job — its completed pod holds pvc-protection on the
    //    old PVC.  Foreground deletion cascades to the pod.
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    if let Some(ref job_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_job_name.clone())
    {
        match jobs.delete(job_name, &DeleteParams::foreground()).await {
            Ok(_) => info!(%inst_name, %job_name, "deleted migration job"),
            Err(kube::Error::Api(ref e)) if e.code == 404 => {}
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

/// Idempotent PVC rebind — called by `ensure()` on each reconcile tick.
/// Deletes old PVC, patches PV, deletes temp PVC, creates final PVC.
async fn finalize_filestore_pvc_rebind(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);

    let orig_pvc_name = format!("{inst_name}-filestore-pvc");
    let temp_pvc_name = format!("{inst_name}-filestore-pvc-temp");

    let pv_name = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_pv_name.clone())
        .ok_or_else(|| {
            crate::error::Error::Reconcile("migrationPvName not set in status".into())
        })?;

    // 1. Delete old PVC (idempotent).
    match pvcs.delete(&orig_pvc_name, &DeleteParams::default()).await {
        Ok(_) => info!(%inst_name, "deleted old filestore PVC"),
        Err(kube::Error::Api(ref e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }

    // 2. If old PVC still terminating, return Ok — next reconcile will retry.
    if pvcs.get(&orig_pvc_name).await.is_ok() {
        return Ok(());
    }

    // 3. Delete temp PVC first (idempotent) — must happen before clearing
    //    claimRef, otherwise the PV controller may re-bind to the temp PVC.
    match pvcs.delete(&temp_pvc_name, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(ref e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }

    // 4. Patch PV: set Retain + clear claimRef.  Use default PatchParams
    //    (not SSA) so the null merge reliably clears the field.
    let pvs: Api<k8s_openapi::api::core::v1::PersistentVolume> = Api::all(client.clone());
    pvs.patch(
        &pv_name,
        &PatchParams::default(),
        &Patch::Merge(&json!({
            "spec": {
                "persistentVolumeReclaimPolicy": "Retain",
                "claimRef": null,
            }
        })),
    )
    .await?;

    // 5. Create final PVC (idempotent — skip if exists).
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
        let oref = super::super::helpers::controller_owner_ref(instance);

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

    Ok(())
}
