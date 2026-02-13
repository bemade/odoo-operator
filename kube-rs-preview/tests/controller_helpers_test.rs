//! Unit tests for controller helper functions.

use kube::api::ObjectMeta;

use odoo_operator::controller::helpers::*;
use odoo_operator::crd::odoo_init_job::{OdooInitJob, OdooInitJobSpec};
use odoo_operator::crd::odoo_instance::{IngressSpec, OdooInstance, OdooInstanceSpec};
use odoo_operator::crd::shared::OdooInstanceRef;

/// Build a minimal OdooInstance for testing.
fn test_instance(name: &str, pull_secret: Option<&str>) -> OdooInstance {
    OdooInstance {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("default".to_string()),
            uid: Some("test-uid-1234".to_string()),
            ..Default::default()
        },
        spec: OdooInstanceSpec {
            image: None,
            image_pull_secret: pull_secret.map(|s| s.to_string()),
            admin_password: "admin".to_string(),
            replicas: 1,
            ingress: IngressSpec {
                hosts: vec!["test.example.com".to_string()],
                issuer: None,
                class: None,
            },
            resources: None,
            filestore: None,
            config_options: None,
            database: None,
            strategy: None,
            webhook: None,
            probes: None,
            affinity: None,
            tolerations: vec![],
        },
        status: None,
    }
}

/// Build a minimal OdooInitJob for testing owner references.
fn test_init_job(name: &str) -> OdooInitJob {
    OdooInitJob {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("default".to_string()),
            uid: Some("init-uid-5678".to_string()),
            ..Default::default()
        },
        spec: OdooInitJobSpec {
            odoo_instance_ref: OdooInstanceRef {
                name: "my-odoo".to_string(),
                namespace: None,
            },
            modules: vec!["base".to_string()],
            webhook: None,
        },
        status: None,
    }
}

// ── controller_owner_ref ────────────────────────────────────────────────────

#[test]
fn test_controller_owner_ref_sets_api_version_and_kind() {
    let job = test_init_job("my-init");
    let oref = controller_owner_ref(&job);

    assert_eq!(oref.api_version, "bemade.org/v1alpha1");
    assert_eq!(oref.kind, "OdooInitJob");
    assert_eq!(oref.name, "my-init");
    assert_eq!(oref.uid, "init-uid-5678");
    assert_eq!(oref.controller, Some(true));
    assert_eq!(oref.block_owner_deletion, Some(true));
}

#[test]
fn test_controller_owner_ref_works_for_odoo_instance() {
    let inst = test_instance("my-odoo", None);
    let oref = controller_owner_ref(&inst);

    assert_eq!(oref.kind, "OdooInstance");
    assert_eq!(oref.name, "my-odoo");
    assert_eq!(oref.uid, "test-uid-1234");
}

#[test]
fn test_controller_owner_ref_missing_uid_defaults_to_empty() {
    let mut job = test_init_job("no-uid");
    job.metadata.uid = None;
    let oref = controller_owner_ref(&job);

    assert_eq!(oref.uid, "");
}

// ── odoo_security_context ───────────────────────────────────────────────────

#[test]
fn test_odoo_security_context_values() {
    let ctx = odoo_security_context();
    assert_eq!(ctx.run_as_user, Some(100));
    assert_eq!(ctx.run_as_group, Some(101));
    assert_eq!(ctx.fs_group, Some(101));
}

// ── odoo_volumes ────────────────────────────────────────────────────────────

#[test]
fn test_odoo_volumes_names_and_sources() {
    let vols = odoo_volumes("my-odoo");
    assert_eq!(vols.len(), 2);

    // Filestore PVC volume
    assert_eq!(vols[0].name, "filestore");
    let pvc = vols[0].persistent_volume_claim.as_ref().unwrap();
    assert_eq!(pvc.claim_name, "my-odoo-filestore-pvc");

    // ConfigMap volume
    assert_eq!(vols[1].name, "odoo-conf");
    let cm = vols[1].config_map.as_ref().unwrap();
    assert_eq!(cm.name, "my-odoo-odoo-conf");
}

// ── odoo_volume_mounts ──────────────────────────────────────────────────────

#[test]
fn test_odoo_volume_mounts_paths() {
    let mounts = odoo_volume_mounts();
    assert_eq!(mounts.len(), 2);

    assert_eq!(mounts[0].name, "filestore");
    assert_eq!(mounts[0].mount_path, "/var/lib/odoo");

    assert_eq!(mounts[1].name, "odoo-conf");
    assert_eq!(mounts[1].mount_path, "/etc/odoo");
}

// ── image_pull_secrets ──────────────────────────────────────────────────────

#[test]
fn test_image_pull_secrets_none_when_not_configured() {
    let inst = test_instance("my-odoo", None);
    assert!(image_pull_secrets(&inst).is_none());
}

#[test]
fn test_image_pull_secrets_returns_list_when_configured() {
    let inst = test_instance("my-odoo", Some("my-registry-secret"));
    let secrets = image_pull_secrets(&inst).unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].name, "my-registry-secret");
}

// ── env ─────────────────────────────────────────────────────────────────────

#[test]
fn test_env_creates_plain_value_envvar() {
    let e = env("MY_VAR", "my_value");
    assert_eq!(e.name, "MY_VAR");
    assert_eq!(e.value, Some("my_value".to_string()));
    assert!(e.value_from.is_none());
}

#[test]
fn test_env_accepts_string_and_str() {
    let from_str = env("A", "val");
    let from_string = env("A", String::from("val"));
    assert_eq!(from_str.value, from_string.value);
}

// ── cm_env ──────────────────────────────────────────────────────────────────

#[test]
fn test_cm_env_creates_configmap_ref_envvar() {
    let e = cm_env("HOST", "my-odoo-odoo-conf", "db_host");
    assert_eq!(e.name, "HOST");
    assert!(e.value.is_none());

    let vf = e.value_from.unwrap();
    let cm_ref = vf.config_map_key_ref.unwrap();
    assert_eq!(cm_ref.name, "my-odoo-odoo-conf");
    assert_eq!(cm_ref.key, "db_host");
}

// ── FIELD_MANAGER ───────────────────────────────────────────────────────────

#[test]
fn test_field_manager_value() {
    assert_eq!(FIELD_MANAGER, "odoo-operator");
}

// ── OdooJobBuilder ──────────────────────────────────────────────────────────

use k8s_openapi::api::core::v1::{Affinity, Container, PodAffinity, Volume};

#[test]
fn test_builder_minimal_produces_valid_job() {
    let instance = test_instance("my-odoo", None);
    let init_job = test_init_job("my-init");

    let container = Container {
        name: "test".into(),
        image: Some("odoo:18.0".into()),
        ..Default::default()
    };

    let job = OdooJobBuilder::new("my-init-", "default", &init_job, &instance)
        .containers(vec![container])
        .build();

    // Metadata
    let meta = &job.metadata;
    assert_eq!(meta.generate_name, Some("my-init-".to_string()));
    assert_eq!(meta.namespace, Some("default".to_string()));
    let orefs = meta.owner_references.as_ref().unwrap();
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "OdooInitJob");

    // JobSpec defaults
    let spec = job.spec.as_ref().unwrap();
    assert_eq!(spec.backoff_limit, Some(0));
    assert_eq!(spec.ttl_seconds_after_finished, Some(900));
    assert!(spec.active_deadline_seconds.is_none());

    // PodSpec
    let pod = spec.template.spec.as_ref().unwrap();
    assert_eq!(pod.restart_policy, Some("Never".to_string()));
    assert!(pod.image_pull_secrets.is_none()); // no pull secret configured
    assert!(pod.affinity.is_none());

    // Security context
    let sc = pod.security_context.as_ref().unwrap();
    assert_eq!(sc.run_as_user, Some(100));

    // Standard volumes (filestore + odoo-conf)
    let vols = pod.volumes.as_ref().unwrap();
    assert_eq!(vols.len(), 2);
    assert_eq!(vols[0].name, "filestore");
    assert_eq!(vols[1].name, "odoo-conf");

    // Containers
    assert_eq!(pod.containers.len(), 1);
    assert_eq!(pod.containers[0].name, "test");
    assert!(pod.init_containers.is_none());
}

#[test]
fn test_builder_with_all_options() {
    let instance = test_instance("my-odoo", Some("my-secret"));
    let init_job = test_init_job("my-init");

    let extra_vol = Volume {
        name: "backup".into(),
        empty_dir: Some(Default::default()),
        ..Default::default()
    };

    let affinity = Affinity {
        pod_affinity: Some(PodAffinity {
            ..Default::default()
        }),
        ..Default::default()
    };

    let init_container = Container {
        name: "download".into(),
        image: Some("curl:latest".into()),
        ..Default::default()
    };

    let main_container = Container {
        name: "restore".into(),
        image: Some("odoo:18.0".into()),
        ..Default::default()
    };

    let job = OdooJobBuilder::new("my-restore-", "prod", &init_job, &instance)
        .active_deadline(3600)
        .extra_volumes(vec![extra_vol])
        .affinity(affinity)
        .init_containers(vec![init_container])
        .containers(vec![main_container])
        .build();

    let spec = job.spec.as_ref().unwrap();
    assert_eq!(spec.active_deadline_seconds, Some(3600));

    let pod = spec.template.spec.as_ref().unwrap();

    // Pull secrets from instance
    let secrets = pod.image_pull_secrets.as_ref().unwrap();
    assert_eq!(secrets[0].name, "my-secret");

    // Affinity set
    assert!(pod.affinity.is_some());

    // 2 standard + 1 extra volume
    let vols = pod.volumes.as_ref().unwrap();
    assert_eq!(vols.len(), 3);
    assert_eq!(vols[2].name, "backup");

    // Init containers
    let inits = pod.init_containers.as_ref().unwrap();
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].name, "download");

    // Main containers
    assert_eq!(pod.containers.len(), 1);
    assert_eq!(pod.containers[0].name, "restore");
}

#[test]
fn test_builder_empty_init_containers_becomes_none() {
    let instance = test_instance("my-odoo", None);
    let init_job = test_init_job("my-init");

    let job = OdooJobBuilder::new("x-", "ns", &init_job, &instance)
        .init_containers(vec![])
        .containers(vec![Container {
            name: "main".into(),
            ..Default::default()
        }])
        .build();

    let pod = job.spec.unwrap().template.spec.unwrap();
    assert!(pod.init_containers.is_none());
}
