use serde_json::json;

use kube::api::{Api, Patch, PatchParams};

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

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

/// The `scale` subresource must expose a label selector so a HorizontalPodAutoscaler
/// can discover the managed pods. The controller writes `status.selector =
/// "app=<name>"`, and the apiserver surfaces it on the `/scale` subresource via
/// the CRD's `labelSelectorPath`.
#[tokio::test]
async fn status_selector_is_populated_and_exposed_on_scale() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-sel").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-sel-init").await;

    // The controller sets status.selector during reconcile.
    assert!(
        wait_for(TIMEOUT, POLL, || async move {
            let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
            api.get("test-sel")
                .await
                .ok()
                .and_then(|i| i.status)
                .and_then(|s| s.selector)
                == Some("app=test-sel".to_string())
        })
        .await,
        "expected status.selector to be set to app=test-sel"
    );

    // The apiserver exposes it on the /scale subresource (what an HPA reads).
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let scale = instances.get_scale("test-sel").await?;
    assert_eq!(
        scale.status.and_then(|s| s.selector),
        Some("app=test-sel".to_string()),
        "scale subresource should report the label selector"
    );

    ready_handle.abort();
    Ok(())
}

/// An autoscaler scales by writing the `/scale` subresource, not the full spec.
/// A replica count written there must flow through `.spec.replicas` to the
/// Deployment — and without the old `.max(1)` floor, the count is honored
/// verbatim.
#[tokio::test]
async fn scale_subresource_write_updates_deployment() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-hpa").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-hpa-init").await;
    check_deployment_scale(c, ns, "test-hpa", 1).await?;

    // Simulate an HPA: write replicas via the scale subresource (not spec).
    ready_handle.abort();
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances
        .patch_scale(
            "test-hpa",
            &PatchParams::default(),
            &Patch::Merge(json!({ "spec": { "replicas": 4 } })),
        )
        .await?;

    assert!(
        wait_for(TIMEOUT, POLL, || async move {
            check_deployment_scale(c, ns, "test-hpa", 4).await.is_ok()
        })
        .await,
        "expected Deployment to scale to 4 after a /scale subresource write"
    );
    Ok(())
}
