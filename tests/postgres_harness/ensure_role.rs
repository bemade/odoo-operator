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
