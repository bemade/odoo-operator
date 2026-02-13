use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;
use odoo_operator::crd::odoo_restore_job::OdooRestoreJob;

/// Uninitialized → Restoring → Starting → Running
#[tokio::test]
async fn restore_job_lifecycle() {
    let ctx = TestContext::new("test-restore").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-restore", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let restore_api: Api<OdooRestoreJob> = Api::namespaced(c.clone(), ns);
    let restore_job: OdooRestoreJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooRestoreJob",
        "metadata": { "name": "test-restore-job", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "test-restore" },
            "source": {
                "type": "s3",
                "s3": {
                    "bucket": "test-bucket",
                    "objectKey": "test-key",
                    "endpoint": "http://localhost:9000",
                },
            },
        }
    }))
    .unwrap();
    restore_api
        .create(&PostParams::default(), &restore_job)
        .await
        .expect("failed to create OdooRestoreJob");

    assert!(
        wait_for_phase(c, ns, "test-restore", OdooInstancePhase::Restoring).await,
        "expected Restoring after restore job created"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooRestoreJob>(c, ns, "test-restore-job").await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-restore", OdooInstancePhase::Starting).await,
        "expected Starting after restore completed"
    );

    let ready_handle = keep_deployment_ready(c.clone(), ns.into(), "test-restore".into(), 1);

    assert!(
        wait_for_phase(c, ns, "test-restore", OdooInstancePhase::Running).await,
        "expected Running after deployment ready"
    );

    ready_handle.abort();
}
