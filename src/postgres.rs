use serde::Deserialize;
use tokio_postgres::NoTls;
use tracing::{info, warn};

use crate::error::Result;

/// Per-cluster entry from the postgres-clusters Secret.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostgresClusterConfig {
    pub host: String,
    pub port: i32,
    pub admin_user: String,
    pub admin_password: String,
    #[serde(default)]
    pub default: bool,
}

/// Parameters for provisioning a read-only Postgres role on a tenant database.
/// Bundles the 5 caller-supplied values to stay within clippy's argument-count limit.
pub struct ReadonlyRoleParams<'a> {
    pub ro_username: &'a str,
    pub ro_password: &'a str,
    pub owner_username: &'a str,
    pub owner_password: &'a str,
    pub db_name: &'a str,
    pub connection_limit: i32,
}

/// Trait abstracting PostgreSQL role management so tests can substitute a no-op.
#[async_trait::async_trait]
pub trait PostgresManager: Send + Sync {
    async fn ensure_role(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
    ) -> Result<()>;

    async fn delete_role(&self, pg: &PostgresClusterConfig, username: &str) -> Result<()>;

    /// Returns whether a database with `db_name` exists on the cluster.
    /// Distinguishes "definitely absent" (`Ok(false)`) from "cannot reach
    /// cluster" (`Err(_)`) so callers can act on the former without
    /// false-positive flips on transient outages.
    async fn database_exists(&self, pg: &PostgresClusterConfig, db_name: &str) -> Result<bool>;

    /// Ensure the `report.url` system parameter in the Odoo database points to
    /// the in-cluster web service so that cron-triggered report generation can
    /// reach the wkhtmltopdf endpoint.
    async fn ensure_report_url(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
        report_url: &str,
    ) -> Result<()>;

    /// Query the running PostgreSQL server for its major version (e.g. 16, 17, 18).
    async fn detect_server_major_version(&self, pg: &PostgresClusterConfig) -> Result<u32>;

    /// Ensure a read-only PostgreSQL role `ro_username` exists on the cluster,
    /// with SELECT-only privileges on `db_name`.
    ///
    /// The role is created with LOGIN, NOSUPERUSER, NOCREATEDB and the supplied
    /// `connection_limit`.  The following grants are applied (idempotently):
    ///   - CONNECT on the tenant database
    ///   - USAGE on schema `public`
    ///   - SELECT on all existing tables in schema `public`
    ///   - ALTER DEFAULT PRIVILEGES … GRANT SELECT on future tables
    ///
    /// Explicitly **no** INSERT/UPDATE/DELETE/DDL is granted.
    ///
    /// The method connects as the admin user for role CREATE/ALTER, then as the
    /// tenant owner (`owner_username` / `owner_password`) for per-DB grants
    /// (owner-issued grants are required for per-table SELECT in a tenant DB).
    async fn ensure_readonly_role(
        &self,
        pg: &PostgresClusterConfig,
        params: ReadonlyRoleParams<'_>,
    ) -> Result<()>;

    /// Drop the read-only role `ro_username` if it exists.  No-op if absent.
    /// Revokes existing grants before dropping so the DROP ROLE succeeds even
    /// if other objects hold privileges.
    async fn delete_readonly_role(
        &self,
        pg: &PostgresClusterConfig,
        ro_username: &str,
        db_name: &str,
    ) -> Result<()>;
}

/// Production implementation backed by tokio-postgres.
pub struct PgPostgresManager;

#[async_trait::async_trait]
impl PostgresManager for PgPostgresManager {
    async fn ensure_role(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, pg.admin_user, pg.admin_password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });

        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
                &[&username],
            )
            .await?;
        let exists: bool = row.get(0);
        let safe_user = quote_ident(username);
        // Passwords are random per-instance Secrets; when the role already
        // exists, rotate to the current value so a same-name re-create after
        // a finalizer-blocked delete can authenticate with its fresh Secret
        // (issue #119, part C). But only ALTER when the supplied password
        // does not already authenticate: on PostgreSQL 16+ ALTER ROLE
        // requires the admin to hold ADMIN OPTION on the target role (or be
        // superuser), and an externally managed admin (e.g. CloudNativePG
        // `managed.roles`) does not retain that grant across the external
        // manager's own reconciles — an unconditional ALTER then fails every
        // reconcile with "permission denied to alter role" (issue #128).
        if exists {
            if password_authenticates(pg, username, password).await {
                return Ok(());
            }
            let stmt = format!("ALTER ROLE {safe_user} WITH PASSWORD '{password}'");
            client.execute(&stmt, &[]).await?;
            info!(%username, "reset postgres role password");
            return Ok(());
        }

        let stmt = format!("CREATE ROLE {safe_user} WITH PASSWORD '{password}' CREATEDB LOGIN");
        client.execute(&stmt, &[]).await?;
        info!(%username, "created postgres role");
        Ok(())
    }

    async fn ensure_report_url(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
        report_url: &str,
    ) -> Result<()> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname={}",
            pg.host, pg.port, username, password, db_name
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });

        // Upsert report.url — only writes if the value actually differs.
        let rows_affected = client
            .execute(
                "INSERT INTO ir_config_parameter (key, value, create_uid, create_date, write_uid, write_date) \
                 VALUES ('report.url', $1, 1, now() AT TIME ZONE 'UTC', 1, now() AT TIME ZONE 'UTC') \
                 ON CONFLICT (key) DO UPDATE SET value = $1, write_uid = 1, write_date = now() AT TIME ZONE 'UTC' \
                 WHERE ir_config_parameter.value IS DISTINCT FROM $1",
                &[&report_url],
            )
            .await?;

        if rows_affected > 0 {
            info!(%db_name, %report_url, "set report.url system parameter");
        }

        Ok(())
    }

    async fn delete_role(&self, pg: &PostgresClusterConfig, username: &str) -> Result<()> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, pg.admin_user, pg.admin_password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });

        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
                &[&username],
            )
            .await?;
        let exists: bool = row.get(0);
        if !exists {
            return Ok(());
        }

        // Drop owned databases first.
        let rows = client
            .query(
                "SELECT d.datname FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid \
                 WHERE r.rolname = $1 AND d.datistemplate = false",
                &[&username],
            )
            .await?;

        for row in &rows {
            let db: String = row.get(0);
            let safe_db = quote_ident(&db);
            client
                .execute(&format!("DROP DATABASE {safe_db}"), &[])
                .await?;
            info!(%db, "dropped database");
        }

        let safe_user = quote_ident(username);
        client
            .execute(&format!("DROP ROLE {safe_user}"), &[])
            .await?;
        info!(%username, "deleted postgres role");
        Ok(())
    }

    async fn database_exists(&self, pg: &PostgresClusterConfig, db_name: &str) -> Result<bool> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, pg.admin_user, pg.admin_password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });
        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
                &[&db_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn detect_server_major_version(&self, pg: &PostgresClusterConfig) -> Result<u32> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, pg.admin_user, pg.admin_password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });

        let row = client.query_one("SHOW server_version_num", &[]).await?;
        let raw: String = row.get(0);
        let n: u32 = raw.trim().parse().map_err(|e| {
            crate::error::Error::config(format!("could not parse server_version_num {raw:?}: {e}"))
        })?;
        Ok(n / 10000)
    }

    async fn ensure_readonly_role(
        &self,
        pg: &PostgresClusterConfig,
        params: ReadonlyRoleParams<'_>,
    ) -> Result<()> {
        let ReadonlyRoleParams {
            ro_username,
            ro_password,
            owner_username,
            owner_password,
            db_name,
            connection_limit,
        } = params;
        let safe_ro = quote_ident(ro_username);
        let safe_db = quote_ident(db_name);

        // ── Step 1: Create / update the role (admin connection to postgres) ───
        {
            let connstr = format!(
                "host={} port={} user={} password={} dbname=postgres",
                pg.host, pg.port, pg.admin_user, pg.admin_password
            );
            let (admin, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    warn!("postgres connection error: {e}");
                }
            });

            let row = admin
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
                    &[&ro_username],
                )
                .await?;
            let exists: bool = row.get(0);

            if exists {
                // Update password + connection limit in case they changed.
                admin
                    .execute(
                        &format!(
                            "ALTER ROLE {safe_ro} WITH \
                             LOGIN NOSUPERUSER NOCREATEDB NOINHERIT \
                             CONNECTION LIMIT {connection_limit} \
                             PASSWORD '{ro_password}'"
                        ),
                        &[],
                    )
                    .await?;
            } else {
                admin
                    .execute(
                        &format!(
                            "CREATE ROLE {safe_ro} WITH \
                             LOGIN NOSUPERUSER NOCREATEDB NOINHERIT \
                             CONNECTION LIMIT {connection_limit} \
                             PASSWORD '{ro_password}'"
                        ),
                        &[],
                    )
                    .await?;
                info!(%ro_username, "created read-only postgres role");
            }

            // Grant CONNECT on the tenant database to the read-only role.
            //
            // PUBLIC already holds CONNECT by default, so this is not strictly
            // required for the role to connect — but it is explicit, harmless,
            // and keeps the role working if PUBLIC CONNECT is ever revoked on
            // this database.
            //
            // We deliberately do NOT revoke PUBLIC CONNECT from sibling tenant
            // databases to enforce single-tenant connectivity.  The read-only
            // credential is consumed only from inside the tenant's own pod
            // (e.g. an in-Odoo read-only SQL console that opens its own
            // connection as this role) and is never network-reachable; it also
            // holds no SELECT grants on any other database, so the ability to
            // connect to a sibling DB exposes nothing.  Revoking PUBLIC CONNECT
            // cluster-wide would be a one-way, global side effect triggered by a
            // single tenant's opt-in — out of scope for this role.
            admin
                .execute(
                    &format!("GRANT CONNECT ON DATABASE {safe_db} TO {safe_ro}"),
                    &[],
                )
                .await?;
        }

        // ── Step 2: Schema/table grants — must run as DB owner ────────────────
        {
            let owner_connstr = format!(
                "host={} port={} user={} password={} dbname={}",
                pg.host, pg.port, owner_username, owner_password, db_name
            );
            let (owner_conn, connection) = tokio_postgres::connect(&owner_connstr, NoTls).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    warn!("postgres connection error: {e}");
                }
            });

            owner_conn
                .execute(&format!("GRANT USAGE ON SCHEMA public TO {safe_ro}"), &[])
                .await?;

            owner_conn
                .execute(
                    &format!("GRANT SELECT ON ALL TABLES IN SCHEMA public TO {safe_ro}"),
                    &[],
                )
                .await?;

            // Ensure future tables created by the owner are also readable.
            owner_conn
                .execute(
                    &format!(
                        "ALTER DEFAULT PRIVILEGES IN SCHEMA public \
                         GRANT SELECT ON TABLES TO {safe_ro}"
                    ),
                    &[],
                )
                .await?;

            info!(%ro_username, %db_name, "applied read-only grants");
        }

        Ok(())
    }

    async fn delete_readonly_role(
        &self,
        pg: &PostgresClusterConfig,
        ro_username: &str,
        db_name: &str,
    ) -> Result<()> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, pg.admin_user, pg.admin_password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });

        let row = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
                &[&ro_username],
            )
            .await?;
        let exists: bool = row.get(0);
        if !exists {
            return Ok(());
        }

        let safe_ro = quote_ident(ro_username);
        let safe_db = quote_ident(db_name);

        // ── Step 1: Revoke all per-DB privileges by connecting as admin to
        //   the tenant database.  `DROP OWNED BY` removes:
        //     - GRANT SELECT ON ALL TABLES IN SCHEMA public TO <ro>
        //     - GRANT USAGE ON SCHEMA public TO <ro>
        //     - ALTER DEFAULT PRIVILEGES ... GRANT SELECT ON TABLES TO <ro>
        //   PostgreSQL requires all privileges to be revoked before DROP ROLE
        //   succeeds; doing it via DROP OWNED BY is the safest single command.
        //
        //   We connect as the cluster admin (superuser) to the tenant DB.
        //   Superusers can drop any role's owned objects.  If the tenant DB no
        //   longer exists we skip this step gracefully and proceed to DROP ROLE.
        let tenant_connstr = format!(
            "host={} port={} user={} password={} dbname={}",
            pg.host, pg.port, pg.admin_user, pg.admin_password, db_name
        );
        match tokio_postgres::connect(&tenant_connstr, NoTls).await {
            Ok((tenant_client, connection)) => {
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        warn!("postgres connection error: {e}");
                    }
                });
                // Revoke ALTER DEFAULT PRIVILEGES entries created by the owner
                // for this role — must be done before DROP OWNED BY on some PG
                // versions to avoid catalog inconsistencies.
                let _ = tenant_client
                    .execute(
                        &format!(
                            "ALTER DEFAULT PRIVILEGES IN SCHEMA public \
                             REVOKE SELECT ON TABLES FROM {safe_ro}"
                        ),
                        &[],
                    )
                    .await;
                // DROP OWNED BY revokes all remaining privileges for the role
                // in this database (USAGE, SELECT, etc.).
                let _ = tenant_client
                    .execute(&format!("DROP OWNED BY {safe_ro}"), &[])
                    .await;
            }
            Err(e) => {
                // Tenant DB may have already been dropped; log and continue.
                warn!(%db_name, "could not connect to tenant db for DROP OWNED BY cleanup: {e}");
            }
        }

        // ── Step 2: Revoke CONNECT on the database (admin conn to postgres). ──
        let _ = client
            .execute(
                &format!("REVOKE CONNECT ON DATABASE {safe_db} FROM {safe_ro}"),
                &[],
            )
            .await;

        // ── Step 3: Drop the role. ─────────────────────────────────────────────
        client.execute(&format!("DROP ROLE {safe_ro}"), &[]).await?;
        info!(%ro_username, "deleted read-only postgres role");
        Ok(())
    }
}

/// Minimal SQL identifier quoting (double-quote wrapping + escape internal quotes).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Probe whether `username`/`password` can authenticate against the cluster.
/// Any failure (bad password, unreachable host, missing CONNECT privilege)
/// returns `false`, in which case the caller falls back to ALTER ROLE — the
/// pre-#128 behavior.
async fn password_authenticates(
    pg: &PostgresClusterConfig,
    username: &str,
    password: &str,
) -> bool {
    let connstr = format!(
        "host={} port={} user={} password={} dbname=postgres",
        pg.host, pg.port, username, password
    );
    match tokio_postgres::connect(&connstr, NoTls).await {
        Ok((client, connection)) => {
            tokio::spawn(async move {
                let _ = connection.await;
            });
            client.simple_query("SELECT 1").await.is_ok()
        }
        Err(_) => false,
    }
}

/// No-op implementation for testing.
pub struct NoopPostgresManager;

#[async_trait::async_trait]
impl PostgresManager for NoopPostgresManager {
    async fn ensure_role(&self, _: &PostgresClusterConfig, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn delete_role(&self, _: &PostgresClusterConfig, _: &str) -> Result<()> {
        Ok(())
    }
    async fn database_exists(&self, _: &PostgresClusterConfig, _: &str) -> Result<bool> {
        Ok(true)
    }
    async fn ensure_report_url(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<()> {
        Ok(())
    }
    async fn detect_server_major_version(&self, _: &PostgresClusterConfig) -> Result<u32> {
        Ok(18)
    }
    async fn ensure_readonly_role(
        &self,
        _: &PostgresClusterConfig,
        _: ReadonlyRoleParams<'_>,
    ) -> Result<()> {
        Ok(())
    }
    async fn delete_readonly_role(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
    ) -> Result<()> {
        Ok(())
    }
}
