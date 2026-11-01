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
//!
//! Issue #199 — the live name only ever holds a neutralized database:
//!
//! 7. `clone-db.sh` loads into the temp DB and stops there: the live staging
//!    DB is untouched afterwards, and the clone is in the temp DB.
//! 8. `cutover-db.sh` with `REQUIRE_NEUTRALIZED=true` swaps a neutralized temp
//!    DB into the live name: the live name holds the clone, the old staging
//!    data is gone, and the temp name no longer exists.
//! 9. It refuses a temp DB that is not marked neutralized, and leaves both
//!    the live DB and the temp DB (for the neutralize retry) as they were.
//! 10. With no temp DB to swap in it fails and leaves the live DB as it was.
//! 11. Without `REQUIRE_NEUTRALIZED` (`spec.neutralize: false`) it swaps the
//!     clone in as is, also when the staging instance has no live DB yet.
//! 12. `drop_refresh_temp_databases` drops the target's leftover temp DBs
//!     (`<target>_refresh_<8 hex>`, owned by the tenant role) and nothing
//!     else: not the live DB, not near-miss names, not another role's DB.

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, connect_as, pg_manager, run_script};

const CLONE_DB_SCRIPT: &str = include_str!("../../scripts/clone-db.sh");
const CUTOVER_DB_SCRIPT: &str = include_str!("../../scripts/cutover-db.sh");
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
/// then hold the temp DB open, which no operator reconcile does.
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

    // Criterion 2.  The clone lives in the temp DB until the cutover.
    let c = connect_as(tgt_owner, PW, temp_db).await;
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
    let c = connect_as(tgt_owner, PW, temp_db).await;
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
    drop(c);
    pg_manager()
        .ensure_extensions(&cluster_config(), tgt_owner, PW, temp_db, true)
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

// ── Issue #199: clone into the temp DB, cut over only once neutralized ──────

/// The previous staging database: one marker row the refresh must not touch
/// until the cutover.
async fn setup_old_staging(owner: &str, db: &str) {
    admin_client()
        .await
        .simple_query(&format!(r#"CREATE DATABASE "{db}" OWNER "{owner}""#))
        .await
        .expect("create old staging db");
    connect_as(owner, PW, db)
        .await
        .batch_execute(
            "CREATE TABLE staging_marker (v text);
             INSERT INTO staging_marker VALUES ('old staging');",
        )
        .await
        .expect("seed old staging db");
}

/// What `scripts/neutralize.sh` leaves behind that the cutover checks.  The
/// real neutralize needs `odoo-bin`, which the harness does not have.
async fn mark_neutralized(owner: &str, db: &str) {
    connect_as(owner, PW, db)
        .await
        .batch_execute(
            "CREATE TABLE ir_config_parameter (key varchar PRIMARY KEY, value text);
             INSERT INTO ir_config_parameter VALUES ('database.is_neutralized', 'true');",
        )
        .await
        .expect("mark temp db neutralized");
}

async fn db_exists(db: &str) -> bool {
    admin_client()
        .await
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&db])
        .await
        .expect("query pg_database")
        .is_some()
}

async fn has_table(owner: &str, db: &str, table: &str) -> bool {
    connect_as(owner, PW, db)
        .await
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
        .await
        .expect("probe table")
        .get(0)
}

/// The harness's own connections close asynchronously after their client is
/// dropped; wait them out so they cannot hold the temp DB open for the
/// rename.  In the cluster nothing connects to the temp DB at cutover time:
/// the neutralize container has exited before the cutover container starts.
async fn wait_for_no_sessions(db: &str) {
    let c = admin_client().await;
    for _ in 0..100 {
        let n: i64 = c
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = $1",
                &[&db],
            )
            .await
            .expect("count sessions")
            .get(0);
        if n == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("sessions on {db} never closed");
}

async fn run_cutover(
    owner: &'static str,
    tgt_db: &'static str,
    temp_db: &'static str,
    require_neutralized: bool,
) -> std::process::Output {
    wait_for_no_sessions(temp_db).await;
    tokio::task::spawn_blocking(move || {
        let mut envs = vec![
            ("HOST", "127.0.0.1"),
            ("PORT", "5432"),
            ("USER", owner),
            ("PASSWORD", PW),
            ("TEMP_DB", temp_db),
            ("TGT_DB", tgt_db),
        ];
        if require_neutralized {
            envs.push(("REQUIRE_NEUTRALIZED", "true"));
        }
        run_script(CUTOVER_DB_SCRIPT, &envs)
    })
    .await
    .expect("cutover task panicked")
}

/// Criteria 7 and 8: the clone stops at the temp DB, and the cutover swaps
/// it into the live name once it is neutralized.
#[tokio::test]
async fn clone_leaves_live_db_until_cutover_swaps_it() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_cut", "clone_src_cut_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_cut", "clone_tgt_cut_db");
    let temp_db = "clone_tgt_cut_db_refresh_1";
    setup_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    setup_old_staging(tgt_owner, tgt_db).await;
    prepare_like_controller(src_owner, src_db, tgt_owner, temp_db).await;

    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    assert!(
        out.status.success(),
        "clone-db.sh failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Criterion 7.
    assert!(
        has_table(tgt_owner, tgt_db, "staging_marker").await,
        "the clone touched the live staging DB"
    );
    assert!(
        !has_table(tgt_owner, tgt_db, "mail_blacklist").await,
        "the clone landed in the live staging DB before neutralize"
    );
    assert!(
        has_table(tgt_owner, temp_db, "mail_blacklist").await,
        "the clone is missing from the temp DB"
    );

    // Criterion 8.
    mark_neutralized(tgt_owner, temp_db).await;
    let out = run_cutover(tgt_owner, tgt_db, temp_db, true).await;
    assert!(
        out.status.success(),
        "cutover-db.sh failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(has_table(tgt_owner, tgt_db, "mail_blacklist").await);
    assert!(!has_table(tgt_owner, tgt_db, "staging_marker").await);
    assert!(!db_exists(temp_db).await, "temp DB survived the cutover");

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}

/// Criterion 9.
#[tokio::test]
async fn cutover_refuses_a_temp_db_that_is_not_neutralized() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_cutneg", "clone_src_cutneg_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_cutneg", "clone_tgt_cutneg_db");
    let temp_db = "clone_tgt_cutneg_db_refresh_1";
    setup_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    setup_old_staging(tgt_owner, tgt_db).await;
    prepare_like_controller(src_owner, src_db, tgt_owner, temp_db).await;
    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    assert!(out.status.success(), "clone-db.sh failed");

    let out = run_cutover(tgt_owner, tgt_db, temp_db, true).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "cutover of an un-neutralized DB succeeded"
    );
    assert!(stdout.contains("CRITICAL"), "unexpected failure:\n{stdout}");
    assert!(has_table(tgt_owner, tgt_db, "staging_marker").await);
    assert!(
        has_table(tgt_owner, temp_db, "mail_blacklist").await,
        "the temp DB must survive for the neutralize retry"
    );

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}

/// Criterion 10.
#[tokio::test]
async fn cutover_without_a_temp_db_keeps_the_live_db() -> anyhow::Result<()> {
    let (tgt_owner, tgt_db) = ("clone_tgt_cutnone", "clone_tgt_cutnone_db");
    let temp_db = "clone_tgt_cutnone_db_refresh_1";
    reset_role_and_db(tgt_owner, tgt_db).await;
    setup_old_staging(tgt_owner, tgt_db).await;

    let out = run_cutover(tgt_owner, tgt_db, temp_db, false).await;
    assert!(!out.status.success(), "cutover without a temp DB succeeded");
    assert!(has_table(tgt_owner, tgt_db, "staging_marker").await);

    cleanup(&[tgt_db], &[tgt_owner]).await;
    Ok(())
}

/// Criterion 11.
#[tokio::test]
async fn cutover_without_neutralize_swaps_onto_a_missing_live_db() -> anyhow::Result<()> {
    let (src_owner, src_db) = ("clone_src_cutraw", "clone_src_cutraw_db");
    let (tgt_owner, tgt_db) = ("clone_tgt_cutraw", "clone_tgt_cutraw_db");
    let temp_db = "clone_tgt_cutraw_db_refresh_1";
    setup_source(src_owner, src_db).await;
    reset_role_and_db(tgt_owner, tgt_db).await;
    prepare_like_controller(src_owner, src_db, tgt_owner, temp_db).await;
    let out = run_clone(src_owner, src_db, tgt_owner, tgt_db, temp_db).await;
    assert!(out.status.success(), "clone-db.sh failed");
    assert!(!db_exists(tgt_db).await);

    let out = run_cutover(tgt_owner, tgt_db, temp_db, false).await;
    assert!(
        out.status.success(),
        "cutover-db.sh failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(has_table(tgt_owner, tgt_db, "mail_blacklist").await);
    assert!(!db_exists(temp_db).await);

    cleanup(&[src_db, tgt_db, temp_db], &[src_owner, tgt_owner]).await;
    Ok(())
}

/// Criterion 12.
#[tokio::test]
async fn drops_only_the_targets_leftover_temp_dbs() -> anyhow::Result<()> {
    let (owner, live) = ("clone_sweep", "clone_sweep_db");
    let other = "clone_sweep_other";
    reset_role_and_db(owner, live).await;
    reset_role_and_db(other, "clone_sweep_db_refresh_aaaaaaaa").await;
    let stale = [
        "clone_sweep_db_refresh_0123abcd",
        "clone_sweep_db_refresh_deadbeef",
    ];
    let kept = [
        live,
        "clone_sweep_db_refresh_0123abcz",
        "clone_sweep_db_refresh_0123abcd9",
        "clone_sweep_dbx_refresh_0123abcd",
    ];
    let admin = admin_client().await;
    for db in stale.iter().chain(kept.iter()) {
        admin
            .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
            .await?;
        admin
            .simple_query(&format!(r#"CREATE DATABASE "{db}" OWNER "{owner}""#))
            .await?;
    }
    let foreign = "clone_sweep_db_refresh_aaaaaaaa";
    admin
        .simple_query(&format!(r#"CREATE DATABASE "{foreign}" OWNER "{other}""#))
        .await?;

    let mut dropped = pg_manager()
        .drop_refresh_temp_databases(&cluster_config(), owner, PW, live)
        .await?;
    dropped.sort();
    assert_eq!(dropped, stale.to_vec());
    for db in stale {
        assert!(!db_exists(db).await, "{db} survived the sweep");
    }
    for db in kept.iter().chain([&foreign]) {
        assert!(db_exists(db).await, "{db} must not be swept");
    }

    let mut all: Vec<&str> = kept.to_vec();
    all.push(foreign);
    cleanup(&all, &[owner, other]).await;
    Ok(())
}
