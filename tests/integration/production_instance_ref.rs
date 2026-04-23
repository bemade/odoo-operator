use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use odoo_operator::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;

/// Happy path: a staging `OdooInstance` with `spec.productionInstanceRef`
/// set makes the operator auto-create an `OdooStagingRefreshJob` whose
/// spec points at the referenced source, and the target transitions from
/// Uninitialized to CloningFromSource on the next reconcile.
///
/// Unlike `staging_refresh::staging_refresh_happy_path`, this test drives
/// the flow end-to-end from a single `OdooInstance` manifest — no
/// hand-created refresh CR.
#[tokio::test]
async fn auto_refresh_created_from_production_instance_ref() -> anyhow::Result<()> {
    // Source production instance up to Running (typical pre-condition
    // users will hit in the field — the source is a live prod).
    let ctx = TestContext::new("source-inst").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "source-init").await;

    // Staging target with productionInstanceRef. Init stays default
    // (enabled=true) — the operator should pick the refresh branch over
    // the init branch because productionInstanceRef is set.
    let target_name = "staging-inst";
    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": target_name, "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["staging.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "standard" },
            "environment": "Staging",
            "productionInstanceRef": { "name": "source-inst" },
        }
    }))
    .unwrap();
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances.create(&PostParams::default(), &target).await?;

    // The operator auto-creates "{target}-auto-refresh".
    let auto_refresh_name = format!("{target_name}-auto-refresh");
    let refresh_api: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = refresh_api.clone();
            let n = auto_refresh_name.clone();
            async move { api.get(&n).await.is_ok() }
        })
        .await,
        "operator never auto-created OdooStagingRefreshJob"
    );

    // Verify spec + owner-reference shape.
    let refresh = refresh_api.get(&auto_refresh_name).await?;
    assert_eq!(refresh.spec.odoo_instance_ref.name, target_name);
    assert_eq!(refresh.spec.source.instance_name, "source-inst");
    let orefs = refresh.metadata.owner_references.as_ref().unwrap();
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "OdooInstance");
    assert_eq!(orefs[0].name, target_name);
    assert_eq!(orefs[0].controller, Some(true));
    assert_eq!(
        refresh
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("bemade.org/auto-refresh"))
            .map(String::as_str),
        Some("true"),
        "auto-refresh marker label missing"
    );

    // No OdooInitJob should have been created — the refresh path
    // replaces the init path entirely.
    let init_name = format!("{target_name}-auto-init");
    let init_api: Api<odoo_operator::crd::odoo_init_job::OdooInitJob> =
        Api::namespaced(c.clone(), ns);
    assert!(
        init_api.get(&init_name).await.is_err(),
        "OdooInitJob {init_name} was created unexpectedly — the refresh \
         path should preempt the init path"
    );

    // Target transitions into CloningFromSource.
    assert!(
        wait_for_phase(c, ns, target_name, OdooInstancePhase::CloningFromSource).await,
        "expected target CloningFromSource after auto-created refresh"
    );

    source_ready.abort();
    Ok(())
}

/// Applying an OdooInstance with `environment: Production` AND
/// `productionInstanceRef` set must fail at admission time thanks to the
/// CRD CEL rule. The api-server returns an Invalid error; the reconciler
/// guard is belt-and-suspenders.
#[tokio::test]
async fn production_instance_ref_rejected_on_production() -> anyhow::Result<()> {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let bad: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "bad-prod", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["bad.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "standard" },
            "environment": "Production",
            "productionInstanceRef": { "name": "anywhere" },
        }
    }))
    .unwrap();

    let err = instances
        .create(&PostParams::default(), &bad)
        .await
        .expect_err("apply should have been rejected by CEL validation");
    let msg = format!("{err}");
    assert!(
        msg.contains("productionInstanceRef")
            || msg.contains("Production")
            || msg.contains("Invalid"),
        "unexpected rejection message: {msg}"
    );
    Ok(())
}
