use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;
use std::time::{Duration, Instant};

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstance;

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(200);

/// Wait for a Deployment to exist and return its spec.template.metadata.labels.
async fn wait_for_depl_pod_labels(
    client: &kube::Client,
    ns: &str,
    name: &str,
) -> std::collections::BTreeMap<String, String> {
    let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let start = Instant::now();
    loop {
        if let Ok(dep) = deps.get(name).await {
            if let Some(labels) = dep
                .spec
                .and_then(|s| s.template.metadata)
                .and_then(|m| m.labels)
            {
                return labels;
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "deployment {name} never got pod template labels"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Default environment is Staging; web + cron Deployments' pod templates
/// carry `bemade.org/environment=staging` and `bemade.org/instance=<name>`.
#[tokio::test]
async fn default_environment_is_staging() {
    let ctx = TestContext::new("env-default").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let web_labels = wait_for_depl_pod_labels(c, ns, "env-default").await;
    assert_eq!(
        web_labels.get("bemade.org/environment").map(String::as_str),
        Some("staging"),
        "web pod template must carry environment=staging by default"
    );
    assert_eq!(
        web_labels.get("bemade.org/instance").map(String::as_str),
        Some("env-default")
    );
    assert_eq!(
        web_labels.get("app").map(String::as_str),
        Some("env-default"),
        "existing app selector label must be preserved"
    );

    let cron_labels = wait_for_depl_pod_labels(c, ns, "env-default-cron").await;
    assert_eq!(
        cron_labels
            .get("bemade.org/environment")
            .map(String::as_str),
        Some("staging"),
        "cron pod template must carry environment=staging"
    );
}

/// Explicit environment: Production → label is `production`.  Patches the
/// created instance's spec.environment so we can reuse TestContext (which
/// creates a default-staging instance, leaving us to flip it).
#[tokio::test]
async fn production_environment_sets_production_label() {
    let ctx = TestContext::new("env-prod").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    // Flip to Production via a spec patch.  The reconcile loop will pick
    // up the change and roll the Deployment's pod template labels.
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances
        .patch(
            "env-prod",
            &PatchParams::apply("odoo-operator-test"),
            &Patch::Merge(&json!({
                "apiVersion": "bemade.org/v1alpha1",
                "kind": "OdooInstance",
                "spec": { "environment": "Production" }
            })),
        )
        .await
        .unwrap();

    // Poll until the pod template reflects the production label.
    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    let start = Instant::now();
    loop {
        let labels = deps
            .get("env-prod")
            .await
            .ok()
            .and_then(|d| d.spec)
            .and_then(|s| s.template.metadata)
            .and_then(|m| m.labels)
            .unwrap_or_default();
        if labels.get("bemade.org/environment").map(String::as_str) == Some("production") {
            return;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "deployment did not pick up environment=production (saw {labels:?})"
        );
        tokio::time::sleep(POLL).await;
    }
}
