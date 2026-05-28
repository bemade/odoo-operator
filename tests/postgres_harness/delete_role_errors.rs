//! Regression test for issue #119, part E.
//!
//! `delete_role` failures must surface the actual PostgreSQL error text in
//! the returned `Error` (and therefore in operator logs / events). The
//! production path currently bubbles `tokio_postgres::Error` through the
//! `#[from]` impl on `Error::Postgres`, whose Display only emits "db error"
//! — operators cannot diagnose why cleanup is stuck without manually
//! connecting to the cluster.

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, pg_manager};

async fn cleanup(username: &str, depdb: &str) {
    let c = admin_client().await;
    // The dependency arrangement requires unwinding before drop.
    let _ = c
        .simple_query(&format!(
            r#"REVOKE ALL ON DATABASE "{depdb}" FROM "{username}""#
        ))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{depdb}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{username}""#))
        .await;
}

#[tokio::test]
async fn delete_role_error_includes_real_postgres_message() -> anyhow::Result<()> {
    let username = "odoo.test.delete_role_err";
    let depdb = "odoo_test_delete_role_err_dep";
    cleanup(username, depdb).await;

    // Set up a role with privileges on a database it does NOT own — the
    // delete_role code path drops owned databases first, so we need a
    // dependency that survives that step. GRANT CONNECT on an admin-owned
    // DB blocks the subsequent DROP ROLE with a dependency error.
    let c = admin_client().await;
    c.simple_query(&format!(
        r#"CREATE ROLE "{username}" WITH PASSWORD 'x' LOGIN"#
    ))
    .await?;
    c.simple_query(&format!(r#"CREATE DATABASE "{depdb}""#))
        .await?;
    c.simple_query(&format!(
        r#"GRANT CONNECT ON DATABASE "{depdb}" TO "{username}""#
    ))
    .await?;

    let cfg = cluster_config();
    let result = pg_manager().delete_role(&cfg, username).await;
    let err = match result {
        Ok(()) => panic!("expected delete_role to fail due to dependent grant"),
        Err(e) => e,
    };

    let msg = format!("{err}");
    assert!(
        // Postgres emits something like "role ... cannot be dropped because
        // some objects depend on it". We don't pin the exact wording but do
        // require *some* substring of the real PG message instead of the
        // opaque tokio_postgres default ("db error").
        msg.to_lowercase().contains("cannot be dropped")
            || msg.to_lowercase().contains("depend")
            || msg.to_lowercase().contains("grant"),
        "expected error to surface the actual PostgreSQL message, got: {msg:?}"
    );

    cleanup(username, depdb).await;
    Ok(())
}
