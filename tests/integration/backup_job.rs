use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// Running → BackingUp → Running
#[tokio::test]
async fn backup_job_lifecycle() {
    let ctx = TestContext::new("test-backup").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-backup-init").await;

    // Create OdooBackupJob → Running → BackingUp.
    let backup_api: Api<OdooBackupJob> = Api::namespaced(c.clone(), ns);
    let backup_job: OdooBackupJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooBackupJob",
        "metadata": { "name": "test-backup-job", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "test-backup" },
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
        wait_for_phase(c, ns, "test-backup", OdooInstancePhase::BackingUp).await,
        "expected BackingUp after backup job created"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooBackupJob>(c, ns, "test-backup-job").await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-backup", OdooInstancePhase::Running).await,
        "expected Running after backup completed"
    );

    ready_handle.abort();
}

/// Two OdooBackupJobs queued at nearly the same time must not cause the
/// OdooInstance phase to flap BackingUp → Running → BackingUp as the first
/// one completes and the reconciler picks up the second.  Regression test
/// for the concurrent-backup phase-flap bug.
#[tokio::test]
async fn concurrent_backups_do_not_flap_phase() {
    let ctx = TestContext::new("test-backup-flap").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-backup-flap-init").await;

    let backup_api: Api<OdooBackupJob> = Api::namespaced(c.clone(), ns);
    for name in ["test-backup-flap-a", "test-backup-flap-b"] {
        let spec: OdooBackupJob = serde_json::from_value(json!({
            "apiVersion": "bemade.org/v1alpha1",
            "kind": "OdooBackupJob",
            "metadata": { "name": name, "namespace": ns },
            "spec": {
                "odooInstanceRef": { "name": "test-backup-flap" },
                "destination": {
                    "bucket": "test-bucket",
                    "objectKey": format!("test-key-{name}"),
                    "endpoint": "http://localhost:9000",
                },
            }
        }))
        .unwrap();
        backup_api
            .create(&PostParams::default(), &spec)
            .await
            .expect("failed to create OdooBackupJob");
    }

    assert!(
        wait_for_phase(c, ns, "test-backup-flap", OdooInstancePhase::BackingUp).await,
        "expected BackingUp after first backup job created"
    );

    // Complete the first K8s Job (whichever the snapshot picked first).
    let k8s_job_a = wait_for_k8s_job_name::<OdooBackupJob>(c, ns, "test-backup-flap-a").await;
    fake_job_succeeded(c, ns, &k8s_job_a).await;

    // Give the reconciler enough time to process the completion.  Without
    // the fix, the phase would transition BackingUp → Running at this point
    // and then flap back to BackingUp when the second CR is picked up.
    // With the fix, the phase must stay in BackingUp throughout.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let instances: Api<odoo_operator::crd::odoo_instance::OdooInstance> =
        Api::namespaced(c.clone(), ns);
    let inst = instances.get("test-backup-flap").await.unwrap();
    let phase = inst.status.and_then(|s| s.phase);
    assert_eq!(
        phase,
        Some(OdooInstancePhase::BackingUp),
        "phase must stay in BackingUp while another backup job is still pending (got {phase:?})"
    );

    // Now complete the second — instance should transition to Running.
    let k8s_job_b = wait_for_k8s_job_name::<OdooBackupJob>(c, ns, "test-backup-flap-b").await;
    fake_job_succeeded(c, ns, &k8s_job_b).await;

    assert!(
        wait_for_phase(c, ns, "test-backup-flap", OdooInstancePhase::Running).await,
        "expected Running after the last pending backup completes"
    );

    ready_handle.abort();
}
