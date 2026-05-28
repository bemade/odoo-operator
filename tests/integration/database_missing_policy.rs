//! Issue #119, part D — when the per-instance database is observed missing
//! while `status.dbInitialized == true`, the operator's reaction depends on
//! `spec.database.missingPolicy`:
//!
//!   * `Recreate` — auto-flip `dbInitialized` to false so the state machine
//!     drives back to Uninitialized and the auto-init path recreates the DB.
//!   * `Ignore` (default) — publish a Warning event but do not flip; humans
//!     decide whether to restore the DB or trigger a re-init.

use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_init_job::OdooInitJob;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

/// Create an OdooInstance with auto-init enabled, an explicit db name (so
/// the test doesn't need to derive it from the UID), and the requested
/// missing-policy value.
async fn create_instance(name: &str, ns: &str, client: &kube::Client, db_name: &str, policy: &str) {
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let inst: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["test.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": {
                "storageSize": "1Gi",
                "storageClass": "standard",
            },
            "database": {
                "name": db_name,
                "missingPolicy": policy,
            },
            "init": {
                "modules": ["base"],
            },
        }
    }))
    .unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance");
}

/// Drive a freshly-created instance through init → Running with
/// dbInitialized=true.
async fn run_to_running(c: &kube::Client, ns: &str, name: &str) -> tokio::task::JoinHandle<()> {
    let init_job_name = format!("{name}-auto-init");
    let k8s_job = wait_for_k8s_job_name::<OdooInitJob>(c, ns, &init_job_name).await;
    fake_job_succeeded(c, ns, &k8s_job).await;
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting after init job success"
    );
    fake_deployment_ready(c, ns, name, 1).await;
    let handle = keep_deployment_ready(c.clone(), ns.into(), name.into(), 1);
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Running).await,
        "expected Running"
    );
    handle
}

/// `missingPolicy: Recreate` — once the operator observes the DB is gone,
/// it must flip `status.dbInitialized` to false so the state machine
/// auto-triggers a fresh init job.
#[tokio::test]
async fn recreate_policy_flips_db_initialized_when_database_missing() -> anyhow::Result<()> {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-dbmissing-recreate";
    let db_name = "odoo_test_dbmissing_recreate";

    create_instance(name, ns, c, db_name, "Recreate").await;
    let _ready = run_to_running(c, ns, name).await;

    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    assert_eq!(
        instances
            .get_status(name)
            .await?
            .status
            .map(|s| s.db_initialized),
        Some(true),
        "precondition: dbInitialized should be true after init"
    );

    // Simulate the DB getting dropped out-of-band. In production a missing
    // DB causes the Odoo pods to crash, which fires deployment-status
    // watch events that wake the reconciler. envtest has no kubelet, so
    // we manually nudge the controller via `touch_instance`.
    mock_pg().set_database_exists(db_name, false);
    touch_instance(c, ns, name).await;

    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = instances.clone();
            async move {
                api.get_status(name)
                    .await
                    .ok()
                    .and_then(|i| i.status)
                    .map(|s| !s.db_initialized)
                    .unwrap_or(false)
            }
        })
        .await,
        "expected status.dbInitialized to flip to false under Recreate policy"
    );

    // Restore the mock so the next reconcile doesn't keep flipping (and
    // pollute the shared mock state if a later test reuses this db_name).
    mock_pg().set_database_exists(db_name, true);
    Ok(())
}

/// Default (`Ignore`) policy — the operator must NOT flip dbInitialized
/// when the DB goes missing. Recovery is operator-external.
#[tokio::test]
async fn ignore_policy_does_not_flip_db_initialized_when_database_missing() -> anyhow::Result<()> {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-dbmissing-ignore";
    let db_name = "odoo_test_dbmissing_ignore";

    create_instance(name, ns, c, db_name, "Ignore").await;
    let _ready = run_to_running(c, ns, name).await;

    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);

    mock_pg().set_database_exists(db_name, false);
    touch_instance(c, ns, name).await;

    // Poll for ~3s — long enough that a Recreate-style flip would have
    // fired. dbInitialized must remain true throughout.
    for _ in 0..6 {
        tokio::time::sleep(POLL).await;
        let still_initialized = instances
            .get_status(name)
            .await?
            .status
            .map(|s| s.db_initialized)
            .unwrap_or(false);
        assert!(
            still_initialized,
            "Ignore policy should not flip dbInitialized; saw it flip during wait"
        );
    }

    mock_pg().set_database_exists(db_name, true);
    Ok(())
}
