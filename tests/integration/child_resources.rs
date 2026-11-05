use gateway_api::apis::standard::httproutes::HTTPRoute;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service};
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

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

    assert!(
        secrets.get("test-child-odoo-conf").await.is_ok(),
        "odoo-conf secret missing"
    );

    let svcs: Api<Service> = Api::namespaced(c.clone(), ns);
    assert!(svcs.get("test-child").await.is_ok(), "service missing");

    let ings: Api<Ingress> = Api::namespaced(c.clone(), ns);
    assert!(ings.get("test-child").await.is_ok(), "ingress missing");

    // No gatewayRef set → no HTTPRoute should exist.
    let routes: Api<HTTPRoute> = Api::namespaced(c.clone(), ns);
    assert!(
        routes.get("test-child").await.is_err(),
        "HTTPRoute should not exist when gatewayRef is absent"
    );

    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    assert!(deps.get("test-child").await.is_ok(), "deployment missing");
    assert!(
        deps.get("test-child-cron").await.is_ok(),
        "cron deployment is missing"
    );
}

/// When a FUSE mount fails, the CSI driver remounts it on the node. Without
/// propagation running containers keep the dead mount until deleted, so both
/// long-running Deployments need it on the filestore (#220). Envtest has no
/// kubelet, so this only guards the rendered spec; the recovery itself needs a
/// real cluster.
#[tokio::test]
async fn deployments_propagate_filestore_mount_changes_from_the_host() {
    let ctx = TestContext::new("test-propagation").await;
    let c = &ctx.client;
    let ns = &ctx.ns;

    assert!(
        wait_for_phase(c, ns, "test-propagation", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    for name in ["test-propagation", "test-propagation-cron"] {
        let dep = deps.get(name).await.expect("deployment missing");
        let mounts = dep
            .spec
            .and_then(|s| s.template.spec)
            .map(|p| p.containers[0].volume_mounts.clone().unwrap_or_default())
            .expect("deployment has no pod spec");
        let propagation = |volume: &str| {
            mounts
                .iter()
                .find(|m| m.name == volume)
                .unwrap_or_else(|| panic!("{name} has no {volume} mount"))
                .mount_propagation
                .clone()
        };
        assert_eq!(
            propagation("filestore").as_deref(),
            Some("HostToContainer"),
            "{name} filestore mount"
        );
        assert_eq!(propagation("odoo-conf"), None, "{name} odoo-conf mount");
    }
}

/// When gatewayRef is set, the operator should create an HTTPRoute and no Ingress.
#[tokio::test]
async fn reconcile_creates_http_route_when_gateway_ref_set() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    // Create an OdooInstance with gatewayRef set.
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "test-gw", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["gw.example.com"],
                "gatewayRef": {
                    "name": "my-gateway",
                    "namespace": "istio-system"
                }
            },
            "filestore": {
                "storageSize": "1Gi",
                "storageClass": "standard"
            },
            "init": { "enabled": false }
        }
    }))
    .unwrap();
    api.create(&PostParams::default(), &inst)
        .await
        .expect("failed to create OdooInstance with gatewayRef");

    assert!(
        wait_for_phase(c, ns, "test-gw", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // HTTPRoute should exist.
    let routes: Api<HTTPRoute> = Api::namespaced(c.clone(), ns);
    assert!(
        routes.get("test-gw").await.is_ok(),
        "HTTPRoute missing when gatewayRef is set"
    );

    // Ingress should NOT exist.
    let ings: Api<Ingress> = Api::namespaced(c.clone(), ns);
    assert!(
        ings.get("test-gw").await.is_err(),
        "Ingress should not exist when gatewayRef is set"
    );
}

/// envtest has no PV controller and no StorageClasses, so PVCs never bind and
/// the API server rejects resize attempts. Create an expansion-enabled
/// StorageClass and fake the PVC into a bound state.
async fn fake_pvc_bound(c: &kube::Client, ns: &str, name: &str, capacity: &str) {
    use k8s_openapi::api::storage::v1::StorageClass;
    let scs: Api<StorageClass> = Api::all(c.clone());
    let sc: StorageClass = serde_json::from_value(json!({
        "apiVersion": "storage.k8s.io/v1",
        "kind": "StorageClass",
        "metadata": { "name": "standard" },
        "provisioner": "k8s.io/test-provisioner",
        "allowVolumeExpansion": true,
    }))
    .unwrap();
    let _ = scs.create(&PostParams::default(), &sc).await;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let spec_patch = json!({ "spec": { "volumeName": format!("pv-{name}") } });
    pvcs.patch(name, &PatchParams::default(), &Patch::Merge(&spec_patch))
        .await
        .expect("failed to set volumeName");
    let status_patch = json!({
        "status": { "phase": "Bound", "capacity": { "storage": capacity } }
    });
    pvcs.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch))
        .await
        .expect("failed to patch PVC status");
}

/// Increasing spec.filestore.storageSize should expand the existing PVC's
/// storage request (PVC expansion in place — no migration phase).
#[tokio::test]
async fn pvc_storage_size_expands_in_place() {
    let ctx = TestContext::new("test-pvc-grow").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-pvc-grow", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let pvc_name = "test-pvc-grow-filestore-pvc";

    // Initial size from TestContext default — capture it.
    let initial = pvcs
        .get(pvc_name)
        .await
        .expect("filestore PVC missing")
        .spec
        .and_then(|s| s.resources)
        .and_then(|r| r.requests)
        .and_then(|m| m.get("storage").cloned())
        .expect("PVC has no storage request")
        .0;
    assert_ne!(initial, "10Gi", "test relies on initial size != 10Gi");

    // The API server only allows resizing bound PVCs. envtest has no
    // PV controller, so simulate binding by setting volumeName + status.
    fake_pvc_bound(c, ns, pvc_name, &initial).await;

    // Bump the spec to 10Gi.
    patch_instance_spec(
        c,
        ns,
        "test-pvc-grow",
        json!({ "filestore": { "storageSize": "10Gi" } }),
    )
    .await;

    // Wait for the operator to patch the PVC.
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let pvcs = pvcs.clone();
            async move {
                match pvcs.get(pvc_name).await {
                    Ok(pvc) => pvc
                        .spec
                        .and_then(|s| s.resources)
                        .and_then(|r| r.requests)
                        .and_then(|m| m.get("storage").cloned())
                        .map(|q| q.0 == "10Gi")
                        .unwrap_or(false),
                    Err(_) => false,
                }
            }
        })
        .await,
        "expected filestore PVC storage request to be expanded to 10Gi"
    );
}

/// Decreasing the storage size should be a no-op on the PVC (the webhook would
/// normally reject this, but in-process tests bypass the webhook). Verifies the
/// operator does not attempt to shrink.
#[tokio::test]
async fn pvc_storage_size_shrink_is_noop() {
    let ctx = TestContext::new("test-pvc-shrink").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    assert!(
        wait_for_phase(c, ns, "test-pvc-shrink", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let pvc_name = "test-pvc-shrink-filestore-pvc";
    let initial = pvcs
        .get(pvc_name)
        .await
        .expect("filestore PVC missing")
        .spec
        .and_then(|s| s.resources)
        .and_then(|r| r.requests)
        .and_then(|m| m.get("storage").cloned())
        .expect("PVC has no storage request")
        .0;

    fake_pvc_bound(c, ns, pvc_name, &initial).await;

    patch_instance_spec(
        c,
        ns,
        "test-pvc-shrink",
        json!({ "filestore": { "storageSize": "1Mi" } }),
    )
    .await;

    // Give the operator several reconciles to (not) act, then assert unchanged.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let after = pvcs
        .get(pvc_name)
        .await
        .unwrap()
        .spec
        .and_then(|s| s.resources)
        .and_then(|r| r.requests)
        .and_then(|m| m.get("storage").cloned())
        .unwrap()
        .0;
    assert_eq!(after, initial, "PVC should not shrink");
}

// ── odoo.conf lives in a Secret (#131) ────────────────────────────────────────

/// Name of the volume carrying odoo.conf, and the Secret/ConfigMap behind it.
fn conf_volume(dep: &Deployment) -> k8s_openapi::api::core::v1::Volume {
    dep.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|p| p.volumes.as_ref())
        .and_then(|v| v.iter().find(|v| v.name == "odoo-conf").cloned())
        .expect("deployment has no odoo-conf volume")
}

/// The rendered odoo.conf carries `db_password` and `admin_passwd`, so it
/// must be a Secret: a ConfigMap is readable by anyone with configmap `get`.
/// Both Deployments must mount that Secret, and no ConfigMap in the namespace
/// may carry either credential.
#[tokio::test]
async fn odoo_conf_is_rendered_into_an_owned_secret() {
    let ctx = TestContext::new("conf-sec").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    assert!(
        wait_for_phase(c, ns, "conf-sec", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    let conf = secrets
        .get("conf-sec-odoo-conf")
        .await
        .expect("odoo-conf Secret missing");
    let data = conf.data.clone().unwrap_or_default();
    let rendered = String::from_utf8(data["odoo.conf"].0.clone()).unwrap();
    assert!(rendered.contains("db_password = "), "{rendered}");
    assert!(rendered.contains("admin_passwd = admin\n"), "{rendered}");
    for key in ["db_host", "db_port", "db_name", "db_user", "db_password"] {
        assert!(data.contains_key(key), "conf Secret lacks {key}");
    }

    // Owned by the instance, so it is garbage-collected with it.
    let inst: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let uid = inst.get("conf-sec").await.unwrap().metadata.uid.unwrap();
    let orefs = conf.metadata.owner_references.unwrap_or_default();
    assert!(
        orefs
            .iter()
            .any(|o| o.uid == uid && o.controller == Some(true)),
        "conf Secret must be controller-owned by the instance: {orefs:?}"
    );

    let cms: Api<ConfigMap> = Api::namespaced(c.clone(), ns);
    for cm in cms.list(&Default::default()).await.unwrap() {
        for (k, v) in cm.data.unwrap_or_default() {
            assert!(
                !v.contains("db_password") && !v.contains("admin_passwd") && k != "db_password",
                "ConfigMap {} carries a credential in key {k}",
                cm.metadata.name.unwrap_or_default()
            );
        }
    }

    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    for name in ["conf-sec", "conf-sec-cron"] {
        let vol = conf_volume(&deps.get(name).await.unwrap());
        assert!(vol.config_map.is_none(), "{name} still mounts a ConfigMap");
        assert_eq!(
            vol.secret.and_then(|s| s.secret_name).as_deref(),
            Some("conf-sec-odoo-conf"),
            "{name} must mount the conf Secret"
        );
    }
}

/// Upgrade path from an operator that rendered odoo.conf into a ConfigMap.
///
/// The legacy ConfigMap must not outlive the move to the Secret (it holds
/// both credentials in plaintext), but it must not vanish while anything still
/// needs it either: a pod that has not started yet would hang on a missing
/// configMap volume. Here an in-flight Job left behind by the old operator
/// still references it, so the ConfigMap stays until that Job finishes.
#[tokio::test]
async fn legacy_conf_configmap_removed_once_nothing_references_it() {
    use k8s_openapi::api::batch::v1::Job;
    use kube::api::DeleteParams;
    use odoo_operator::controller::helpers::FIELD_MANAGER;

    let ctx = TestContext::new("legacy-conf").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let name = "legacy-conf";
    let cm_name = "legacy-conf-odoo-conf";
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    let inst: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let uid = inst.get(name).await.unwrap().metadata.uid.unwrap();

    // What the previous operator version left behind: the owned ConfigMap…
    let cms: Api<ConfigMap> = Api::namespaced(c.clone(), ns);
    let legacy: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {
            "name": cm_name,
            "ownerReferences": [{
                "apiVersion": "bemade.org/v1alpha1", "kind": "OdooInstance",
                "name": name, "uid": uid, "controller": true,
                "blockOwnerDeletion": true,
            }],
        },
        "data": { "odoo.conf": "[options]\ndb_password = x\n", "db_password": "x" },
    }))
    .unwrap();
    cms.create(&PostParams::default(), &legacy).await.unwrap();

    // …a backup Job still running, reading credentials from it…
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    let job: Job = serde_json::from_value(json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": "old-backup" },
        "spec": { "template": { "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "dump", "image": "postgres:17",
                "env": [{ "name": "PASSWORD", "valueFrom": {
                    "configMapKeyRef": { "name": cm_name, "key": "db_password" } } }],
            }],
        } } },
    }))
    .unwrap();
    jobs.create(&PostParams::default(), &job).await.unwrap();

    // …and Deployments whose last apply mounted the ConfigMap. Re-applied as
    // the operator's own field manager, exactly as the old version wrote them.
    let deps: Api<Deployment> = Api::namespaced(c.clone(), ns);
    for dname in [name, "legacy-conf-cron"] {
        let mut v = serde_json::to_value(deps.get(dname).await.unwrap()).unwrap();
        let meta = v["metadata"].as_object_mut().unwrap();
        for k in [
            "managedFields",
            "resourceVersion",
            "uid",
            "creationTimestamp",
            "generation",
        ] {
            meta.remove(k);
        }
        v.as_object_mut().unwrap().remove("status");
        for vol in v["spec"]["template"]["spec"]["volumes"]
            .as_array_mut()
            .unwrap()
        {
            if vol["name"] == "odoo-conf" {
                *vol = json!({ "name": "odoo-conf", "configMap": { "name": cm_name } });
            }
        }
        // Check the apply's own response: the operator reacts to the
        // Deployment event and may already have re-applied by a later GET.
        let applied = deps
            .patch(
                dname,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&v),
            )
            .await
            .unwrap();
        let vol = conf_volume(&applied);
        assert!(vol.config_map.is_some() && vol.secret.is_none());
    }

    // The operator moves both Deployments onto the Secret…
    touch_instance(c, ns, name).await;
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let deps = deps.clone();
            async move {
                let mut ok = true;
                for d in ["legacy-conf", "legacy-conf-cron"] {
                    let vol = conf_volume(&deps.get(d).await.unwrap());
                    ok &= vol.config_map.is_none() && vol.secret.is_some();
                }
                ok
            }
        })
        .await,
        "deployments never moved onto the conf Secret"
    );

    // …but keeps the ConfigMap while the old Job may still start a pod on it.
    touch_instance(c, ns, name).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        cms.get_opt(cm_name).await.unwrap().is_some(),
        "legacy ConfigMap deleted while an unfinished Job still references it"
    );

    // Once that Job is done, the ConfigMap goes.
    fake_job_succeeded(c, ns, "old-backup").await;
    touch_instance(c, ns, name).await;
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let cms = cms.clone();
            async move { cms.get_opt(cm_name).await.unwrap().is_none() }
        })
        .await,
        "legacy ConfigMap never removed after the last reference went away"
    );
    let _ = jobs.delete("old-backup", &DeleteParams::default()).await;
}

/// A same-named ConfigMap the instance does not own is somebody else's
/// object: the migration must never delete it.
#[tokio::test]
async fn unowned_conf_named_configmap_is_left_alone() {
    let ctx = TestContext::new("foreign-cm").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    assert!(
        wait_for_phase(c, ns, "foreign-cm", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    let cms: Api<ConfigMap> = Api::namespaced(c.clone(), ns);
    let cm: ConfigMap = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": "foreign-cm-odoo-conf" },
        "data": { "note": "not the operator's" },
    }))
    .unwrap();
    cms.create(&PostParams::default(), &cm).await.unwrap();
    touch_instance(c, ns, "foreign-cm").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(cms.get_opt("foreign-cm-odoo-conf").await.unwrap().is_some());
}

/// Every Job pod reads its database credentials from the conf Secret by
/// `secretKeyRef` and mounts nothing from a ConfigMap: the init Job through
/// the standard volume set, the backup Job through injected env vars.
#[tokio::test]
async fn job_pods_read_credentials_from_the_conf_secret() {
    use k8s_openapi::api::batch::v1::Job;
    use odoo_operator::controller::helpers::pod_spec_references_configmap;
    use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
    use odoo_operator::crd::odoo_init_job::OdooInitJob;

    let ctx = TestContext::new("job-creds").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let conf = "job-creds-odoo-conf";
    let ready = fast_track_to_running(&ctx, "job-creds-init").await;

    let backups: Api<OdooBackupJob> = Api::namespaced(c.clone(), ns);
    let bj: OdooBackupJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1", "kind": "OdooBackupJob",
        "metadata": { "name": "job-creds-backup" },
        "spec": {
            "odooInstanceRef": { "name": "job-creds" },
            "destination": { "bucket": "b", "objectKey": "k", "endpoint": "http://localhost:9000" },
        }
    }))
    .unwrap();
    backups.create(&PostParams::default(), &bj).await.unwrap();
    let backup = wait_for_k8s_job_name::<OdooBackupJob>(c, ns, "job-creds-backup").await;
    let init = wait_for_k8s_job_name::<OdooInitJob>(c, ns, "job-creds-init").await;

    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    for jname in [&init, &backup] {
        let spec = jobs
            .get(jname)
            .await
            .unwrap()
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        assert!(
            !pod_spec_references_configmap(&spec, conf),
            "Job {jname} still reads from the {conf} ConfigMap"
        );
    }
    let init_spec = jobs
        .get(&init)
        .await
        .unwrap()
        .spec
        .unwrap()
        .template
        .spec
        .unwrap();
    assert!(
        init_spec
            .volumes
            .unwrap_or_default()
            .iter()
            .any(|v| v.name == "odoo-conf"
                && v.secret.as_ref().and_then(|s| s.secret_name.as_deref()) == Some(conf)),
        "init Job must mount odoo.conf from the Secret"
    );
    let backup_spec = jobs
        .get(&backup)
        .await
        .unwrap()
        .spec
        .unwrap()
        .template
        .spec
        .unwrap();
    let pw = backup_spec
        .containers
        .iter()
        .chain(backup_spec.init_containers.iter().flatten())
        .flat_map(|c| c.env.iter().flatten())
        .find(|e| e.name == "PASSWORD")
        .expect("backup Job has no PASSWORD env var");
    let r = pw
        .value_from
        .as_ref()
        .and_then(|v| v.secret_key_ref.as_ref());
    assert_eq!(
        r.map(|r| (r.name.as_str(), r.key.as_str())),
        Some((conf, "db_password"))
    );

    fake_job_succeeded(c, ns, &backup).await;
    ready.abort();
}
