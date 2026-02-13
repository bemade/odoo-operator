use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// Verify the shared envtest server boots and the controller starts reconciling.
#[tokio::test]
async fn envtest_boots_and_reconciles() {
    let ctx = TestContext::new("test-boot").await;
    assert!(
        wait_for_phase(
            &ctx.client,
            &ctx.ns,
            "test-boot",
            OdooInstancePhase::Uninitialized
        )
        .await,
        "expected Uninitialized"
    );
}
