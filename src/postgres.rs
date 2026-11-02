use serde::Deserialize;
use tokio_postgres::error::SqlState;
use tokio_postgres::NoTls;
use tracing::{debug, info, warn};

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

    /// Drop `db_name` if it exists and create it empty, owned by `username`.
    ///
    /// Connects as that tenant role (a CREATEDB role), so the database is
    /// owned by the role that will later load into it. Used to prepare the
    /// temp database of a staging refresh before `scripts/clone-db.sh` runs,
    /// so that `ensure_extensions` — whose IMMUTABLE step needs the admin
    /// connection the clone Job does not have — can run on it first.
    async fn recreate_database(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
    ) -> Result<()>;

    /// Drop the leftover temp databases of `target_db`'s staging refreshes:
    /// every database named like [`crate::helpers::refresh_temp_db_name`]
    /// gives `target_db` that `username` owns.  Returns the names dropped.
    ///
    /// Connects as that tenant role, so it can only ever drop the tenant's
    /// own databases.  A refresh whose final Job failed keeps its temp DB for
    /// the neutralize retry, and a refresh CR can be deleted or pruned with
    /// its temp DB still present; the next refresh of the instance sweeps
    /// them before it loads its own.
    async fn drop_refresh_temp_databases(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        target_db: &str,
    ) -> Result<Vec<String>>;

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

    /// Ensure the PostgreSQL extensions Odoo relies on exist in the tenant
    /// database.
    ///
    /// Odoo creates `pg_trgm` unconditionally, and `unaccent` only when the
    /// `--unaccent` flag is set, in `_initialize_db` — i.e. only for databases
    /// Odoo itself creates. Databases that arrive by restore or clone (see
    /// `scripts/restore-load-db.sh`, which uses `createdb` + `pg_restore`) never
    /// run that path, so either extension can be missing.
    ///
    /// Without `unaccent`, `registry.has_unaccent` is false and Odoo silently
    /// loses accent-insensitive search and deduplication across the whole
    /// database. Reconciling it here rather than in the odoo.conf template is
    /// what covers the restore and clone paths, and lets existing instances
    /// self-heal.
    ///
    /// Both are trusted extensions on PG13+, so the tenant owner may create
    /// them. Marking `unaccent` IMMUTABLE requires *ownership* of the function,
    /// which a trusted-extension install leaves with the bootstrap superuser —
    /// so that step uses the admin connection and is best-effort. Without it
    /// Odoo reports the function PRESENT rather than INDEXABLE: fully
    /// functional for search, but unusable as the basis of a trigram index.
    async fn ensure_extensions(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
        with_unaccent: bool,
    ) -> Result<()>;

    /// Whether `db_name` provides `unaccent` some other way than the
    /// `unaccent` extension installed in its current schema — the layout
    /// `ensure_extensions` produces.
    ///
    /// Odoo.sh, for one, installs the extension in a schema of its own and
    /// reaches it through a plain IMMUTABLE SQL `public.unaccent(text)`
    /// wrapper, which Odoo's trigram indexes are then built on. A dump of such
    /// a database defines that wrapper itself, so a target that already got
    /// the extension in `public` rejects it ("function unaccent already
    /// exists"). The staging refresh asks this of the source to decide whether
    /// to prepare the temp DB with unaccent or leave it to the dump.
    ///
    /// Connects as `username`, the role the dump will run as, so the current
    /// schema is the one the dump and the target resolve against.
    async fn has_custom_unaccent(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
    ) -> Result<bool>;

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
        //
        // Only an authentication failure is evidence that the password is
        // wrong. Any other probe failure (server saturated, briefly
        // unreachable, ...) says nothing about the password, so skip the
        // ALTER and let the next reconcile re-check; resetting on such
        // errors re-salted every tenant's password whenever the server was
        // overloaded, and hit the #128 failure on top.
        if exists {
            match probe_password(pg, username, password).await {
                PasswordProbe::Authenticates => return Ok(()),
                PasswordProbe::Rejected => {}
                PasswordProbe::Inconclusive(e) => {
                    warn!(
                        %username,
                        "could not verify postgres role password, leaving it \
                         unchanged until the next reconcile: {e}"
                    );
                    return Ok(());
                }
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

    async fn ensure_extensions(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
        with_unaccent: bool,
    ) -> Result<()> {
        // Tenant owner, not admin: the owner has CREATE on its own database,
        // and both extensions are trusted, so no elevated connection is needed
        // to install them.
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

        // Check before creating so that the common case (both present) issues
        // no DDL and logs nothing — this runs on every reconcile.
        let rows = client
            .query(
                "SELECT extname FROM pg_extension WHERE extname IN ('pg_trgm', 'unaccent')",
                &[],
            )
            .await?;
        let present: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();

        // pg_trgm unconditionally, matching Odoo's own _initialize_db;
        // unaccent only when the instance opts in.
        let wanted: &[&str] = if with_unaccent {
            &["pg_trgm", "unaccent"]
        } else {
            &["pg_trgm"]
        };
        for ext in wanted.iter().copied() {
            if present.iter().any(|e| e == ext) {
                continue;
            }
            // Extension names here are hardcoded literals, not user input.
            match client
                .execute(&format!("CREATE EXTENSION IF NOT EXISTS {ext}"), &[])
                .await
            {
                Ok(_) => info!(%db_name, %ext, "created postgres extension"),
                Err(e) => {
                    // Non-fatal: a missing extension degrades Odoo's search,
                    // it does not stop the instance from serving.
                    warn!(%db_name, %ext, %e, "failed to create postgres extension");
                }
            }
        }

        if !with_unaccent {
            return Ok(());
        }

        // `unaccent` must be IMMUTABLE to be indexable. provolatile is 'i' when
        // immutable, 's' (stable) as shipped.
        let volatility = client
            .query_opt(
                "SELECT provolatile FROM pg_proc \
                 WHERE proname = 'unaccent' AND pronargs = 1 \
                   AND pronamespace = current_schema::regnamespace",
                &[],
            )
            .await?
            .map(|r| r.get::<_, i8>(0));
        if volatility == Some(b'i' as i8) {
            return Ok(());
        }

        // Needs function ownership, which the tenant role does not have.
        let admin_connstr = format!(
            "host={} port={} user={} password={} dbname={}",
            pg.host, pg.port, pg.admin_user, pg.admin_password, db_name
        );
        let (admin, admin_conn) = match tokio_postgres::connect(&admin_connstr, NoTls).await {
            Ok(pair) => pair,
            Err(e) => {
                debug!(%db_name, %e, "no admin connection for unaccent IMMUTABLE; leaving it stable");
                return Ok(());
            }
        };
        tokio::spawn(async move {
            if let Err(e) = admin_conn.await {
                warn!("postgres admin connection error: {e}");
            }
        });
        match admin
            .execute("ALTER FUNCTION unaccent(text) IMMUTABLE", &[])
            .await
        {
            Ok(_) => info!(%db_name, "marked unaccent() IMMUTABLE"),
            // Purely an optimisation: without it Odoo still uses unaccent for
            // search, it just cannot build trigram indexes over it. Logged at
            // debug so a cluster whose admin lacks the privilege does not warn
            // on every reconcile.
            Err(e) => debug!(%db_name, %e, "could not mark unaccent() IMMUTABLE"),
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

    async fn has_custom_unaccent(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
    ) -> Result<bool> {
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
        // Custom when the extension lives outside the current schema, or when
        // something in the current schema named unaccent is not an extension
        // member (a wrapper, whichever extension it calls into).
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_extension \
                                 WHERE extname = 'unaccent' \
                                   AND extnamespace <> current_schema::regnamespace) \
                     OR EXISTS (SELECT 1 FROM pg_proc p \
                                 WHERE p.proname = 'unaccent' \
                                   AND p.pronamespace = current_schema::regnamespace \
                                   AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                                                    WHERE d.classid = 'pg_proc'::regclass \
                                                      AND d.objid = p.oid \
                                                      AND d.deptype = 'e'))",
                &[],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn recreate_database(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        db_name: &str,
    ) -> Result<()> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, username, password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });
        let safe_db = quote_ident(db_name);
        // Separate statements: neither may run inside a transaction block.
        client
            .execute(
                &format!("DROP DATABASE IF EXISTS {safe_db} WITH (FORCE)"),
                &[],
            )
            .await?;
        client
            .execute(&format!("CREATE DATABASE {safe_db}"), &[])
            .await?;
        info!(%db_name, owner = %username, "recreated empty database");
        Ok(())
    }

    async fn drop_refresh_temp_databases(
        &self,
        pg: &PostgresClusterConfig,
        username: &str,
        password: &str,
        target_db: &str,
    ) -> Result<Vec<String>> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            pg.host, pg.port, username, password
        );
        let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("postgres connection error: {e}");
            }
        });
        let rows = client
            .query(
                "SELECT datname::text FROM pg_database \
                  WHERE datdba = (SELECT oid FROM pg_roles WHERE rolname = current_user) \
                    AND left(datname, length($1)) = $1",
                &[&target_db],
            )
            .await?;
        let mut dropped = Vec::new();
        for row in rows {
            let name: String = row.get(0);
            if !crate::helpers::is_refresh_temp_db_name(target_db, &name) {
                continue;
            }
            client
                .execute(
                    &format!(
                        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                        quote_ident(&name)
                    ),
                    &[],
                )
                .await?;
            info!(db_name = %name, owner = %username, "dropped leftover staging-refresh temp database");
            dropped.push(name);
        }
        Ok(dropped)
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

/// Outcome of logging in as a tenant role with the password the operator
/// holds for it.
enum PasswordProbe {
    /// The login succeeded: the password is current.
    Authenticates,
    /// The server checked the password and rejected it.
    Rejected,
    /// The login failed for any other reason, so the password's validity is
    /// unknown. Wrapped in the crate error so it logs the server's message
    /// rather than tokio-postgres's bare "db error".
    Inconclusive(crate::error::Error),
}

/// Probe whether `username`/`password` can authenticate against the cluster.
///
/// Only SQLSTATE 28P01 (`invalid_password`) counts as [`PasswordProbe::Rejected`]:
/// Postgres raises it exactly when a password-based method (`scram-sha-256`,
/// `md5`, `password`) compared the supplied password with the role's stored
/// one and they differed, which is the only failure `ALTER ROLE ... WITH
/// PASSWORD` can fix. 28000 (`invalid_authorization_specification`) is
/// deliberately inconclusive: it covers a pg_hba.conf rejection (no matching
/// entry, `reject`, a TLS-only entry), where the password is never checked;
/// non-password methods (LDAP, RADIUS, cert, ...), which do not consult the
/// stored password; and a NOLOGIN role, rejected only after the password was
/// accepted. Every other error (connection refused, timeouts, `too many
/// clients`, reserved slots, per-role connection limits, missing CONNECT
/// privilege, ...) is inconclusive too.
async fn probe_password(
    pg: &PostgresClusterConfig,
    username: &str,
    password: &str,
) -> PasswordProbe {
    let connstr = format!(
        "host={} port={} user={} password={} dbname=postgres",
        pg.host, pg.port, username, password
    );
    // `connect` resolves only after the server has authenticated the role
    // and finished session startup, so success means the password is current.
    match tokio_postgres::connect(&connstr, NoTls).await {
        Ok((_client, connection)) => {
            tokio::spawn(async move {
                let _ = connection.await;
            });
            PasswordProbe::Authenticates
        }
        Err(e) if e.code() == Some(&SqlState::INVALID_PASSWORD) => PasswordProbe::Rejected,
        Err(e) => PasswordProbe::Inconclusive(e.into()),
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
    async fn recreate_database(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<()> {
        Ok(())
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
    async fn ensure_extensions(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
        _: &str,
        _: bool,
    ) -> Result<()> {
        Ok(())
    }
    async fn has_custom_unaccent(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<bool> {
        Ok(false)
    }
    async fn drop_refresh_temp_databases(
        &self,
        _: &PostgresClusterConfig,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Vec<String>> {
        Ok(vec![])
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
