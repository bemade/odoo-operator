use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// When imagePullSecret is set, the operator should copy the registry secret
/// from the operator namespace into the instance namespace.
#[tokio::test]
async fn image_pull_secret_copied_to_namespace() {
    let ctx = TestContext::new("test-pull").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    // Create a fake registry secret in the operator namespace ("default").
    let op_secrets: Api<Secret> = Api::namespaced(c.clone(), "default");
    let registry_secret: Secret = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": "test-registry", "namespace": "default" },
        "type": "kubernetes.io/dockerconfigjson",
        "stringData": {
            ".dockerconfigjson": r#"{"auths":{"registry.example.com":{"auth":"dGVzdDp0ZXN0"}}}"#
        }
    }))
    .unwrap();
    // Ignore AlreadyExists — another parallel test may have created it.
    let _ = op_secrets
        .create(&PostParams::default(), &registry_secret)
        .await;

    // Patch the OdooInstance to reference the pull secret.
    patch_instance_spec(
        c,
        ns,
        "test-pull",
        json!({ "imagePullSecret": "test-registry" }),
    )
    .await;

    // Wait for the operator to copy the secret into the test namespace and
    // verify the type in one shot to avoid a race with envtest shutdown.
    let ns_secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let ns_secrets = ns_secrets.clone();
            async move {
                match ns_secrets.get("test-registry").await {
                    Ok(s) => s.type_.as_deref() == Some("kubernetes.io/dockerconfigjson"),
                    Err(_) => false,
                }
            }
        })
        .await,
        "expected registry secret with correct type to be copied into instance namespace"
    );
}

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
    assert!(
        deps.get("test-child-cron").await.is_ok(),
        "cron deployment is missing"
    );
}
