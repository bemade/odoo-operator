use kube::api::Api;

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

#[tokio::test]
async fn finalizer_cleans_up_on_delete() {
    let ctx = TestContext::new("test-finalizer").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-finalizer", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get("test-finalizer").await.unwrap();
    let finalizers = inst.metadata.finalizers.unwrap_or_default();
    assert!(
        finalizers.iter().any(|f| f.contains("postgres-cleanup")),
        "expected postgres-cleanup finalizer, got: {finalizers:?}"
    );

    api.delete("test-finalizer", &Default::default())
        .await
        .expect("failed to delete instance");

    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = api.clone();
            async move { api.get("test-finalizer").await.is_err() }
        })
        .await,
        "instance should be deleted after finalizer runs"
    );
}
