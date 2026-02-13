use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_init_job::OdooInitJob;
use odoo_operator::crd::odoo_instance::OdooInstancePhase;

/// Uninitialized → Initializing → Starting → Running
#[tokio::test]
async fn init_job_lifecycle() {
    let ctx = TestContext::new("test-init").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-init", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let init_api: Api<OdooInitJob> = Api::namespaced(c.clone(), ns);
    let init_job: OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": "test-init-job", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "test-init" },
            "modules": ["base"],
        }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &init_job)
        .await
        .expect("failed to create OdooInitJob");

    assert!(
        wait_for_phase(c, ns, "test-init", OdooInstancePhase::Initializing).await,
        "expected Initializing after init job created"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooInitJob>(c, ns, "test-init-job").await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-init", OdooInstancePhase::Starting).await,
        "expected Starting after init job completed"
    );

    let ready_handle = keep_deployment_ready(c.clone(), ns.into(), "test-init".into(), 1);

    assert!(
        wait_for_phase(c, ns, "test-init", OdooInstancePhase::Running).await,
        "expected Running after deployment ready"
    );

    ready_handle.abort();
}

/// Uninitialized → Initializing → InitFailed → (retry) Initializing → Starting
#[tokio::test]
async fn init_job_failure_and_retry() {
    let ctx = TestContext::new("test-initfail").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-initfail", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let init_api: Api<OdooInitJob> = Api::namespaced(c.clone(), ns);
    let init_job: OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": "test-initfail-job1", "namespace": ns },
        "spec": { "odooInstanceRef": { "name": "test-initfail" } }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &init_job)
        .await
        .unwrap();

    assert!(
        wait_for_phase(c, ns, "test-initfail", OdooInstancePhase::Initializing).await,
        "expected Initializing"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooInitJob>(c, ns, "test-initfail-job1").await;
    fake_job_failed(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, "test-initfail", OdooInstancePhase::InitFailed).await,
        "expected InitFailed after job failure"
    );

    // Retry with a second init job.
    let retry_job: OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": "test-initfail-job2", "namespace": ns },
        "spec": { "odooInstanceRef": { "name": "test-initfail" } }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &retry_job)
        .await
        .unwrap();

    assert!(
        wait_for_phase(c, ns, "test-initfail", OdooInstancePhase::Initializing).await,
        "expected Initializing on retry"
    );

    let k8s_job2 = wait_for_k8s_job_name::<OdooInitJob>(c, ns, "test-initfail-job2").await;
    fake_job_succeeded(c, ns, &k8s_job2).await;

    assert!(
        wait_for_phase(c, ns, "test-initfail", OdooInstancePhase::Starting).await,
        "expected Starting after retry succeeded"
    );
}
