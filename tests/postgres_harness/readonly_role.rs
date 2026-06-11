//! Security-critical integration tests for `ensure_readonly_role` and
//! `delete_readonly_role` against a real Docker PostgreSQL instance.
//!
//! Invariants asserted here:
//!   1. Granted role can SELECT but not INSERT / UPDATE / DELETE / DDL.
//!   2. Role cannot read another tenant's data — it holds no SELECT grants
//!      outside its own tenant DB (connectivity to a sibling DB, which PUBLIC
//!      still permits, exposes nothing).
//!   3. `delete_readonly_role` drops the role; subsequent login fails.

use odoo_operator::postgres::{PostgresManager, ReadonlyRoleParams};

use super::harness::{
    admin_client, cluster_config, connect_as, pg_manager, try_connect_as, ADMIN_PASSWORD,
    ADMIN_USER,
};

/// Unique prefix so tests can run in parallel without collisions.
const PREFIX: &str = "odoo.test.ro";

async fn cleanup(ro_user: &str, owner_user: &str, tenant_db: &str, other_db: &str) {
    let c = admin_client().await;
    // Best-effort teardown — order matters to avoid dependency errors.
    let _ = c
        .simple_query(&format!(
            r#"REVOKE ALL PRIVILEGES ON DATABASE "{tenant_db}" FROM "{ro_user}""#
        ))
        .await;
    let _ = c
        .simple_query(&format!(
            r#"REVOKE ALL PRIVILEGES ON DATABASE "{other_db}" FROM "{ro_user}""#
        ))
        .await;
    // Must terminate any open connections to the tenant DB before dropping it.
    let _ = c
        .simple_query(&format!(
            r#"SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
               WHERE datname = '{tenant_db}' AND pid <> pg_backend_pid()"#
        ))
        .await;
    let _ = c
        .simple_query(&format!(
            r#"SELECT pg_terminate_backend(pid)
               FROM pg_stat_activity
               WHERE datname = '{other_db}' AND pid <> pg_backend_pid()"#
        ))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{tenant_db}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{other_db}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{ro_user}""#))
        .await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{owner_user}""#))
        .await;
}

/// Set up a tenant DB owned by `owner_user` with a simple test table.
async fn setup_tenant(owner_user: &str, owner_password: &str, tenant_db: &str) {
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{owner_user}""#))
        .await;
    c.simple_query(&format!(
        r#"CREATE ROLE "{owner_user}" WITH PASSWORD '{owner_password}' LOGIN CREATEDB"#
    ))
    .await
    .expect("create owner role");
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{tenant_db}""#))
        .await;
    c.simple_query(&format!(
        r#"CREATE DATABASE "{tenant_db}" OWNER "{owner_user}""#
    ))
    .await
    .expect("create tenant db");

    // Create a table as the owner so the RO role has something to SELECT.
    let owner_connstr = format!(
        "host=127.0.0.1 port={} user={} password={} dbname={}",
        cluster_config().port,
        owner_user,
        owner_password,
        tenant_db
    );
    let (owner_conn, conn) = tokio_postgres::connect(&owner_connstr, tokio_postgres::NoTls)
        .await
        .expect("connect as owner");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    owner_conn
        .simple_query("CREATE TABLE test_tbl (id serial PRIMARY KEY, val text)")
        .await
        .expect("create test table");
    owner_conn
        .simple_query("INSERT INTO test_tbl (val) VALUES ('hello')")
        .await
        .expect("insert row");
}

// ── Test 1: SELECT allowed; DML/DDL denied ────────────────────────────────────

#[tokio::test]
async fn readonly_role_select_allowed_dml_denied() -> anyhow::Result<()> {
    let ro_user = &format!("{PREFIX}.dml_denied_ro");
    let owner_user = &format!("{PREFIX}.dml_denied_owner");
    let owner_password = "owner-pw-dml";
    let ro_password = "ro-pw-dml";
    let tenant_db = "odoo_test_ro_dml_denied";

    cleanup(ro_user, owner_user, tenant_db, "").await;
    setup_tenant(owner_user, owner_password, tenant_db).await;

    let cfg = cluster_config();
    pg_manager()
        .ensure_readonly_role(
            &cfg,
            ReadonlyRoleParams {
                ro_username: ro_user,
                ro_password,
                owner_username: owner_user,
                owner_password,
                db_name: tenant_db,
                connection_limit: 5,
            },
        )
        .await
        .expect("ensure_readonly_role failed");

    // Connect to the tenant DB as the RO role.
    let ro_connstr = format!(
        "host=127.0.0.1 port={} user={} password={} dbname={}",
        cfg.port, ro_user, ro_password, tenant_db
    );
    let (ro_conn, conn) = tokio_postgres::connect(&ro_connstr, tokio_postgres::NoTls)
        .await
        .expect("ro role should be able to CONNECT to tenant db");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // SELECT must succeed.
    let rows = ro_conn
        .query("SELECT id, val FROM test_tbl", &[])
        .await
        .expect("SELECT should succeed for the read-only role");
    assert!(!rows.is_empty(), "expected at least one row");

    // INSERT must fail with permission denied.
    // tokio_postgres::Error::to_string() returns "db error"; the actual PG
    // message is in as_db_error().message().
    let insert_err = ro_conn
        .simple_query("INSERT INTO test_tbl (val) VALUES ('bad')")
        .await
        .expect_err("INSERT should be denied for the read-only role");
    let insert_msg = insert_err
        .as_db_error()
        .map(|d| d.message().to_lowercase())
        .unwrap_or_else(|| insert_err.to_string().to_lowercase());
    assert!(
        insert_msg.contains("permission denied") || insert_msg.contains("denied"),
        "expected 'permission denied' on INSERT, got: {insert_msg:?}"
    );

    // UPDATE must fail with permission denied.
    let update_err = ro_conn
        .simple_query("UPDATE test_tbl SET val = 'x' WHERE true")
        .await
        .expect_err("UPDATE should be denied for the read-only role");
    let update_msg = update_err
        .as_db_error()
        .map(|d| d.message().to_lowercase())
        .unwrap_or_else(|| update_err.to_string().to_lowercase());
    assert!(
        update_msg.contains("permission denied") || update_msg.contains("denied"),
        "expected 'permission denied' on UPDATE, got: {update_msg:?}"
    );

    // DELETE must fail with permission denied.
    let delete_err = ro_conn
        .simple_query("DELETE FROM test_tbl WHERE true")
        .await
        .expect_err("DELETE should be denied for the read-only role");
    let delete_msg = delete_err
        .as_db_error()
        .map(|d| d.message().to_lowercase())
        .unwrap_or_else(|| delete_err.to_string().to_lowercase());
    assert!(
        delete_msg.contains("permission denied") || delete_msg.contains("denied"),
        "expected 'permission denied' on DELETE, got: {delete_msg:?}"
    );

    // DDL (CREATE TABLE) must fail with permission denied.
    let ddl_err = ro_conn
        .simple_query("CREATE TABLE should_fail (id int)")
        .await
        .expect_err("CREATE TABLE should be denied for the read-only role");
    let ddl_msg = ddl_err
        .as_db_error()
        .map(|d| d.message().to_lowercase())
        .unwrap_or_else(|| ddl_err.to_string().to_lowercase());
    assert!(
        ddl_msg.contains("permission denied") || ddl_msg.contains("denied"),
        "expected 'permission denied' on CREATE TABLE, got: {ddl_msg:?}"
    );

    cleanup(ro_user, owner_user, tenant_db, "").await;
    Ok(())
}

// ── Test 2: Role cannot read another tenant's data ────────────────────────────
//
// We no longer revoke PUBLIC CONNECT cluster-wide, so the RO role *can* connect
// to a sibling tenant DB (PUBLIC grants CONNECT by default).  The boundary that
// actually matters — and that this role does enforce — is that it holds no
// SELECT grants outside its own tenant DB, so it can read nothing there.

#[tokio::test]
async fn readonly_role_cannot_read_other_tenant_data() -> anyhow::Result<()> {
    let ro_user = &format!("{PREFIX}.scoped_ro");
    let owner_user = &format!("{PREFIX}.scoped_owner");
    let owner_password = "owner-pw-scope";
    let ro_password = "ro-pw-scope";
    let tenant_db = "odoo_test_ro_scoped";
    let other_db = "odoo_test_ro_other_tenant";

    cleanup(ro_user, owner_user, tenant_db, other_db).await;
    setup_tenant(owner_user, owner_password, tenant_db).await;

    // Create a second "other tenant" DB with a table holding sensitive data.
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP DATABASE IF EXISTS "{other_db}""#))
        .await;
    c.simple_query(&format!(r#"CREATE DATABASE "{other_db}""#))
        .await
        .expect("create other tenant db");
    let other_admin = connect_as(ADMIN_USER, ADMIN_PASSWORD, other_db).await;
    other_admin
        .simple_query(
            "CREATE TABLE secret_tbl (id serial PRIMARY KEY, val text); \
             INSERT INTO secret_tbl (val) VALUES ('other-tenant-secret')",
        )
        .await
        .expect("create secret table in other tenant db");

    let cfg = cluster_config();
    pg_manager()
        .ensure_readonly_role(
            &cfg,
            ReadonlyRoleParams {
                ro_username: ro_user,
                ro_password,
                owner_username: owner_user,
                owner_password,
                db_name: tenant_db,
                connection_limit: 5,
            },
        )
        .await
        .expect("ensure_readonly_role failed");

    // Connection to the tenant DB must succeed.
    try_connect_as(ro_user, ro_password, tenant_db)
        .await
        .expect("ro role should connect to its own tenant db");

    // The RO role may connect to the other tenant DB (PUBLIC CONNECT), but it
    // must NOT be able to read any data there — no SELECT/USAGE was granted.
    let ro_other = connect_as(ro_user, ro_password, other_db).await;
    let read_err = ro_other
        .simple_query("SELECT val FROM secret_tbl")
        .await
        .expect_err("ro role must NOT be able to read another tenant's data");
    let read_msg = read_err
        .as_db_error()
        .map(|d| d.message().to_lowercase())
        .unwrap_or_else(|| read_err.to_string().to_lowercase());
    assert!(
        read_msg.contains("permission denied") || read_msg.contains("denied"),
        "expected permission denied reading other tenant's table, got: {read_msg:?}"
    );

    cleanup(ro_user, owner_user, tenant_db, other_db).await;
    Ok(())
}

// ── Test 3: delete_readonly_role drops the role; login fails after ────────────

#[tokio::test]
async fn readonly_role_teardown_drops_role() -> anyhow::Result<()> {
    let ro_user = &format!("{PREFIX}.teardown_ro");
    let owner_user = &format!("{PREFIX}.teardown_owner");
    let owner_password = "owner-pw-td";
    let ro_password = "ro-pw-td";
    let tenant_db = "odoo_test_ro_teardown";

    cleanup(ro_user, owner_user, tenant_db, "").await;
    setup_tenant(owner_user, owner_password, tenant_db).await;

    let cfg = cluster_config();
    pg_manager()
        .ensure_readonly_role(
            &cfg,
            ReadonlyRoleParams {
                ro_username: ro_user,
                ro_password,
                owner_username: owner_user,
                owner_password,
                db_name: tenant_db,
                connection_limit: 5,
            },
        )
        .await
        .expect("ensure_readonly_role failed");

    // Confirm the role exists and can connect before teardown.
    try_connect_as(ro_user, ro_password, tenant_db)
        .await
        .expect("ro role should be able to connect before teardown");

    // Delete the role.
    pg_manager()
        .delete_readonly_role(&cfg, ro_user, tenant_db)
        .await
        .expect("delete_readonly_role failed");

    // After deletion, login must fail.
    let login_err = try_connect_as(ro_user, ro_password, tenant_db).await;
    assert!(
        login_err.is_err(),
        "login should fail after delete_readonly_role — role must have been dropped"
    );

    // Verify the role is gone from pg_roles.
    let c = admin_client().await;
    let row = c
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
            &[ro_user],
        )
        .await
        .expect("pg_roles query failed");
    let exists: bool = row.get(0);
    assert!(
        !exists,
        "role must not exist in pg_roles after delete_readonly_role"
    );

    cleanup(ro_user, owner_user, tenant_db, "").await;
    Ok(())
}

// ── Test 4: delete_readonly_role is idempotent (no-op if already absent) ──────

#[tokio::test]
async fn readonly_role_delete_is_idempotent() -> anyhow::Result<()> {
    let ro_user = &format!("{PREFIX}.noop_ro");
    let cfg = cluster_config();

    // Ensure role is absent to start.
    let c = admin_client().await;
    let _ = c
        .simple_query(&format!(r#"DROP ROLE IF EXISTS "{ro_user}""#))
        .await;

    // Calling delete on a non-existent role must be a no-op (not an error).
    pg_manager()
        .delete_readonly_role(&cfg, ro_user, "odoo_nonexistent_db")
        .await
        .expect("delete_readonly_role must be a no-op when role does not exist");

    Ok(())
}
