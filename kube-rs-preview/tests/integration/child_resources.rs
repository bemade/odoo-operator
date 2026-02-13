use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::Api;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

#[tokio::test]
async fn reconcile_creates_child_resources() {
    let ctx = TestContext::new("test-child").await;
    let c = &ctx.client;
    let ns = &ctx.ns;

    assert!(
        wait_for_phase(c, ns, "test-child", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    assert!(
        secrets.get("test-child-odoo-user").await.is_ok(),
        "odoo-user secret missing"
    );

    let cms: Api<ConfigMap> = Api::namespaced(c.clone(), ns);
    assert!(
        cms.get("test-child-odoo-conf").await.is_ok(),
        "odoo-conf configmap missing"
    );

    let svcs: Api<Service> = Api::namespaced(c.clone(), ns);
    assert!(svcs.get("test-child").await.is_ok(), "service missing");

    let ings: Api<Ingress> = Api::namespaced(c.clone(), ns);
    assert!(ings.get("test-child").await.is_ok(), "ingress missing");

    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    assert!(deps.get("test-child").await.is_ok(), "deployment missing");
}
