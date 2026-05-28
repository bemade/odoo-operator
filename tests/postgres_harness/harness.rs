//! Docker-backed PostgreSQL test harness.
//!
//! Spawns a single `postgres:18` container per test binary on a random host
//! port, waits for it to accept connections, and exposes a real
//! `PgPostgresManager` + `PostgresClusterConfig` pointing at it.  The
//! container is reaped on process exit (or via the explicit drop guard;
//! `--rm` ensures docker cleans it up even on panic).

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use odoo_operator::postgres::{PgPostgresManager, PostgresClusterConfig};

pub const ADMIN_USER: &str = "postgres";
pub const ADMIN_PASSWORD: &str = "harness-pw";

struct Harness {
    port: u16,
    container_id: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Best-effort cleanup; `--rm` makes this redundant unless the docker
        // daemon was busy. Ignore errors.
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

static HARNESS: OnceLock<Harness> = OnceLock::new();

fn ensure_docker_available() {
    let ok = Command::new("docker")
        .args(["version", "--format", "{{.Client.Version}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        panic!(
            "docker is not available — the postgres_harness test binary \
             requires docker. Install docker or skip with \
             `cargo test --test integration` (envtest tests only)."
        );
    }
}

fn start_container() -> Harness {
    ensure_docker_available();

    // Use port 0 to ask the kernel for a free port, then read it back from
    // `docker port`. Avoids races between parallel test binaries.
    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "-e",
            &format!("POSTGRES_PASSWORD={ADMIN_PASSWORD}"),
            "-e",
            &format!("POSTGRES_USER={ADMIN_USER}"),
            "-p",
            "127.0.0.1::5432",
            "postgres:18",
        ])
        .output()
        .expect("failed to spawn docker run");
    assert!(
        out.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let container_id = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let port_out = Command::new("docker")
        .args(["port", &container_id, "5432/tcp"])
        .output()
        .expect("failed to spawn docker port");
    let port_line = String::from_utf8_lossy(&port_out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let port: u16 = port_line
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse port from docker port: {port_line:?}"));

    // Wait for the server to accept connections.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            // TCP open ≠ accepting queries; do one query to be sure.
            let probe = std::process::Command::new("docker")
                .args([
                    "exec",
                    &container_id,
                    "pg_isready",
                    "-U",
                    ADMIN_USER,
                    "-h",
                    "127.0.0.1",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if probe.map(|s| s.success()).unwrap_or(false) {
                break;
            }
        }
        if Instant::now() > deadline {
            let logs = Command::new("docker")
                .args(["logs", &container_id])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            panic!("postgres container never became ready. Logs:\n{logs}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    Harness { port, container_id }
}

fn harness() -> &'static Harness {
    HARNESS.get_or_init(start_container)
}

/// Build a `PostgresClusterConfig` pointing at the harness container.
pub fn cluster_config() -> PostgresClusterConfig {
    let h = harness();
    PostgresClusterConfig {
        host: "127.0.0.1".to_string(),
        port: h.port as i32,
        admin_user: ADMIN_USER.to_string(),
        admin_password: ADMIN_PASSWORD.to_string(),
        default: true,
    }
}

/// A `PgPostgresManager` (the real production impl) wired against the
/// harness container.
pub fn pg_manager() -> PgPostgresManager {
    PgPostgresManager
}

/// Connect to the harness as the admin user. Useful for setup/assertion SQL.
pub async fn admin_client() -> tokio_postgres::Client {
    connect_as(ADMIN_USER, ADMIN_PASSWORD, "postgres").await
}

/// Connect to the harness as the given user.
pub async fn connect_as(user: &str, password: &str, dbname: &str) -> tokio_postgres::Client {
    let cfg = cluster_config();
    let connstr = format!(
        "host={} port={} user={} password={} dbname={}",
        cfg.host, cfg.port, user, password, dbname
    );
    let (client, connection) = tokio_postgres::connect(&connstr, tokio_postgres::NoTls)
        .await
        .expect("failed to connect to harness postgres");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Try to connect — returns Err if auth fails, used to verify password changes.
pub async fn try_connect_as(
    user: &str,
    password: &str,
    dbname: &str,
) -> Result<(), tokio_postgres::Error> {
    let cfg = cluster_config();
    let connstr = format!(
        "host={} port={} user={} password={} dbname={}",
        cfg.host, cfg.port, user, password, dbname
    );
    let (client, connection) = tokio_postgres::connect(&connstr, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    // One trivial query to make sure the connection is actually authenticated.
    client.simple_query("SELECT 1").await?;
    Ok(())
}
