//! Ephemeral (emptyDir) filestore — no PVC is provisioned, pods mount an
//! emptyDir under the same volume name, and a flip from a persistent
//! filestore RETAINS the existing PVC instead of deleting it.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::{Api, PostParams};
use serde_json::json;

use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

use super::common::{patch_instance_spec, wait_for, wait_for_phase, TestContext, POLL, TIMEOUT};

/// Fetch the named deployment's "filestore" volume, if the deployment and
/// the volume exist. Returns (has_empty_dir, has_pvc).
async fn filestore_volume_sources(
    c: &kube::Client,
    ns: &str,
    deploy: &str,
) -> Option<(bool, bool)> {
    let deployments: Api<Deployment> = Api::namespaced(c.clone(), ns);
    let d = deployments.get(deploy).await.ok()?;
    let vols = d
        .spec?
        .template
        .spec?
        .volumes?
        .into_iter()
        .find(|v| v.name == "filestore")?;
    Some((
        vols.empty_dir.is_some(),
        vols.persistent_volume_claim.is_some(),
    ))
}

/// An instance created with `filestore.emptyDir: true` gets no filestore
/// PVC, no injected storageSize/storageClass defaults, and both the web and
/// cron pod templates mount an emptyDir under the standard volume name.
#[tokio::test]
async fn empty_dir_instance_creates_no_pvc() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "test-emptydir";

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
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
            "filestore": { "emptyDir": true },
            "init": { "enabled": false },
        }
    }))
    .unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance");

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // Web and cron deployments mount an emptyDir, not a PVC.
    for deploy in [name.to_string(), format!("{name}-cron")] {
        assert!(
            wait_for(TIMEOUT, POLL, || {
                let c = c.clone();
                let deploy = deploy.clone();
                let ns = ns.to_string();
                async move {
                    matches!(
                        filestore_volume_sources(&c, &ns, &deploy).await,
                        Some((true, false))
                    )
                }
            })
            .await,
            "{deploy}: filestore volume should be an emptyDir with no PVC reference"
        );
    }

    // No filestore PVC was created.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    assert!(
        pvcs.get(&format!("{name}-filestore-pvc")).await.is_err(),
        "no filestore PVC should exist for an emptyDir instance"
    );

    // The defaulting pass must not have injected storageSize/storageClass.
    let live = api.get(name).await.expect("instance fetch");
    let fs = live.spec.filestore.expect("filestore spec present");
    assert!(fs.empty_dir);
    assert!(
        fs.storage_size.is_none() && fs.storage_class.is_none(),
        "size/class defaults must not be injected for an emptyDir filestore"
    );
}

/// Flipping a persistent instance to `emptyDir: true` switches the pod
/// volumes to emptyDir, strips the leftover storageSize/storageClass from
/// the spec, and RETAINS the existing PVC (never deletes it) so the flip is
/// reversible and a premature flip orphans data recoverably.
#[tokio::test]
async fn flip_to_empty_dir_retains_pvc() {
    let name = "test-emptydir-flip";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // The persistent-mode PVC exists.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let pvc_name = format!("{name}-filestore-pvc");
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let pvcs = pvcs.clone();
            let pvc_name = pvc_name.clone();
            async move { pvcs.get(&pvc_name).await.is_ok() }
        })
        .await,
        "filestore PVC should exist before the flip"
    );

    patch_instance_spec(c, ns, name, json!({ "filestore": { "emptyDir": true } })).await;

    // Deployment volume flips to emptyDir.
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let c = c.clone();
            let ns = ns.to_string();
            let deploy = name.to_string();
            async move {
                matches!(
                    filestore_volume_sources(&c, &ns, &deploy).await,
                    Some((true, false))
                )
            }
        })
        .await,
        "filestore volume should become an emptyDir after the flip"
    );

    // Leftover size/class from the persistent phase are stripped.
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = api.clone();
            let name = name.to_string();
            async move {
                match api.get(&name).await {
                    Ok(i) => i.spec.filestore.is_some_and(|f| {
                        f.empty_dir && f.storage_size.is_none() && f.storage_class.is_none()
                    }),
                    Err(_) => false,
                }
            }
        })
        .await,
        "leftover storageSize/storageClass should be stripped after the flip"
    );

    // The PVC is retained — this is the recoverability contract.
    assert!(
        pvcs.get(&pvc_name).await.is_ok(),
        "filestore PVC must be RETAINED after the flip to emptyDir"
    );
}
