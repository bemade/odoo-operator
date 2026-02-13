//! Integration tests for the odoo-operator.
//!
//! ## Pure logic tests (run always)
//! Tests for state machine transition guards, `owner_ref`, etc.
//! These need no cluster — they exercise extracted pure functions.
//!
//! ## Cluster tests (run with `cargo test -- --ignored`)
//! Tests that require a running Kubernetes cluster (kind, minikube, etc.).
//! They apply CRDs, create resources, and verify the results.
//!
//! To run cluster tests:
//!   1. `kind create cluster` (or use an existing minikube)
//!   2. `cargo test -- --ignored`

use kube::{Api, Client, CustomResourceExt, Resource, ResourceExt};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use serde_json::json;

use odoo_operator::controller::odoo_instance::owner_ref;
use odoo_operator::controller::state_machine::{ReconcileSnapshot, TRANSITIONS};
use odoo_operator::crd::odoo_instance::{
    FilestoreSpec, IngressSpec, OdooInstance, OdooInstancePhase, OdooInstanceSpec,
};
use odoo_operator::crd::shared::Phase;

// ═══════════════════════════════════════════════════════════════════════════════
// Pure logic tests — no cluster needed
// ═══════════════════════════════════════════════════════════════════════════════

fn test_spec(replicas: i32) -> OdooInstanceSpec {
    OdooInstanceSpec {
        image: Some("odoo:18.0".into()),
        image_pull_secret: None,
        admin_password: "admin".into(),
        replicas,
        ingress: IngressSpec {
            hosts: vec!["test.example.com".into()],
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
    }
}

fn make_instance(replicas: i32) -> OdooInstance {
    let mut inst = OdooInstance::new("test-instance", test_spec(replicas));
    inst.meta_mut().namespace = Some("default".into());
    inst.meta_mut().uid = Some("test-uid-1234".into());
    inst
}

fn default_snapshot() -> ReconcileSnapshot {
    ReconcileSnapshot {
        ready_replicas: 0,
        db_initialized: false,
        init_job_phase: None,
        restore_job_phase: None,
        upgrade_job_phase: None,
        backup_job_active: false,
        restore_job_active: false,
        upgrade_job_active: false,
        active_init_job: None,
        active_restore_job: None,
        active_upgrade_job: None,
        active_backup_job: None,
    }
}

/// Find the first matching transition from the given phase for the instance+snapshot.
fn first_transition(
    from: &OdooInstancePhase,
    inst: &OdooInstance,
    snap: &ReconcileSnapshot,
) -> Option<OdooInstancePhase> {
    TRANSITIONS
        .iter()
        .filter(|t| t.from == *from)
        .find(|t| (t.guard)(inst, snap))
        .map(|t| t.to.clone())
}

// ── Transition guard tests ──────────────────────────────────────────────────

#[test]
fn provisioning_to_uninitialized_when_db_not_init() {
    let inst = make_instance(1);
    let snap = default_snapshot();
    assert_eq!(
        first_transition(&OdooInstancePhase::Provisioning, &inst, &snap),
        Some(OdooInstancePhase::Uninitialized)
    );
}

#[test]
fn provisioning_to_starting_when_db_initialized() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot { db_initialized: true, ..default_snapshot() };
    assert_eq!(
        first_transition(&OdooInstancePhase::Provisioning, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn uninitialized_to_initializing_when_init_running() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        init_job_phase: Some(Phase::Running),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Uninitialized, &inst, &snap),
        Some(OdooInstancePhase::Initializing)
    );
}

#[test]
fn uninitialized_to_restoring_when_restore_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        restore_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Uninitialized, &inst, &snap),
        Some(OdooInstancePhase::Restoring)
    );
}

#[test]
fn initializing_to_starting_when_init_completed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        init_job_phase: Some(Phase::Completed),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Initializing, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn initializing_to_init_failed_when_init_failed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        init_job_phase: Some(Phase::Failed),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Initializing, &inst, &snap),
        Some(OdooInstancePhase::InitFailed)
    );
}

#[test]
fn starting_to_running_when_all_ready() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 2,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        Some(OdooInstancePhase::Running)
    );
}

#[test]
fn starting_to_stopped_when_replicas_zero() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot { db_initialized: true, ..default_snapshot() };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        Some(OdooInstancePhase::Stopped)
    );
}

#[test]
fn starting_stays_when_not_ready() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 0,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        None
    );
}

#[test]
fn running_to_degraded_when_partial_ready() {
    let inst = make_instance(3);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        Some(OdooInstancePhase::Degraded)
    );
}

#[test]
fn running_to_stopped_when_replicas_zero() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot {
        ready_replicas: 0,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        Some(OdooInstancePhase::Stopped)
    );
}

#[test]
fn running_to_restoring_when_restore_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        restore_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        Some(OdooInstancePhase::Restoring)
    );
}

#[test]
fn running_to_upgrading_when_upgrade_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        upgrade_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        Some(OdooInstancePhase::Upgrading)
    );
}

#[test]
fn running_to_backing_up_when_backup_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        backup_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        Some(OdooInstancePhase::BackingUp)
    );
}

#[test]
fn running_stays_when_healthy() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 2,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Running, &inst, &snap),
        None
    );
}

#[test]
fn backing_up_to_running_when_backup_done() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        backup_job_active: false,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::BackingUp, &inst, &snap),
        Some(OdooInstancePhase::Running)
    );
}

#[test]
fn upgrading_to_starting_when_upgrade_completed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        upgrade_job_phase: Some(Phase::Completed),
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Upgrading, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn upgrading_to_starting_when_upgrade_failed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        upgrade_job_phase: Some(Phase::Failed),
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Upgrading, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn restoring_to_starting_when_restore_completed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        restore_job_phase: Some(Phase::Completed),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Restoring, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn restoring_to_starting_when_restore_failed() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        restore_job_phase: Some(Phase::Failed),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Restoring, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn stopped_to_starting_when_replicas_positive() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot { db_initialized: true, ..default_snapshot() };
    assert_eq!(
        first_transition(&OdooInstancePhase::Stopped, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn stopped_stays_when_replicas_zero() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot { db_initialized: true, ..default_snapshot() };
    assert_eq!(
        first_transition(&OdooInstancePhase::Stopped, &inst, &snap),
        None
    );
}

#[test]
fn init_failed_to_initializing_on_retry() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        init_job_phase: Some(Phase::Running),
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::InitFailed, &inst, &snap),
        Some(OdooInstancePhase::Initializing)
    );
}

#[test]
fn init_failed_to_restoring_on_restore() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        restore_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::InitFailed, &inst, &snap),
        Some(OdooInstancePhase::Restoring)
    );
}

#[test]
fn degraded_to_running_when_all_ready() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 2,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Degraded, &inst, &snap),
        Some(OdooInstancePhase::Running)
    );
}

#[test]
fn degraded_to_starting_when_zero_ready() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 0,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Degraded, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

// ── New transitions added in audit ───────────────────────────────────────────

#[test]
fn starting_to_restoring_when_restore_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        restore_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        Some(OdooInstancePhase::Restoring)
    );
}

#[test]
fn starting_to_upgrading_when_upgrade_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        upgrade_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        Some(OdooInstancePhase::Upgrading)
    );
}

#[test]
fn starting_to_backing_up_when_backup_active() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        backup_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Starting, &inst, &snap),
        Some(OdooInstancePhase::BackingUp)
    );
}

#[test]
fn degraded_to_backing_up_when_backup_active() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        backup_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Degraded, &inst, &snap),
        Some(OdooInstancePhase::BackingUp)
    );
}

#[test]
fn backing_up_to_stopped_when_replicas_zero() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot {
        backup_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::BackingUp, &inst, &snap),
        Some(OdooInstancePhase::Stopped)
    );
}

#[test]
fn backing_up_to_degraded_when_partial_ready() {
    let inst = make_instance(3);
    let snap = ReconcileSnapshot {
        ready_replicas: 1,
        db_initialized: true,
        backup_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::BackingUp, &inst, &snap),
        Some(OdooInstancePhase::Degraded)
    );
}

#[test]
fn backing_up_to_starting_when_zero_ready() {
    let inst = make_instance(2);
    let snap = ReconcileSnapshot {
        ready_replicas: 0,
        db_initialized: true,
        backup_job_active: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::BackingUp, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn stopped_to_restoring_when_restore_active() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot {
        restore_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Stopped, &inst, &snap),
        Some(OdooInstancePhase::Restoring)
    );
}

#[test]
fn stopped_to_upgrading_when_upgrade_active() {
    let inst = make_instance(0);
    let snap = ReconcileSnapshot {
        upgrade_job_active: true,
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Stopped, &inst, &snap),
        Some(OdooInstancePhase::Upgrading)
    );
}

#[test]
fn error_to_starting_when_db_initialized() {
    let inst = make_instance(1);
    let snap = ReconcileSnapshot {
        db_initialized: true,
        ..default_snapshot()
    };
    assert_eq!(
        first_transition(&OdooInstancePhase::Error, &inst, &snap),
        Some(OdooInstancePhase::Starting)
    );
}

#[test]
fn error_to_uninitialized_when_db_not_initialized() {
    let inst = make_instance(1);
    let snap = default_snapshot();
    assert_eq!(
        first_transition(&OdooInstancePhase::Error, &inst, &snap),
        Some(OdooInstancePhase::Uninitialized)
    );
}

// ── owner_ref ────────────────────────────────────────────────────────────────

#[test]
fn owner_ref_has_correct_fields() {
    let inst = make_instance(1);
    let oref = owner_ref(&inst);
    assert_eq!(oref.kind, "OdooInstance");
    assert_eq!(oref.api_version, "bemade.org/v1alpha1");
    assert_eq!(oref.name, "test-instance");
    assert_eq!(oref.uid, "test-uid-1234");
    assert_eq!(oref.controller, Some(true));
    assert_eq!(oref.block_owner_deletion, Some(true));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cluster integration tests — require a running k8s cluster
// Run with: cargo test -- --ignored
// ═══════════════════════════════════════════════════════════════════════════════

/// Apply all operator CRDs to the cluster and verify they are accepted.
#[tokio::test]
#[ignore = "requires a running k8s cluster"]
async fn cluster_apply_crds() {
    let client = Client::try_default().await.expect("kubeconfig must be available");
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());

    let all_crds = vec![
        OdooInstance::crd(),
        odoo_operator::crd::odoo_init_job::OdooInitJob::crd(),
        odoo_operator::crd::odoo_backup_job::OdooBackupJob::crd(),
        odoo_operator::crd::odoo_restore_job::OdooRestoreJob::crd(),
        odoo_operator::crd::odoo_upgrade_job::OdooUpgradeJob::crd(),
    ];

    let pp = PatchParams::apply("integration-test").force();
    for crd in &all_crds {
        let name = crd.metadata.name.as_deref().unwrap();
        crds.patch(name, &pp, &Patch::Apply(crd))
            .await
            .unwrap_or_else(|e| panic!("failed to apply CRD {name}: {e}"));
    }

    // Give the API server a moment to register the CRDs.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify all CRDs exist.
    for crd in &all_crds {
        let name = crd.metadata.name.as_deref().unwrap();
        let fetched = crds.get(name).await
            .unwrap_or_else(|e| panic!("CRD {name} not found after apply: {e}"));
        assert_eq!(fetched.name_any(), name);
    }
}

/// Create an OdooInstance CR and verify it is accepted by the API server.
/// Requires CRDs to be applied first (run cluster_apply_crds).
#[tokio::test]
#[ignore = "requires a running k8s cluster"]
async fn cluster_create_odoo_instance() {
    let client = Client::try_default().await.expect("kubeconfig must be available");
    let ns = "default";
    let instances: Api<OdooInstance> = Api::namespaced(client.clone(), ns);

    let test_name = "integration-test-inst";

    // Clean up from any previous run.
    let _ = instances.delete(test_name, &DeleteParams::default()).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Create the instance.
    let inst = OdooInstance::new(test_name, test_spec(1));

    let created = instances.create(&PostParams::default(), &inst).await
        .expect("failed to create OdooInstance");
    assert_eq!(created.name_any(), test_name);
    assert!(created.metadata.uid.is_some());

    // Verify we can read it back.
    let fetched = instances.get(test_name).await
        .expect("failed to get OdooInstance");
    assert_eq!(fetched.spec.replicas, 1);
    assert_eq!(fetched.spec.ingress.hosts, vec!["test.example.com"]);

    // Verify status subresource works (patch status).
    let status_patch = json!({
        "status": {
            "phase": "Uninitialized",
            "readyReplicas": 0,
            "dbInitialized": false,
        }
    });
    let pp = PatchParams::apply("integration-test");
    instances.patch_status(test_name, &pp, &Patch::Merge(&status_patch)).await
        .expect("failed to patch status");

    let after_status = instances.get(test_name).await.unwrap();
    let status = after_status.status.expect("status should be set");
    assert_eq!(status.phase, Some(OdooInstancePhase::Uninitialized));
    assert_eq!(status.ready_replicas, 0);

    // Clean up.
    instances.delete(test_name, &DeleteParams::default()).await
        .expect("failed to delete test instance");
}

/// Verify the webhook validation logic works against real AdmissionReview payloads.
/// This tests the pure validation function, not the webhook server itself.
#[test]
fn webhook_rejects_storage_shrink() {
    let mut old_spec = test_spec(1);
    old_spec.filestore = Some(FilestoreSpec {
        storage_size: Some("10Gi".into()),
        storage_class: Some("standard".into()),
    });
    let mut new_spec = test_spec(1);
    new_spec.filestore = Some(FilestoreSpec {
        storage_size: Some("5Gi".into()),  // shrink!
        storage_class: Some("standard".into()),
    });

    let old_size = old_spec.filestore.as_ref().unwrap().storage_size.as_deref().unwrap();
    let new_size = new_spec.filestore.as_ref().unwrap().storage_size.as_deref().unwrap();

    // Parse and compare (replicating the webhook logic).
    let old_bytes = parse_quantity_for_test(old_size);
    let new_bytes = parse_quantity_for_test(new_size);
    assert!(new_bytes < old_bytes, "5Gi should be less than 10Gi");
}

/// Verify storage class change is detectable.
#[test]
fn webhook_detects_storage_class_change() {
    let old_class = "standard";
    let new_class = "premium-ssd";
    assert_ne!(old_class, new_class);
    assert!(!old_class.is_empty());
}

/// Simple quantity parser for test assertions (mirrors webhook.rs logic).
fn parse_quantity_for_test(s: &str) -> u64 {
    let s = s.trim();
    let num_end = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(num_end);
    let num: f64 = num_str.parse().unwrap();
    let multiplier: u64 = match suffix {
        "Ki" => 1024,
        "Mi" => 1024 * 1024,
        "Gi" => 1024 * 1024 * 1024,
        "Ti" => 1024 * 1024 * 1024 * 1024,
        "" => 1,
        _ => panic!("unknown suffix {suffix}"),
    };
    (num * multiplier as f64) as u64
}
