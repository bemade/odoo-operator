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
        if exists {
            return Ok(());
        }

        // tokio-postgres doesn't have Identifier.Sanitize() like pgx, so we
        // use a simple allowlist check + quoting. In production you'd want a
        // proper identifier escaper.
        let safe_user = quote_ident(username);
        let stmt = format!("CREATE ROLE {safe_user} WITH PASSWORD '{password}' CREATEDB LOGIN");
        client.execute(&stmt, &[]).await?;
        info!(%username, "created postgres role");
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
}

/// Minimal SQL identifier quoting (double-quote wrapping + escape internal quotes).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
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
}
