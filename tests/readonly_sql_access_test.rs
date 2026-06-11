//! Unit tests for the readOnlySqlAccess CRD field and read-only role helpers.

use kube::api::ObjectMeta;
use odoo_operator::crd::odoo_instance::{
    CronSpec, DatabaseSpec, IngressSpec, OdooInstance, OdooInstanceSpec, ReadOnlySqlAccessSpec,
};
use odoo_operator::helpers::{odoo_ro_username, odoo_username};

// ── Helper ───────────────────────────────────────────────────────────────────

fn make_instance_with_ro(ro_enabled: bool, connection_limit: i32) -> OdooInstance {
    OdooInstance {
        metadata: ObjectMeta {
            name: Some("rwi2".to_string()),
            namespace: Some("rwi".to_string()),
            uid: Some("ae819407-a467-4155-b2b2-8f38c400fb1f".to_string()),
            ..Default::default()
        },
        spec: OdooInstanceSpec {
            image: None,
            image_pull_secret: None,
            admin_password: "admin".to_string(),
            replicas: 1,
            cron: CronSpec::default(),
            ingress: IngressSpec {
                hosts: vec!["rwi2.example.com".to_string()],
                issuer: None,
                class: None,
                gateway_ref: None,
            },
            resources: None,
            filestore: None,
            config_options: None,
            database: Some(DatabaseSpec {
                cluster: None,
                name: Some("odoo_ae819407_a467_4155_b2b2_8f38c400fb1f".to_string()),
                missing_policy: Default::default(),
            }),
            init: Default::default(),
            environment: Default::default(),
            production_instance_ref: None,
            strategy: None,
            webhook: None,
            probes: None,
            affinity: None,
            tolerations: vec![],
            read_only_sql_access: Some(ReadOnlySqlAccessSpec {
                enabled: ro_enabled,
                connection_limit,
            }),
        },
        status: None,
    }
}

fn make_instance_no_ro() -> OdooInstance {
    OdooInstance {
        metadata: ObjectMeta {
            name: Some("basic".to_string()),
            namespace: Some("ns".to_string()),
            uid: Some("00000000-0000-0000-0000-000000000001".to_string()),
            ..Default::default()
        },
        spec: OdooInstanceSpec {
            image: None,
            image_pull_secret: None,
            admin_password: "admin".to_string(),
            replicas: 1,
            cron: CronSpec::default(),
            ingress: IngressSpec {
                hosts: vec!["basic.example.com".to_string()],
                issuer: None,
                class: None,
                gateway_ref: None,
            },
            resources: None,
            filestore: None,
            config_options: None,
            database: None,
            init: Default::default(),
            environment: Default::default(),
            production_instance_ref: None,
            strategy: None,
            webhook: None,
            probes: None,
            affinity: None,
            tolerations: vec![],
            read_only_sql_access: None,
        },
        status: None,
    }
}

// ── CRD field round-trip ─────────────────────────────────────────────────────

#[test]
fn crd_field_absent_by_default() {
    let inst = make_instance_no_ro();
    assert!(inst.spec.read_only_sql_access.is_none());
}

#[test]
fn crd_field_enabled_true_round_trips() {
    let inst = make_instance_with_ro(true, 5);
    let spec = inst.spec.read_only_sql_access.as_ref().unwrap();
    assert!(spec.enabled);
    assert_eq!(spec.connection_limit, 5);
}

#[test]
fn crd_field_enabled_false_round_trips() {
    let inst = make_instance_with_ro(false, 3);
    let spec = inst.spec.read_only_sql_access.as_ref().unwrap();
    assert!(!spec.enabled);
    assert_eq!(spec.connection_limit, 3);
}

#[test]
fn crd_field_json_round_trips() {
    let inst = make_instance_with_ro(true, 10);
    let json = serde_json::to_value(&inst.spec).unwrap();
    let ro = json
        .get("readOnlySqlAccess")
        .expect("readOnlySqlAccess present");
    assert_eq!(ro["enabled"], true);
    assert_eq!(ro["connectionLimit"], 10);
}

#[test]
fn crd_field_absent_serialises_without_key() {
    let inst = make_instance_no_ro();
    let json = serde_json::to_value(&inst.spec).unwrap();
    // skip_serializing_if = "Option::is_none" means the key must be absent.
    assert!(
        json.get("readOnlySqlAccess").is_none(),
        "readOnlySqlAccess should not appear when None"
    );
}

// ── ro_username derivation ───────────────────────────────────────────────────

#[test]
fn ro_username_is_owner_plus_suffix() {
    let owner = odoo_username("rwi", "rwi2");
    let ro = odoo_ro_username("rwi", "rwi2");
    assert_eq!(owner, "odoo.rwi.rwi2");
    assert_eq!(ro, "odoo.rwi.rwi2_ro");
}

#[test]
fn ro_username_does_not_have_createdb() {
    // The role name itself should end in _ro — convention that communicates
    // the role is read-only.  Actual NOSUPERUSER/NOCREATEDB enforcement lives
    // in the PgPostgresManager SQL, but the name is the first signal.
    let ro = odoo_ro_username("ns", "inst");
    assert!(ro.ends_with("_ro"), "ro role name must end with _ro");
}

// ── ReadOnlySqlAccessSpec defaults ──────────────────────────────────────────

#[test]
fn ro_spec_default_is_disabled() {
    let spec = ReadOnlySqlAccessSpec::default();
    assert!(!spec.enabled);
    assert_eq!(spec.connection_limit, 5);
}

// ── Reconcile guard — enabled check ─────────────────────────────────────────

#[test]
fn reconcile_guard_enabled_flag() {
    // Simulate the guard used in reconcile_instance: only provision when enabled.
    let enabled_inst = make_instance_with_ro(true, 5);
    let disabled_inst = make_instance_with_ro(false, 5);
    let absent_inst = make_instance_no_ro();

    let should_provision = |inst: &OdooInstance| {
        inst.spec
            .read_only_sql_access
            .as_ref()
            .is_some_and(|s| s.enabled)
    };

    assert!(should_provision(&enabled_inst));
    assert!(!should_provision(&disabled_inst));
    assert!(!should_provision(&absent_inst));
}
