//! Unit tests for `readonly_sql_env` — env-var injection for the read-only
//! SQL access feature.

use k8s_openapi::api::core::v1::EnvVarSource;
use kube::api::ObjectMeta;

use odoo_operator::controller::child_resources::{readonly_sql_env, ro_secret_name};
use odoo_operator::crd::odoo_instance::{
    CronSpec, IngressSpec, OdooInstance, OdooInstanceSpec, ReadOnlySqlAccessSpec,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_instance(name: &str, ro: Option<ReadOnlySqlAccessSpec>) -> OdooInstance {
    OdooInstance {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("test-ns".to_string()),
            uid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()),
            ..Default::default()
        },
        spec: OdooInstanceSpec {
            image: None,
            image_pull_secret: None,
            admin_password: "admin".to_string(),
            replicas: 1,
            cron: CronSpec::default(),
            ingress: IngressSpec {
                hosts: vec!["test.example.com".to_string()],
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
            read_only_sql_access: ro,
        },
        status: None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn readonly_sql_env_enabled_returns_two_vars() {
    let inst = make_instance(
        "myinstance",
        Some(ReadOnlySqlAccessSpec {
            enabled: true,
            connection_limit: 5,
        }),
    );
    let vars = readonly_sql_env(&inst);
    assert_eq!(vars.len(), 2, "expected exactly 2 env vars when enabled");

    let user_var = vars.iter().find(|v| v.name == "ODOO_RO_DB_USER");
    let pass_var = vars.iter().find(|v| v.name == "ODOO_RO_DB_PASSWORD");

    assert!(user_var.is_some(), "ODOO_RO_DB_USER must be present");
    assert!(pass_var.is_some(), "ODOO_RO_DB_PASSWORD must be present");
}

#[test]
fn readonly_sql_env_user_points_at_correct_secret_and_key() {
    let inst = make_instance(
        "myinstance",
        Some(ReadOnlySqlAccessSpec {
            enabled: true,
            connection_limit: 5,
        }),
    );
    let vars = readonly_sql_env(&inst);
    let user_var = vars.iter().find(|v| v.name == "ODOO_RO_DB_USER").unwrap();

    let secret_ref = match &user_var.value_from {
        Some(EnvVarSource {
            secret_key_ref: Some(r),
            ..
        }) => r,
        _ => panic!("ODOO_RO_DB_USER must use secretKeyRef"),
    };
    assert_eq!(secret_ref.name, ro_secret_name("myinstance"));
    assert_eq!(secret_ref.key, "username");
}

#[test]
fn readonly_sql_env_password_points_at_correct_secret_and_key() {
    let inst = make_instance(
        "myinstance",
        Some(ReadOnlySqlAccessSpec {
            enabled: true,
            connection_limit: 5,
        }),
    );
    let vars = readonly_sql_env(&inst);
    let pass_var = vars
        .iter()
        .find(|v| v.name == "ODOO_RO_DB_PASSWORD")
        .unwrap();

    let secret_ref = match &pass_var.value_from {
        Some(EnvVarSource {
            secret_key_ref: Some(r),
            ..
        }) => r,
        _ => panic!("ODOO_RO_DB_PASSWORD must use secretKeyRef"),
    };
    assert_eq!(secret_ref.name, ro_secret_name("myinstance"));
    assert_eq!(secret_ref.key, "password");
}

#[test]
fn readonly_sql_env_disabled_returns_empty() {
    let inst = make_instance(
        "myinstance",
        Some(ReadOnlySqlAccessSpec {
            enabled: false,
            connection_limit: 5,
        }),
    );
    let vars = readonly_sql_env(&inst);
    assert!(
        vars.is_empty(),
        "expected no env vars when enabled: false, got {:?}",
        vars.iter().map(|v| &v.name).collect::<Vec<_>>()
    );
}

#[test]
fn readonly_sql_env_absent_returns_empty() {
    let inst = make_instance("myinstance", None);
    let vars = readonly_sql_env(&inst);
    assert!(
        vars.is_empty(),
        "expected no env vars when read_only_sql_access is None, got {:?}",
        vars.iter().map(|v| &v.name).collect::<Vec<_>>()
    );
}
