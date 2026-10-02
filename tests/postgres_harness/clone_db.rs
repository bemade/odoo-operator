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
//! 4. A source laid out the way Odoo.sh does it — the `unaccent` extension in
//!    its own schema, plus a plain IMMUTABLE `public.unaccent(text)` SQL
//!    wrapper that the trigram indexes use — is detected as custom, while an
//!    operator-prepared source and a database without unaccent are not.
//! 5. Such a source clones without error once the temp DB is prepared the way
//!    the controller does it: the preparation leaves unaccent to the dump.
//!    In the clone the wrapper is IMMUTABLE, the index exists, and the
//!    steady-state `ensure_extensions` on the result is a no-op that
//!    succeeds.
//! 6. Negative control: preparing that temp DB with unaccent regardless (the
//!    2.8.2 behaviour) fails, because the extension it creates in `public`
//!    already provides the `unaccent(text)` the dump's wrapper defines.

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

/// A source as Odoo.sh lays it out: `unaccent` installed in `unaccent_schema`
/// and reached through an IMMUTABLE SQL wrapper in `public`, which Odoo's
/// trigram indexes are built on.
async fn setup_odoosh_source(owner: &str, db: &str) {
    reset_role_and_db(owner, db).await;
    admin_client()
        .await
        .simple_query(&format!(r#"CREATE DATABASE "{db}" OWNER "{owner}""#))
        .await
        .expect("create source db");
    connect_as(owner, PW, db)
        .await
        .batch_execute(
            "CREATE EXTENSION pg_trgm;
             CREATE SCHEMA unaccent_schema;
             CREATE EXTENSION unaccent SCHEMA unaccent_schema;
             CREATE FUNCTION public.unaccent(text) RETURNS text LANGUAGE sql IMMUTABLE AS
                 $$ SELECT unaccent_schema.unaccent('unaccent_schema.unaccent', $1) $$;
             CREATE TABLE mail_blacklist (id serial PRIMARY KEY, email varchar);
             INSERT INTO mail_blacklist (email) VALUES ('rené@example.com');
             CREATE INDEX mail_blacklist__email_index ON mail_blacklist
                 USING gin (unaccent((email)::text) gin_trgm_ops);",
        )
        .await
        .expect("create Odoo.sh-style unaccent layout on source");
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

/// What `CloningFromSource::ensure` does before it launches the clone Job,
/// for instances that want unaccent.
async fn prepare_like_controller(src_owner: &str, src_db: &str, tgt_owner: &str, temp_db: &str) {
    let mgr = pg_manager();
    let custom = mgr
        .has_custom_unaccent(&cluster_config(), src_owner, PW, src_db)
        .await
        .expect("has_custom_unaccent on source");
    mgr.recreate_database(&cluster_config(), tgt_owner, PW, temp_db)
        .await
        .expect("recreate_database");
    mgr.ensure_extensions(&cluster_config(), tgt_owner, PW, temp_db, !custom)
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
    prepare_like_controller(src_owner, src_db, tgt_owner, temp_db).await;

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

#[tokio::test]
async fn detects_a_custom_unaccent_layout() -> anyhow::Result<()> {
    let mgr = pg_manager();
    let (std_owner, std_db) = ("clone_detect_std", "clone_detect_std_db");
    let (sh_owner, sh_db) = ("clone_detect_sh", "clone_detect_sh_db");
    let (bare_owner, bare_db) = ("clone_detect_bare", "clone_detect_bare_db");
    setup_source(std_owner, std_db).await;
    setup_odoosh_source(sh_owner, sh_db).await;
    reset_role_and_db(bare_owner, bare_db).await;
    mgr.recreate_database(&cluster_config(), bare_owner, PW, bare_db)
        .await?;

    // Criterion 4.
    assert!(
        mgr.has_custom_unaccent(&cluster_config(), sh_owner, PW, sh_db)
            .await?,
        "Odoo.sh layout not detected"
    );
    assert!(
        !mgr.has_custom_unaccent(&cluster_config(), std_owner, PW, std_db)
            .await?,
        "operator-prepared unaccent reported as custom"
    );
    assert!(
        !mgr.has_custom_unaccent(&cluster_config(), bare_owner, PW, bare_db)
            .await?,
        "database without unaccent reported as custom"
    );

    cleanup(
        &[std_db, sh_db, bare_db],
        &[std_owner, sh_owner, bare_owner],
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn clones_a_db_with_an_odoosh_unaccent_layout() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_sh", "clone_src_sh_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_sh", "clone_tgt_sh_db");
    let temp_db = "clone_tgt_sh_db_refresh_1";
    setup_odoosh_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    prepare_like_controller(src_owner, src_db, tgt_owner, temp_db).await;

    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Criterion 5.
    assert!(out.status.success(), "clone-db.sh failed:\n{stderr}");
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
            "SELECT provolatile FROM pg_proc WHERE oid = 'public.unaccent(text)'::regprocedure",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(vol, b'i' as i8, "the unaccent wrapper must stay IMMUTABLE");
    let folded: String = c
        .query_one("SELECT unaccent(email) FROM mail_blacklist", &[])
        .await?
        .get(0);
    assert_eq!(folded, "rene@example.com");
    pg_manager()
        .ensure_extensions(&cluster_config(), tgt_owner, PW, tgt_db, true)
        .await?;

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}

#[tokio::test]
async fn preparing_unaccent_regardless_conflicts_with_an_odoosh_source() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_shneg", "clone_src_shneg_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_shneg", "clone_tgt_shneg_db");
    let temp_db = "clone_tgt_shneg_db_refresh_1";
    setup_odoosh_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    // The 2.8.2 preparation, which ignored the source's layout.
    let mgr = pg_manager();
    mgr.recreate_database(&cluster_config(), tgt_owner, PW, temp_db)
        .await?;
    mgr.ensure_extensions(&cluster_config(), tgt_owner, PW, temp_db, true)
        .await?;

    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Criterion 6.
    assert!(!out.status.success(), "clone unexpectedly succeeded");
    assert!(
        stderr.contains(r#"function "unaccent" already exists with same argument types"#),
        "unexpected failure:\n{stderr}"
    );

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}
