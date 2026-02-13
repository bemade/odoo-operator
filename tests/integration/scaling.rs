use k8s_openapi::api::apps::v1::Deployment;
use kube::api::Api;
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// While Running, changing spec.replicas should update the Deployment's replica
/// count without requiring a phase transition.
#[tokio::test]
async fn running_scales_deployment_on_replica_change() {
    let ctx = TestContext::new("test-rscale").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    // Fast-track to Running with 1 replica.
    let ready_handle = fast_track_to_running(&ctx, "test-rscale-init").await;

    // Verify Deployment has 1 replica.
    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    let dep = deps.get("test-rscale").await.unwrap();
    assert_eq!(dep.spec.as_ref().unwrap().replicas, Some(1));

    // Scale up to 3 while still Running.
    ready_handle.abort();
    patch_instance_spec(c, ns, "test-rscale", json!({ "replicas": 3 })).await;

    // The controller should update the Deployment's replicas to 3
    // while staying in Running (or briefly transitioning through Degraded).
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let deps = deps.clone();
            async move {
                deps.get("test-rscale")
                    .await
                    .ok()
                    .and_then(|d| d.spec.and_then(|s| s.replicas))
                    == Some(3)
            }
        })
        .await,
        "expected Deployment replicas to be updated to 3"
    );
}

/// Running → Stopped → Starting
#[tokio::test]
async fn scale_down_and_up() {
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
}
