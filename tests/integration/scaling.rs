use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// While Running, changing spec.replicas should update the Deployment's replica
/// count without requiring a phase transition.
#[tokio::test]
async fn running_scales_deployment_on_replica_change() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-rscale").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    // Fast-track to Running with 1 replica.
    let ready_handle = fast_track_to_running(&ctx, "test-rscale-init").await;

    // Verify Deployment has 1 replica.
    check_deployment_scale(c, ns, "test-rscale", 1).await?;

    // Scale up to 3 while still Running.
    ready_handle.abort();
    patch_instance_spec(c, ns, "test-rscale", json!({ "replicas": 3 })).await;

    assert!(
        wait_for(TIMEOUT, POLL, || {
            async move {
                check_deployment_scale(c, ns, "test-rscale", 3)
                    .await
                    .is_ok()
            }
        })
        .await,
        "expected Deployment replicas to be updated to 3"
    );
    Ok(())
}

/// Running → Stopped → Starting
#[tokio::test]
async fn scale_down_and_up() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-scale").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-scale-init").await;

    // Scale to 0 → Running → Stopped.
    ready_handle.abort();
    patch_instance_spec(c, ns, "test-scale", json!({ "replicas": 0 })).await;

    assert!(
        wait_for_phase(c, ns, "test-scale", OdooInstancePhase::Stopped).await,
        "expected Stopped after scale to 0"
    );

    // Reset deployment readyReplicas to 0 so Starting doesn't immediately
    // transition to Running when we scale back up.
    fake_deployment_ready(c, ns, "test-scale", 0).await;

    // Scale back to 1 → Stopped → Starting.
    patch_instance_spec(c, ns, "test-scale", json!({ "replicas": 1 })).await;

    assert!(
        wait_for_phase(c, ns, "test-scale", OdooInstancePhase::Starting).await,
        "expected Starting after scale to 1"
    );
    Ok(())
}
