//! Integration tests for filestore StorageClass migration.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    HostPathVolumeSource, PersistentVolume, PersistentVolumeClaim, PersistentVolumeSpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

use odoo_operator::controller::helpers::FIELD_MANAGER;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

use crate::common::*;

/// Wait for a batch/v1 Job to appear with a name matching the given prefix.
async fn wait_for_migration_job(client: &kube::Client, ns: &str, prefix: &str) -> String {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let jobs = jobs.clone();
            let pfx = prefix.to_string();
            async move {
                if let Ok(list) = jobs.list(&ListParams::default()).await {
                    for job in list.items {
                        if let Some(ref n) = job.metadata.name {
                            if n.starts_with(&pfx) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
        })
        .await,
        "migration job with prefix {prefix} never appeared"
    );
    let list = jobs.list(&ListParams::default()).await.unwrap();
    list.items
        .iter()
        .find_map(|j| {
            j.metadata
                .name
                .as_ref()
                .filter(|n| n.starts_with(prefix))
                .cloned()
        })
        .unwrap()
}

/// Wait for a PVC to appear.
async fn wait_for_pvc(client: &kube::Client, ns: &str, pvc_name: &str) {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = pvcs.clone();
            let n = pvc_name.to_string();
            async move { api.get(&n).await.is_ok() }
        })
        .await,
        "PVC {pvc_name} never appeared"
    );
}

/// Create a fake PV and bind a PVC to it (simulates what a provisioner does).
async fn fake_pvc_bound(client: &kube::Client, ns: &str, pvc_name: &str, pv_name: &str) {
    let pvs: Api<PersistentVolume> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);

    if pvs.get(pv_name).await.is_err() {
        let pv = PersistentVolume {
            metadata: ObjectMeta {
                name: Some(pv_name.to_string()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec {
                capacity: Some(
                    [("storage".to_string(), Quantity("1Gi".to_string()))]
                        .into_iter()
                        .collect(),
                ),
                access_modes: Some(vec!["ReadWriteMany".to_string()]),
                persistent_volume_reclaim_policy: Some("Retain".to_string()),
                storage_class_name: Some("standard".to_string()),
                host_path: Some(HostPathVolumeSource {
                    path: "/tmp/fake-pv".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        pvs.create(&PostParams::default(), &pv).await.unwrap();
    }

    let spec_patch = json!({"spec": {"volumeName": pv_name}});
    pvcs.patch(
        pvc_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&spec_patch),
    )
    .await
    .unwrap();

    let status_patch = json!({"status": {"phase": "Bound"}});
    pvcs.patch_status(
        pvc_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&status_patch),
    )
    .await
    .unwrap();
}

// ─── Test 1: Happy path from Running ────────────────────────────────────────

#[tokio::test]
async fn migrate_filestore_from_running() {
    let name = "test-mig-run";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    // Fake the original PVC as bound.
    let orig_pvc = format!("{name}-filestore-pvc");
    fake_pvc_bound(c, ns, &orig_pvc, &format!("{name}-pv-old")).await;

    // Change storageClass to trigger migration.
    patch_instance_spec(
        c,
        ns,
        name,
        json!({"filestore": {"storageClass": "juicefs"}}),
    )
    .await;

    // BeginFilestoreMigration creates temp PVC + rsync Job atomically with
    // the phase transition.  By the time we see MigratingFilestore, both exist.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingFilestore).await,
        "expected MigratingFilestore phase"
    );

    // Fake deployments scaled down.
    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;

    // Fake temp PVC as bound (the transition already created it).
    let temp_pvc = format!("{name}-filestore-pvc-temp");
    wait_for_pvc(c, ns, &temp_pvc).await;
    fake_pvc_bound(c, ns, &temp_pvc, &format!("{name}-pv-new")).await;

    // Fake rsync job success (the transition already created it).
    let job_name = wait_for_migration_job(c, ns, &format!("{name}-migrate-")).await;
    fake_job_succeeded(c, ns, &job_name).await;

    // CompleteFilestoreMigration fires: deletes old PVC, rebinds PV, creates
    // final PVC.  Help envtest through PVC lifecycle in the background.
    let pv_name = format!("{name}-pv-new");
    let assist_handle = {
        let client = c.clone();
        let ns = ns.to_string();
        let orig_pvc = orig_pvc.clone();
        let temp_pvc = temp_pvc.clone();
        let pv_name = pv_name.clone();
        tokio::spawn(async move {
            let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
            loop {
                let _ = pvcs
                    .patch(
                        &orig_pvc,
                        &PatchParams::apply(FIELD_MANAGER),
                        &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
                    )
                    .await;
                let _ = pvcs
                    .patch(
                        &temp_pvc,
                        &PatchParams::apply(FIELD_MANAGER),
                        &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
                    )
                    .await;
                if pvcs.get(&orig_pvc).await.is_ok() {
                    let _ = pvcs
                        .patch(
                            &orig_pvc,
                            &PatchParams::apply(FIELD_MANAGER),
                            &Patch::Merge(&json!({"spec": {"volumeName": &pv_name}})),
                        )
                        .await;
                    let _ = pvcs
                        .patch_status(
                            &orig_pvc,
                            &PatchParams::apply(FIELD_MANAGER),
                            &Patch::Merge(&json!({"status": {"phase": "Bound"}})),
                        )
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting phase after migration"
    );
    assist_handle.abort();

    fake_deployment_ready(c, ns, name, 1).await;
    let _handle = keep_deployment_ready(c.clone(), ns.to_string(), name.to_string(), 1);

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Running).await,
        "expected Running phase after migration"
    );
}

// ─── Test 2: Migration from Stopped ─────────────────────────────────────────

#[tokio::test]
async fn migrate_filestore_from_stopped() {
    let name = "test-mig-stop";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    let orig_pvc = format!("{name}-filestore-pvc");
    fake_pvc_bound(c, ns, &orig_pvc, &format!("{name}-pv-old")).await;

    // Scale to 0 → Stopped.
    patch_instance_spec(c, ns, name, json!({"replicas": 0})).await;
    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Stopped).await,
        "expected Stopped"
    );

    // Trigger migration.
    patch_instance_spec(
        c,
        ns,
        name,
        json!({"filestore": {"storageClass": "juicefs"}}),
    )
    .await;

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingFilestore).await,
        "expected MigratingFilestore"
    );

    let temp_pvc = format!("{name}-filestore-pvc-temp");
    wait_for_pvc(c, ns, &temp_pvc).await;
    fake_pvc_bound(c, ns, &temp_pvc, &format!("{name}-pv-new2")).await;

    let job_name = wait_for_migration_job(c, ns, &format!("{name}-migrate-")).await;
    fake_job_succeeded(c, ns, &job_name).await;

    let pv_name = format!("{name}-pv-new2");
    let assist_handle = {
        let client = c.clone();
        let ns = ns.to_string();
        let orig_pvc = orig_pvc.clone();
        let temp_pvc = temp_pvc.clone();
        let pv_name = pv_name.clone();
        tokio::spawn(async move {
            let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);
            loop {
                let _ = pvcs
                    .patch(
                        &orig_pvc,
                        &PatchParams::apply(FIELD_MANAGER),
                        &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
                    )
                    .await;
                let _ = pvcs
                    .patch(
                        &temp_pvc,
                        &PatchParams::apply(FIELD_MANAGER),
                        &Patch::Merge(&json!({"metadata": {"finalizers": null}})),
                    )
                    .await;
                if pvcs.get(&orig_pvc).await.is_ok() {
                    let _ = pvcs
                        .patch(
                            &orig_pvc,
                            &PatchParams::apply(FIELD_MANAGER),
                            &Patch::Merge(&json!({"spec": {"volumeName": &pv_name}})),
                        )
                        .await;
                    let _ = pvcs
                        .patch_status(
                            &orig_pvc,
                            &PatchParams::apply(FIELD_MANAGER),
                            &Patch::Merge(&json!({"status": {"phase": "Bound"}})),
                        )
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Stopped).await,
        "expected Stopped after migration with replicas=0"
    );
    assist_handle.abort();
}

// ─── Test 3: Rsync failure → rollback ───────────────────────────────────────

#[tokio::test]
async fn migrate_filestore_rsync_failure_rollback() {
    let name = "test-mig-fail";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    let orig_pvc = format!("{name}-filestore-pvc");
    fake_pvc_bound(c, ns, &orig_pvc, &format!("{name}-pv-old")).await;

    patch_instance_spec(
        c,
        ns,
        name,
        json!({"filestore": {"storageClass": "juicefs"}}),
    )
    .await;

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingFilestore).await,
        "expected MigratingFilestore"
    );

    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;

    let temp_pvc = format!("{name}-filestore-pvc-temp");
    wait_for_pvc(c, ns, &temp_pvc).await;
    fake_pvc_bound(c, ns, &temp_pvc, &format!("{name}-pv-new3")).await;

    // Rsync job fails.
    let job_name = wait_for_migration_job(c, ns, &format!("{name}-migrate-")).await;
    fake_job_failed(c, ns, &job_name).await;

    // RollbackFilestoreMigration fires: reverts storageClass, cleans up,
    // transitions to Starting.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting after rollback"
    );

    // Verify storageClass was reverted to "standard" (the original).
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get(name).await.unwrap();
    let sc = inst
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.as_deref());
    assert_eq!(sc, Some("standard"), "storageClass should be reverted");
}

// ─── Test 4: Not triggered during init ──────────────────────────────────────

#[tokio::test]
async fn migrate_filestore_not_triggered_during_init() {
    let name = "test-mig-noinit";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let orig_pvc = format!("{name}-filestore-pvc");
    wait_for_pvc(c, ns, &orig_pvc).await;
    fake_pvc_bound(c, ns, &orig_pvc, &format!("{name}-pv-old")).await;

    let init_api: Api<odoo_operator::crd::odoo_init_job::OdooInitJob> =
        Api::namespaced(c.clone(), ns);
    let init_job: odoo_operator::crd::odoo_init_job::OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": format!("{name}-init"), "namespace": ns },
        "spec": { "odooInstanceRef": { "name": name } }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &init_job)
        .await
        .unwrap();

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Initializing).await,
        "expected Initializing"
    );

    patch_instance_spec(
        c,
        ns,
        name,
        json!({"filestore": {"storageClass": "juicefs"}}),
    )
    .await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get_status(name).await.unwrap();
    let phase = inst.status.as_ref().and_then(|s| s.phase.as_ref());
    assert_ne!(
        phase,
        Some(&OdooInstancePhase::MigratingFilestore),
        "migration should not trigger during Initializing"
    );
}
