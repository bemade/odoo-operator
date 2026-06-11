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
}
