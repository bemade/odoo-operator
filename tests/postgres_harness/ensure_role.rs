//! Regression test for issue #119, part C.
//!
//! When `ensure_role` is invoked for a role that already exists, the role's
//! password must be reset to the supplied value. Today the production
//! implementation early-returns when the role exists, so a same-name
//! re-create after a finalizer-blocked deletion can't authenticate with
//! its freshly-generated per-instance Secret.

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, pg_manager, try_connect_as};

/// Drop the role if it exists from a prior test run (best-effort).
async fn cleanup_role(username: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{username}""#))
        .await;
}

#[tokio::test]
async fn ensure_role_resets_password_when_role_already_exists() -> anyhow::Result<()> {
    let username = "odoo.test.ensure_role_reset";
    cleanup_role(username).await;

    let mgr = pg_manager();
    let cfg = cluster_config();

    // First call: creates the role with the initial password.
    mgr.ensure_role(&cfg, username, "first-password")
        .await
        .expect("initial ensure_role failed");
    try_connect_as(username, "first-password", "postgres")
        .await
        .expect("initial password should authenticate");

    // Second call simulates the operator re-reconciling after a same-name
    // re-create: same username, NEW password from a fresh Secret. Today
    // this is a no-op (early return) — the role keeps the old password.
    mgr.ensure_role(&cfg, username, "second-password")
        .await
        .expect("second ensure_role failed");

    // After the fix, the new password authenticates.
    try_connect_as(username, "second-password", "postgres")
        .await
        .expect(
            "after ensure_role with a new password the new password must \
             authenticate — the role's password was not reset",
        );

    cleanup_role(username).await;
    Ok(())
}

/// Regression test for issue #128.
///
/// On PostgreSQL 16+ `ALTER ROLE ... WITH PASSWORD` by a non-superuser admin
/// requires ADMIN OPTION on the target role. The implicit grant the admin
/// receives at CREATE ROLE time is not durable when the admin role itself is
/// reconciled by an external manager (e.g. CloudNativePG `managed.roles`
/// re-asserts the membership list, stripping it). An unconditional ALTER on
/// every reconcile then fails with "permission denied to alter role" even
/// though the password never changed. `ensure_role` must succeed without
/// ALTER privileges when the supplied password already authenticates.
#[tokio::test]
async fn ensure_role_succeeds_without_alter_privilege_when_password_is_current(
) -> anyhow::Result<()> {
    let username = "odoo.test.ensure_role_no_alter_priv";
    let limited_admin = "odoo.test.limited_admin";
    cleanup_role(username).await;
    cleanup_role(limited_admin).await;

    let superuser = admin_client().await;
    // A realistic non-superuser operator admin: can create roles/databases
    // but holds no blanket privilege over existing roles (PG16+ semantics).
    superuser
        .simple_query(&format!(
            r#"CREATE ROLE "{limited_admin}" WITH LOGIN CREATEROLE CREATEDB PASSWORD 'limited-pw'"#
        ))
        .await?;

    let mut cfg = cluster_config();
    cfg.admin_user = limited_admin.to_string();
    cfg.admin_password = "limited-pw".to_string();

    let mgr = pg_manager();
    // First reconcile: the limited admin creates the role (and implicitly
    // receives ADMIN OPTION on it, being its creator).
    mgr.ensure_role(&cfg, username, "steady-password")
        .await
        .expect("initial ensure_role as limited admin failed");

    // Simulate the external role manager re-asserting the admin's
    // memberships: the implicit grant from CREATE ROLE is stripped.
    superuser
        .simple_query(&format!(r#"REVOKE "{username}" FROM "{limited_admin}""#))
        .await?;

    // Steady-state reconcile: same username, same password. This must not
    // require ALTER ROLE privileges the admin no longer has.
    mgr.ensure_role(&cfg, username, "steady-password")
        .await
        .expect(
            "ensure_role with an unchanged password must not require ALTER \
             ROLE privileges on the target role (issue #128)",
        );

    try_connect_as(username, "steady-password", "postgres")
        .await
        .expect("the unchanged password should still authenticate");

    cleanup_role(username).await;
    cleanup_role(limited_admin).await;
    Ok(())
}
