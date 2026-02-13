use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;
use odoo_operator::crd::odoo_upgrade_job::OdooUpgradeJob;

/// Running → Upgrading → Starting
#[tokio::test]
async fn upgrade_job_lifecycle() {
    let ctx = TestContext::new("test-upgrade").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-upgrade-init").await;

    // Stop faking readyReplicas and reset to 0 so the controller doesn't
    // race through Starting → Running after the upgrade completes.
    ready_handle.abort();
    fake_deployment_ready(c, ns, "test-upgrade", 0).await;

    // Create OdooUpgradeJob → Running → Upgrading.
    let upgrade_api: Api<OdooUpgradeJob> = Api::namespaced(c.clone(), ns);
    let upgrade_job: OdooUpgradeJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooUpgradeJob",
        "metadata": { "name": "test-upgrade-job", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "test-upgrade" },
            "modules": ["base"],
        }
    }))
    .unwrap();
    upgrade_api
        .create(&PostParams::default(), &upgrade_job)
        .await
        .expect("failed to create OdooUpgradeJob");

    assert!(
        wait_for_phase(c, ns, "test-upgrade", OdooInstancePhase::Upgrading).await,
        "expected Upgrading after upgrade job created"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooUpgradeJob>(c, ns, "test-upgrade-job").await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-upgrade", OdooInstancePhase::Starting).await,
        "expected Starting after upgrade completed"
    );
}
