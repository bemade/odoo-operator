//! Tests for `apply_extra_env` — the `spec.extraEnv` / `spec.extraEnvFrom`
//! layering applied to an instance's Odoo containers (web, cron, init,
//! upgrade, neutralize). Operator-tooling containers (the `mc` backup
//! uploader, pg-client clone/restore steps) are deliberately NOT wrapped at
//! their call sites, so this helper is the single place the merge semantics
//! live; the scoping is enforced by where it is (and is not) called.

use k8s_openapi::api::core::v1::{
    Container, EnvFromSource, EnvVar, EnvVarSource, SecretEnvSource, SecretKeySelector,
};
use kube::api::ObjectMeta;

use odoo_operator::controller::helpers::apply_extra_env;
use odoo_operator::crd::odoo_instance::{CronSpec, IngressSpec, OdooInstance, OdooInstanceSpec};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn ev(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

fn make_instance(extra_env: Vec<EnvVar>, extra_env_from: Vec<EnvFromSource>) -> OdooInstance {
    OdooInstance {
        metadata: ObjectMeta {
            name: Some("inst".to_string()),
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
                hosts: vec!["inst.example.com".to_string()],
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
            extra_env,
            extra_env_from,
        },
        status: None,
    }
}

fn secret_from(name: &str) -> EnvFromSource {
    EnvFromSource {
        secret_ref: Some(SecretEnvSource {
            name: name.into(),
            optional: None,
        }),
        ..Default::default()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn noop_when_no_extras_leaves_container_byte_identical() {
    // A container with no env at all (e.g. the init job) must stay env: None /
    // env_from: None so server-side apply sees no diff for instances that set
    // neither field.
    let c = Container {
        name: "init".into(),
        env: None,
        ..Default::default()
    };
    let out = apply_extra_env(c, &make_instance(vec![], vec![]));
    assert!(out.env.is_none(), "env must stay None when no extra_env");
    assert!(
        out.env_from.is_none(),
        "env_from must stay None when no extra_env_from"
    );
}

#[test]
fn noop_when_no_extras_preserves_existing_env() {
    let c = Container {
        name: "odoo".into(),
        env: Some(vec![ev("PGDATABASE", "db")]),
        ..Default::default()
    };
    let out = apply_extra_env(c, &make_instance(vec![], vec![]));
    assert_eq!(out.env.unwrap(), vec![ev("PGDATABASE", "db")]);
}

#[test]
fn extra_env_merges_last_wins_over_operator_env() {
    let c = Container {
        name: "odoo".into(),
        env: Some(vec![ev("PGDATABASE", "db"), ev("ODOO_RC", "/etc/odoo")]),
        ..Default::default()
    };
    // A plain new var plus an override of an operator-set name.
    let out = apply_extra_env(
        c,
        &make_instance(
            vec![ev("AWS_ACCESS_KEY_ID", "AKIA"), ev("PGDATABASE", "other")],
            vec![],
        ),
    );
    let env = out.env.unwrap();
    // Override lands in the base slot; new var appended; order preserved.
    assert_eq!(
        env,
        vec![
            ev("PGDATABASE", "other"),
            ev("ODOO_RC", "/etc/odoo"),
            ev("AWS_ACCESS_KEY_ID", "AKIA"),
        ]
    );
}

#[test]
fn extra_env_supports_value_from_secret() {
    let secret_var = EnvVar {
        name: "SMTP_PASSWORD".into(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: "smtp-creds".into(),
                key: "password".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let c = Container {
        name: "odoo".into(),
        env: None,
        ..Default::default()
    };
    let out = apply_extra_env(c, &make_instance(vec![secret_var.clone()], vec![]));
    assert_eq!(out.env.unwrap(), vec![secret_var]);
}

#[test]
fn extra_env_from_is_appended_to_existing_env_from() {
    let pre = secret_from("operator-managed");
    let user = secret_from("oca-s3-attachments");
    let c = Container {
        name: "odoo".into(),
        env_from: Some(vec![pre.clone()]),
        ..Default::default()
    };
    let out = apply_extra_env(c, &make_instance(vec![], vec![user.clone()]));
    // Appended after the operator's own sources, preserving order.
    assert_eq!(out.env_from.unwrap(), vec![pre, user]);
}
