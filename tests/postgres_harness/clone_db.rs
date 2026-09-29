//! Tests for the staging-refresh DB clone: the controller's preparation of the
//! temp database followed by `scripts/clone-db.sh` (issue #191).
//!
//! ACCEPTANCE CRITERIA
//!
//! 1. A source database whose `unaccent` is IMMUTABLE and which carries a
//!    trigram index over `unaccent(...)` — what Odoo 19 builds for `mail`
//!    fields once the operator has made unaccent indexable — clones without
//!    error, once the temp DB was prepared the way the controller does it.
//! 2. In the clone, the index exists and `unaccent` is IMMUTABLE, so Odoo
//!    keeps treating it as INDEXABLE.
//! 3. Negative control: a temp DB the tenant created on its own (no admin
//!    IMMUTABLE step) fails with the error reported in #191. `pg_dump` does
//!    not carry the volatility of an extension member function, so only the
//!    preparation makes the index restorable.

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, connect_as, pg_manager, run_script};

const CLONE_DB_SCRIPT: &str = include_str!("../../scripts/clone-db.sh");
const PW: &str = "clone-pw";

async fn reset_role_and_db(owner: &str, db: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{owner}""#))
        .await;
    c.simple_query(&format!(
        r#"CREATE ROLE "{owner}" WITH PASSWORD '{PW}' LOGIN CREATEDB NOSUPERUSER"#
    ))
    .await
    .expect("create role");
}

/// A production-like source: unaccent made IMMUTABLE by the operator, and an
/// Odoo-style trigram index over it.
async fn setup_source(owner: &str, db: &str) {
    reset_role_and_db(owner, db).await;
    admin_client()
        .await
        .simple_query(&format!(r#"CREATE DATABASE "{db}" OWNER "{owner}""#))
        .await
        .expect("create source db");
    pg_manager()
        .ensure_extensions(&cluster_config(), owner, PW, db, true)
        .await
        .expect("ensure_extensions on source");
    connect_as(owner, PW, db)
        .await
        .batch_execute(
            "CREATE TABLE mail_blacklist (id serial PRIMARY KEY, email varchar);
             INSERT INTO mail_blacklist (email) VALUES ('rené@example.com');
             CREATE INDEX mail_blacklist__email_index ON mail_blacklist
                 USING gin (unaccent((email)::text) gin_trgm_ops);",
        )
        .await
        .expect("create unaccent trigram index on source");
}

/// Runs off the async runtime: `docker exec` blocks, and a blocked runtime
/// never closes the connections the preparation step dropped — they would
/// then hold the temp DB open and fail the script's final rename, which no
/// operator reconcile does.
async fn run_clone(
    src_owner: &'static str,
    src_db: &'static str,
    tgt_owner: &'static str,
    tgt_db: &'static str,
    temp_db: &'static str,
) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        run_script(
            CLONE_DB_SCRIPT,
            &[
                ("SRC_HOST", "127.0.0.1"),
                ("SRC_PORT", "5432"),
                ("SRC_USER", src_owner),
                ("SRC_PASSWORD", PW),
                ("SRC_DB", src_db),
                ("TGT_HOST", "127.0.0.1"),
                ("TGT_PORT", "5432"),
                ("TGT_USER", tgt_owner),
                ("TGT_PASSWORD", PW),
                ("TGT_DB", tgt_db),
                ("TEMP_DB", temp_db),
            ],
        )
    })
    .await
    .expect("clone task panicked")
}

/// What `CloningFromSource::ensure` does before it launches the clone Job.
async fn prepare_like_controller(tgt_owner: &str, temp_db: &str) {
    let mgr = pg_manager();
    mgr.recreate_database(&cluster_config(), tgt_owner, PW, temp_db)
        .await
        .expect("recreate_database");
    mgr.ensure_extensions(&cluster_config(), tgt_owner, PW, temp_db, true)
        .await
        .expect("ensure_extensions on temp db");
}

async fn cleanup(dbs: &[&str], roles: &[&str]) {
    let c = admin_client().await;
    for db in dbs {
        let _ = c
            .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
            .await;
    }
    for role in roles {
        let _ = c
            .simple_query(&format!(r#"DROP ROLE IF EXISTS "{role}""#))
            .await;
    }
}

#[tokio::test]
async fn clones_a_db_with_unaccent_trigram_indexes() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_ok", "clone_src_ok_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_ok", "clone_tgt_ok_db");
    let temp_db = "clone_tgt_ok_db_refresh_1";
    setup_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    prepare_like_controller(tgt_owner, temp_db).await;

    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Criterion 1.
    assert!(out.status.success(), "clone-db.sh failed:\n{stderr}");

    // Criterion 2.
    let c = connect_as(tgt_owner, PW, tgt_db).await;
    let idx = c
        .query_opt(
            "SELECT 1 FROM pg_indexes WHERE indexname = 'mail_blacklist__email_index'",
            &[],
        )
        .await?;
    assert!(idx.is_some(), "unaccent trigram index missing in the clone");
    let vol: i8 = c
        .query_one(
            "SELECT provolatile FROM pg_proc WHERE proname = 'unaccent' AND pronargs = 1 \
               AND pronamespace = current_schema::regnamespace",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(vol, b'i' as i8, "unaccent must stay IMMUTABLE in the clone");

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}

#[tokio::test]
async fn temp_db_without_admin_preparation_fails_on_the_index() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_neg", "clone_src_neg_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_neg", "clone_tgt_neg_db");
    let temp_db = "clone_tgt_neg_db_refresh_1";
    setup_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    // Only what the tenant role can do by itself — the pre-#191 behaviour.
    pg_manager()
        .recreate_database(&cluster_config(), tgt_owner, PW, temp_db)
        .await?;

    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Criterion 3.
    assert!(!out.status.success(), "clone unexpectedly succeeded");
    assert!(
        stderr.contains("functions in index expression must be marked IMMUTABLE"),
        "unexpected failure:\n{stderr}"
    );

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}
