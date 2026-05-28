//! Tests for `PostgresManager::database_exists`.
//!
//! The operator's "DB missing" auto-recovery (issue #119, part D) is built
//! on `database_exists` returning `Ok(false)` for a definitely-absent DB.
//! The bare query `SELECT 1 FROM pg_database WHERE datname = $1` returns
//! an empty result set for absent — this test verifies the production
//! impl uses the `SELECT EXISTS(...)` form that collapses both cases into
//! a clean bool, so callers can confidently distinguish "no" from
//! "unknown" (which is `Err(_)`).

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, pg_manager};

async fn cleanup(db_name: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{db_name}""#))
        .await;
}

#[tokio::test]
async fn database_exists_returns_ok_true_when_present() -> anyhow::Result<()> {
    let db_name = "odoo_test_db_exists_present";
    cleanup(db_name).await;

    let c = admin_client().await;
    c.simple_query(&format!(r#"CREATE DATABASE "{db_name}""#))
        .await?;

    let result = pg_manager()
        .database_exists(&cluster_config(), db_name)
        .await?;
    assert!(
        result,
        "expected database_exists to be true for a created DB"
    );

    cleanup(db_name).await;
    Ok(())
}

#[tokio::test]
async fn database_exists_returns_ok_false_when_absent() -> anyhow::Result<()> {
    let db_name = "odoo_test_db_exists_absent";
    cleanup(db_name).await; // make sure it's not there

    let result = pg_manager()
        .database_exists(&cluster_config(), db_name)
        .await?;
    assert!(
        !result,
        "expected database_exists to be Ok(false) for an absent DB; \
         got Ok(true) instead — caller cannot distinguish 'absent' from 'present'"
    );

    Ok(())
}
