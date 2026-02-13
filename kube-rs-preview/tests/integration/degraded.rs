use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// Running (2 replicas) → Degraded (1 ready) → Running (2 ready)
#[tokio::test]
async fn degraded_and_recovery() {
    let ctx = TestContext::new_with_replicas("test-degraded", 2).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let ready_handle = fast_track_to_running(&ctx, "test-degraded-init").await;

    // Drop to 1 ready replica → Running → Degraded.
    ready_handle.abort();
    fake_deployment_ready(c, ns, "test-degraded", 1).await;
    let degraded_handle = keep_deployment_ready(c.clone(), ns.into(), "test-degraded".into(), 1);

    assert!(
        wait_for_phase(c, ns, "test-degraded", OdooInstancePhase::Degraded).await,
        "expected Degraded with 1/2 replicas"
    );

    // Recover to 2 ready replicas → Degraded → Running.
    degraded_handle.abort();
    fake_deployment_ready(c, ns, "test-degraded", 2).await;
    let recovery_handle = keep_deployment_ready(c.clone(), ns.into(), "test-degraded".into(), 2);

    assert!(
        wait_for_phase(c, ns, "test-degraded", OdooInstancePhase::Running).await,
        "expected Running after recovery"
    );

    recovery_handle.abort();
}
