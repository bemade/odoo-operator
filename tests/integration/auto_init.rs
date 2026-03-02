use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_init_job::OdooInitJob;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

/// Creates an OdooInstance with spec.init set (bypasses TestContext::new which
/// uses a fixed JSON shape without the init field).
async fn create_auto_init_instance(name: &str, ns: &str, client: &kube::Client) {
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
            "init": {
                "modules": ["base"],
            },
        }
    }))
    .unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance with auto-init");
}

/// spec.init set → operator auto-creates OdooInitJob → Initializing → Starting → Running
#[tokio::test]
async fn auto_init_lifecycle() -> anyhow::Result<()> {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-autoinit";

    create_auto_init_instance(name, ns, c).await;

    // The operator should auto-create an OdooInitJob named "{name}-auto-init".
    // The instance passes through Uninitialized very quickly, so we check for
    // the init job CR rather than trying to catch the transient phase.
    let auto_init_name = format!("{name}-auto-init");
    let init_api: Api<OdooInitJob> = Api::namespaced(c.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = init_api.clone();
            let n = auto_init_name.clone();
            async move { api.get(&n).await.is_ok() }
        })
        .await,
        "auto-created OdooInitJob never appeared"
    );

    // Verify the auto-created job references the correct instance and modules.
    let init_job = init_api.get(&auto_init_name).await?;
    assert_eq!(init_job.spec.odoo_instance_ref.name, name);
    assert_eq!(init_job.spec.modules, vec!["base".to_string()]);

    // Verify owner reference points to the OdooInstance.
    let orefs = init_job.metadata.owner_references.as_ref().unwrap();
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "OdooInstance");
    assert_eq!(orefs[0].name, name);
    assert_eq!(orefs[0].controller, Some(true));

    // Normal init flow proceeds: Initializing → Starting → Running.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Initializing).await,
        "expected Initializing after auto-init job created"
    );

    let k8s_job = wait_for_k8s_job_name::<OdooInitJob>(c, ns, &auto_init_name).await;
    fake_job_succeeded(c, ns, &k8s_job).await;

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting after init job completed"
    );

    let ready_handle = keep_deployment_ready(c.clone(), ns.into(), name.into(), 1);

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Running).await,
        "expected Running after deployment ready"
    );

    ready_handle.abort();
    Ok(())
}
