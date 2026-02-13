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
