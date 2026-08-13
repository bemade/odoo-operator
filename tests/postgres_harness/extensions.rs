//! Tests for `PostgresManager::ensure_extensions`.
//!
//! ACCEPTANCE CRITERIA
//!
//! 1. Both `pg_trgm` and `unaccent` are created in a tenant database that has
//!    neither. This is the restore/clone case: `scripts/restore-load-db.sh`
//!    builds the database with `createdb` + `pg_restore`, so Odoo's
//!    `_initialize_db` — the only thing that would otherwise create them —
//!    never runs.
//! 2. Creation happens over the TENANT OWNER connection, which is
//!    NOSUPERUSER. Both extensions are trusted on PG13+, so this works; if a
//!    future change routed it through the admin connection instead the
//!    operator would depend on a privilege it should not need.
//! 3. `unaccent` ends up IMMUTABLE (`provolatile = 'i'`) when the admin is a
//!    superuser. This is the non-obvious half: a trusted-extension install
//!    leaves the function owned by the bootstrap superuser, NOT by the role
//!    that ran CREATE EXTENSION, so the tenant owner cannot ALTER it. Odoo
//!    only treats unaccent as INDEXABLE when it is immutable.
//! 4. Calling twice is a no-op and does not error.
//! 5. A database that already has the extensions is left alone and reports
//!    success — the common path, run on every reconcile.
//!
//! 6. When the instance opts out (`spec.database.unaccent: false`), `unaccent`
//!    is NOT created, but `pg_trgm` still is — Odoo creates pg_trgm
//!    unconditionally itself, so the operator matches that rather than
//!    inventing a difference. Opting out must not be a way to end up with a
//!    database Odoo would not have produced on its own.
//!
//! NON-CRITERIA
//!
//! The behaviour when the admin cannot ALTER the function (a non-superuser
//! admin, as on a cluster whose admin role is externally managed) is
//! deliberately not asserted here: it is a best-effort optimisation that
//! logs at debug and returns Ok. Criterion 3 covers the path that matters,
//! and the fallback is a `return Ok(())` with no observable state.

use odoo_operator::postgres::PostgresManager;

use super::harness::{admin_client, cluster_config, connect_as, pg_manager};

const OWNER_PW: &str = "ext-owner-pw";

/// Create a tenant DB owned by a NOSUPERUSER role, with no extensions.
async fn setup_tenant(owner_user: &str, tenant_db: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{tenant_db}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{owner_user}""#))
        .await;
    c.simple_query(&format!(
        r#"CREATE ROLE "{owner_user}" WITH PASSWORD '{OWNER_PW}' LOGIN CREATEDB NOSUPERUSER"#
    ))
    .await
    .expect("create owner role");
    c.simple_query(&format!(
        r#"CREATE DATABASE "{tenant_db}" OWNER "{owner_user}""#
    ))
    .await
    .expect("create tenant db");
}

async fn cleanup(owner_user: &str, tenant_db: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{tenant_db}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{owner_user}""#))
        .await;
}

async fn installed_extensions(owner_user: &str, tenant_db: &str) -> Vec<String> {
    let c = connect_as(owner_user, OWNER_PW, tenant_db).await;
    let rows = c
        .query(
            "SELECT extname FROM pg_extension WHERE extname IN ('pg_trgm', 'unaccent') \
             ORDER BY extname",
            &[],
        )
        .await
        .expect("query pg_extension");
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

async fn unaccent_volatility(owner_user: &str, tenant_db: &str) -> Option<i8> {
    let c = connect_as(owner_user, OWNER_PW, tenant_db).await;
    c.query_opt(
        "SELECT provolatile FROM pg_proc WHERE proname = 'unaccent' AND pronargs = 1 \
           AND pronamespace = current_schema::regnamespace",
        &[],
    )
    .await
    .expect("query pg_proc")
    .map(|r| r.get::<_, i8>(0))
}

#[tokio::test]
async fn creates_both_extensions_in_a_bare_tenant_db() -> anyhow::Result<()> {
    let owner = "odoo_ext_owner_bare";
    let db = "odoo_test_ext_bare";
    setup_tenant(owner, db).await;

    assert!(
        installed_extensions(owner, db).await.is_empty(),
        "precondition: tenant db must start with neither extension"
    );

    pg_manager()
        .ensure_extensions(&cluster_config(), owner, OWNER_PW, db, true)
        .await?;

    // Criteria 1 and 2: created, over a NOSUPERUSER owner connection.
    assert_eq!(
        installed_extensions(owner, db).await,
        vec!["pg_trgm".to_string(), "unaccent".to_string()],
    );

    // Criterion 3: immutable, which the owner alone could not have done.
    assert_eq!(
        unaccent_volatility(owner, db).await,
        Some(b'i' as i8),
        "unaccent must be IMMUTABLE for Odoo to treat it as INDEXABLE"
    );

    cleanup(owner, db).await;
    Ok(())
}

#[tokio::test]
async fn is_idempotent_and_leaves_existing_extensions_alone() -> anyhow::Result<()> {
    let owner = "odoo_ext_owner_idem";
    let db = "odoo_test_ext_idem";
    setup_tenant(owner, db).await;

    let mgr = pg_manager();
    mgr.ensure_extensions(&cluster_config(), owner, OWNER_PW, db, true)
        .await?;
    // Criteria 4 and 5: the every-reconcile path.
    mgr.ensure_extensions(&cluster_config(), owner, OWNER_PW, db, true)
        .await?;

    assert_eq!(
        installed_extensions(owner, db).await,
        vec!["pg_trgm".to_string(), "unaccent".to_string()],
    );
    assert_eq!(unaccent_volatility(owner, db).await, Some(b'i' as i8));

    cleanup(owner, db).await;
    Ok(())
}

#[tokio::test]
async fn tenant_owner_alone_cannot_make_unaccent_immutable() -> anyhow::Result<()> {
    // Not a test of our code, but of the assumption the implementation rests
    // on: that the admin connection is genuinely required for the ALTER. If a
    // future PostgreSQL made trusted extensions owned by their installer, the
    // admin round-trip could be dropped — this test would then fail and say so.
    let owner = "odoo_ext_owner_priv";
    let db = "odoo_test_ext_priv";
    setup_tenant(owner, db).await;

    let c = connect_as(owner, OWNER_PW, db).await;
    c.simple_query("CREATE EXTENSION IF NOT EXISTS unaccent")
        .await
        .expect("trusted extension must be installable by a non-superuser owner");
    let err = c
        .simple_query("ALTER FUNCTION unaccent(text) IMMUTABLE")
        .await
        .expect_err("owner is expected NOT to own a trusted extension's functions");
    // Display for tokio_postgres::Error is just "db error"; the server's
    // message lives in the DbError payload.
    let message = err
        .as_db_error()
        .map(|e| e.message().to_string())
        .unwrap_or_else(|| err.to_string());
    assert!(
        message.contains("must be owner of function"),
        "unexpected failure mode: {message}"
    );

    cleanup(owner, db).await;
    Ok(())
}

#[tokio::test]
async fn skips_unaccent_when_the_instance_opts_out() -> anyhow::Result<()> {
    let owner = "odoo_ext_owner_optout";
    let db = "odoo_test_ext_optout";
    setup_tenant(owner, db).await;

    pg_manager()
        .ensure_extensions(&cluster_config(), owner, OWNER_PW, db, false)
        .await?;

    // Criterion 6: pg_trgm yes, unaccent no.
    assert_eq!(
        installed_extensions(owner, db).await,
        vec!["pg_trgm".to_string()],
        "opting out of unaccent must not also skip pg_trgm"
    );
    assert_eq!(unaccent_volatility(owner, db).await, None);

    cleanup(owner, db).await;
    Ok(())
}
