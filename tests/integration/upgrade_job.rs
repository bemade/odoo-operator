use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;
use odoo_operator::crd::odoo_upgrade_job::OdooUpgradeJob;

/// Running → Upgrading → Starting
#[tokio::test]
async fn upgrade_job_lifecycle() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-upgrade").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-upgrade-init").await;

    // Stop faking readyReplicas and reset to 0 so the controller doesn't
    // race through Starting → Running after the upgrade completes.
    ready_handle.abort();
    fake_deployment_ready(c, ns, "test-upgrade", 0).await;

    // Get the main deployment's resource_version to later check if redeployed
    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    let before = deps
        .get("test-upgrade")
        .await?
        .metadata
        .resource_version
        .unwrap();
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

    // Make sure that the cron deployment is scaled down and the main deployment is still running.
    assert!(
        wait_for(TIMEOUT, POLL, || async {
            check_deployment_scale(c, ns, "test-upgrade-cron", 0)
                .await
                .is_ok()
        })
        .await,
        "expected cron deployment to be scaled down during upgrade"
    );
    check_deployment_scale(c, ns, "test-upgrade", 1).await?;

    let k8s_job = wait_for_k8s_job_name::<OdooUpgradeJob>(c, ns, "test-upgrade-job").await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-upgrade", OdooInstancePhase::Starting).await,
        "expected Starting after upgrade completed"
    );

    // Make sure the cron deployment got scaled back up after upgrade.
    assert!(
        wait_for(TIMEOUT, POLL, || async {
            check_deployment_scale(c, ns, "test-upgrade-cron", 1)
                .await
                .is_ok()
        })
        .await,
        "expected cron deployment to be scaled back up after upgrade"
    );
    check_deployment_scale(c, ns, "test-upgrade", 1).await?;

    // Make sure the main deployment got restarted
    assert!(
        deps.get("test-upgrade")
            .await?
            .metadata
            .resource_version
            .unwrap()
            != before,
        "Expected the main deployment to be redeployed after an upgrade."
    );

    Ok(())
}
