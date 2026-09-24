//! Ephemeral (emptyDir) filestore — no PVC is provisioned, pods mount an
//! emptyDir under the same volume name, and a flip from a persistent
//! filestore RETAINS the existing PVC instead of deleting it.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::{Api, PostParams};
use serde_json::json;

use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

use super::common::{
    fake_job_succeeded, fast_track_to_running, patch_instance_spec, wait_for,
    wait_for_k8s_job_name, wait_for_phase, TestContext, POLL, TIMEOUT,
};

/// Spec for an instance created ephemeral from the start: no PVC ever
/// exists for it, which is the case a retained PVC would otherwise mask.
fn ephemeral_instance_json(name: &str, ns: &str) -> serde_json::Value {
    json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["test.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "emptyDir": true },
            "init": { "enabled": false },
        }
    })
}

/// Fetch the named deployment's "filestore" volume, if the deployment and
/// the volume exist. Returns (has_empty_dir, has_pvc).
async fn filestore_volume_sources(
    c: &kube::Client,
    ns: &str,
    deploy: &str,
) -> Option<(bool, bool)> {
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), ns);
    let d = deployments.get(deploy).await.ok()?;
    let vols = d
        .spec?
        .template
        .spec?
        .volumes?
        .into_iter()
        .find(|v| v.name == "filestore")?;
    Some((
        vols.empty_dir.is_some(),
        vols.persistent_volume_claim.is_some(),
    ))
}

/// An instance created with `filestore.emptyDir: true` gets no filestore
/// PVC, no injected storageSize/storageClass defaults, and both the web and
/// cron pod templates mount an emptyDir under the standard volume name.
#[tokio::test]
async fn empty_dir_instance_creates_no_pvc() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-emptydir";

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst: OdooInstance = serde_json::from_value(ephemeral_instance_json(name, ns)).unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance");

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // Web and cron deployments mount an emptyDir, not a PVC.
    for deploy in [name.to_string(), format!("{name}-cron")] {
        assert!(
            wait_for(TIMEOUT, POLL, || {
                let c = c.clone();
                let deploy = deploy.clone();
                let ns = ns.to_string();
                async move {
                    matches!(
                        filestore_volume_sources(&c, &ns, &deploy).await,
                        Some((true, false))
                    )
                }
            })
            .await,
            "{deploy}: filestore volume should be an emptyDir with no PVC reference"
        );
    }

    // No filestore PVC was created.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    assert!(
        pvcs.get(&format!("{name}-filestore-pvc")).await.is_err(),
        "no filestore PVC should exist for an emptyDir instance"
    );

    // The defaulting pass must not have injected storageSize/storageClass.
    let live = api.get(name).await.expect("instance fetch");
    let fs = live.spec.filestore.expect("filestore spec present");
    assert!(fs.empty_dir);
    assert!(
        fs.storage_size.is_none() && fs.storage_class.is_none(),
        "size/class defaults must not be injected for an emptyDir filestore"
    );
}

/// Flipping a persistent instance to `emptyDir: true` switches the pod
/// volumes to emptyDir, leaves the pre-flip storageSize/storageClass in the
/// spec (inert, so the way back reuses them), and RETAINS the existing PVC
/// (never deletes it) so the flip is reversible and a premature flip orphans
/// data recoverably. Flipping back re-mounts the retained PVC under its
/// original class; no defaults are re-injected over it.
#[tokio::test]
async fn flip_to_empty_dir_retains_pvc() {
    let name = "test-emptydir-flip";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // The persistent-mode PVC exists.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let pvc_name = format!("{name}-filestore-pvc");
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let pvcs = pvcs.clone();
            let pvc_name = pvc_name.clone();
            async move { pvcs.get(&pvc_name).await.is_ok() }
        })
        .await,
        "filestore PVC should exist before the flip"
    );

    patch_instance_spec(c, ns, name, json!({ "filestore": { "emptyDir": true } })).await;

    // Deployment volume flips to emptyDir.
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let c = c.clone();
            let ns = ns.to_string();
            let deploy = name.to_string();
            async move {
                matches!(
                    filestore_volume_sources(&c, &ns, &deploy).await,
                    Some((true, false))
                )
            }
        })
        .await,
        "filestore volume should become an emptyDir after the flip"
    );

    // The PVC is retained — this is the recoverability contract.
    assert!(
        pvcs.get(&pvc_name).await.is_ok(),
        "filestore PVC must be RETAINED after the flip to emptyDir"
    );

    // The pre-flip size/class stay in the spec, inert: they are what makes
    // the way back reuse the retained PVC instead of the operator defaults.
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let live = api.get(name).await.expect("instance fetch");
    let fs = live.spec.filestore.expect("filestore spec present");
    assert!(fs.empty_dir);
    assert_eq!(fs.storage_size.as_deref(), Some("1Gi"));
    assert_eq!(fs.storage_class.as_deref(), Some("standard"));

    // Flip back: the deployment re-mounts the PVC and the spec still names
    // the original class (the operator default would be the same string
    // here, so also assert the PVC object itself is the retained one).
    let pvc_uid_before = pvcs.get(&pvc_name).await.unwrap().metadata.uid;
    patch_instance_spec(c, ns, name, json!({ "filestore": { "emptyDir": false } })).await;
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let c = c.clone();
            let ns = ns.to_string();
            let deploy = name.to_string();
            async move {
                matches!(
                    filestore_volume_sources(&c, &ns, &deploy).await,
                    Some((false, true))
                )
            }
        })
        .await,
        "filestore volume should be the PVC again after flipping back"
    );
    let live = api.get(name).await.expect("instance fetch");
    let fs = live.spec.filestore.expect("filestore spec present");
    assert!(!fs.empty_dir);
    assert_eq!(fs.storage_size.as_deref(), Some("1Gi"));
    assert_eq!(fs.storage_class.as_deref(), Some("standard"));
    assert_eq!(
        pvcs.get(&pvc_name).await.unwrap().metadata.uid,
        pvc_uid_before,
        "flipping back must reuse the retained PVC, not provision a new one"
    );
}

/// A backup of an instance that was CREATED ephemeral (so no filestore PVC
/// has ever existed for it) must produce a schedulable Job: the filestore
/// volume is a job-local emptyDir, not a reference to a PVC that is not
/// there, and no pod affinity is applied (it only exists to co-locate with
/// a pod that has an RWO PVC mounted). Regression: the backup builder
/// bypasses the shared volume seam, so this is the path a flipped instance's
/// retained PVC would silently mask.
#[tokio::test]
async fn backup_of_created_ephemeral_instance_is_schedulable() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-emptydir-backup";

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst: OdooInstance = serde_json::from_value(ephemeral_instance_json(name, ns)).unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance");
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    let ready_handle = fast_track_to_running(&ctx, "test-emptydir-backup-init").await;

    // Precondition: the instance never had a PVC.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    assert!(
        pvcs.get(&format!("{name}-filestore-pvc")).await.is_err(),
        "a created-ephemeral instance must have no filestore PVC"
    );

    // Request a backup WITH the filestore: the operator forces it off and
    // the Job must still be schedulable.
    let backup_api: Api<OdooBackupJob> = Api::namespaced(c.clone(), ns);
    let backup_job: OdooBackupJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooBackupJob",
        "metadata": { "name": "test-emptydir-backup-job", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": name },
            "withFilestore": true,
            "destination": {
                "bucket": "test-bucket",
                "objectKey": "test-key",
                "endpoint": "http://localhost:9000",
            },
        }
    }))
    .unwrap();
    backup_api
        .create(&PostParams::default(), &backup_job)
        .await
        .expect("failed to create OdooBackupJob");
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::BackingUp).await,
        "expected BackingUp"
    );

    let k8s_job_name =
        wait_for_k8s_job_name::<OdooBackupJob>(c, ns, "test-emptydir-backup-job").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    let job = jobs.get(&k8s_job_name).await.expect("backup Job fetch");
    let pod_spec = job
        .spec
        .and_then(|s| s.template.spec)
        .expect("backup Job pod spec");

    let filestore = pod_spec
        .volumes
        .as_ref()
        .and_then(|vs| vs.iter().find(|v| v.name == "filestore"))
        .expect("backup Job declares a filestore volume");
    assert!(
        filestore.empty_dir.is_some() && filestore.persistent_volume_claim.is_none(),
        "backup filestore volume must be a job-local emptyDir, not a PVC reference"
    );
    assert!(
        pod_spec.affinity.is_none(),
        "no pod affinity for an ephemeral instance: there is no PVC to co-locate with"
    );
    let package_env = pod_spec
        .init_containers
        .as_ref()
        .and_then(|cs| cs.iter().find(|ic| ic.name == "package"))
        .and_then(|ic| ic.env.clone())
        .expect("package container env");
    assert!(
        package_env
            .iter()
            .any(|e| e.name == "BACKUP_WITH_FILESTORE" && e.value.as_deref() == Some("false")),
        "withFilestore must be forced off for an ephemeral instance"
    );

    fake_job_succeeded(c, ns, &k8s_job_name).await;
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Running).await,
        "expected Running after the backup completed"
    );
    ready_handle.abort();
}
