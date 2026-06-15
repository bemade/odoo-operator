//! odoo-operator — Kubernetes operator for managing Odoo instances.
//!
//! kube-rs preview: all five controllers run as concurrent tokio tasks
//! within a single binary, sharing a common Context.

use std::sync::Arc;

use clap::Parser;
use k8s_openapi::api::core::v1::{Affinity, ResourceRequirements, Toleration};
use kube::runtime::events::Reporter;
use kube::Client;
use tracing::info;
use warp::Filter;

use odoo_operator::controller;
use odoo_operator::helpers::OperatorDefaults;
use odoo_operator::postgres::PgPostgresManager;
use odoo_operator::webhook;

/// Parse a JSON-encoded string into `Option<T>`. Empty / `{}` / `null` → `None`.
fn parse_json_opt<T: serde::de::DeserializeOwned>(
    s: &str,
    field: &str,
) -> anyhow::Result<Option<T>> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed == "null" {
        return Ok(None);
    }
    serde_json::from_str::<T>(trimmed)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("failed to parse --{field} as JSON: {e}"))
}

/// Parse a JSON-encoded array into `Vec<T>`. Empty / `[]` / `null` → empty vec.
fn parse_json_vec<T: serde::de::DeserializeOwned>(s: &str, field: &str) -> anyhow::Result<Vec<T>> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<T>>(trimmed)
        .map_err(|e| anyhow::anyhow!("failed to parse --{field} as JSON array: {e}"))
}

#[derive(Parser, Debug)]
#[command(
    name = "odoo-operator",
    about = "Kubernetes operator for Odoo instances"
)]
struct Args {
    /// Default Odoo container image when spec.image is not set.
    #[arg(long, default_value = "odoo:18.0", env = "DEFAULT_ODOO_IMAGE")]
    default_odoo_image: String,

    /// Default StorageClass for filestore PVCs.
    #[arg(long, default_value = "standard", env = "DEFAULT_STORAGE_CLASS")]
    default_storage_class: String,

    /// Default PVC size for filestore.
    #[arg(long, default_value = "2Gi", env = "DEFAULT_STORAGE_SIZE")]
    default_storage_size: String,

    /// Default IngressClass annotation value.
    #[arg(long, default_value = "", env = "DEFAULT_INGRESS_CLASS")]
    default_ingress_class: String,

    /// Default cert-manager ClusterIssuer.
    #[arg(long, default_value = "", env = "DEFAULT_INGRESS_ISSUER")]
    default_ingress_issuer: String,

    /// Default Gateway name for HTTPRoute parentRef. When both name and namespace
    /// are non-empty, instances without an explicit gatewayRef get one automatically.
    #[arg(long, default_value = "", env = "DEFAULT_GATEWAY_REF_NAME")]
    default_gateway_ref_name: String,

    /// Default Gateway namespace for HTTPRoute parentRef.
    #[arg(long, default_value = "", env = "DEFAULT_GATEWAY_REF_NAMESPACE")]
    default_gateway_ref_namespace: String,

    /// Hostname of the SMTP sink (e.g. Mailpit) that staging instances
    /// should use for outbound mail.  When set, the operator rewrites
    /// the neutralize-sentinel `ir_mail_server` row on every staging
    /// restore / clone to point at this host.  Leave empty to keep the
    /// sentinel (`smtp_host=invalid` — no outbound mail at all).
    /// Only applies when the target OdooInstance has
    /// `spec.environment: Staging`.
    #[arg(long, default_value = "", env = "DEFAULT_STAGING_SMTP_HOST")]
    default_staging_smtp_host: String,

    /// Port for the staging SMTP sink.  Defaults to 1025 (Mailpit default).
    #[arg(long, default_value = "1025", env = "DEFAULT_STAGING_SMTP_PORT")]
    default_staging_smtp_port: u16,

    /// Encryption for the staging SMTP sink: `none`, `ssl`, or `starttls`.
    /// Defaults to `none` (Mailpit doesn't terminate TLS by default).
    #[arg(long, default_value = "none", env = "DEFAULT_STAGING_SMTP_ENCRYPTION")]
    default_staging_smtp_encryption: String,

    /// Default Odoo pod ResourceRequirements (JSON-encoded). Applied to
    /// OdooInstances that don't set `spec.resources`. Empty string disables.
    #[arg(long, default_value = "", env = "DEFAULT_ODOO_RESOURCES")]
    default_odoo_resources: String,

    /// Default Odoo pod node Affinity (JSON-encoded). Applied to
    /// OdooInstances that don't set `spec.affinity`. Empty string disables.
    #[arg(long, default_value = "", env = "DEFAULT_ODOO_AFFINITY")]
    default_odoo_affinity: String,

    /// Default Odoo pod tolerations (JSON-encoded array). Applied to
    /// OdooInstances that don't set `spec.tolerations`. Empty string disables.
    #[arg(long, default_value = "", env = "DEFAULT_ODOO_TOLERATIONS")]
    default_odoo_tolerations: String,

    /// Name of the Secret containing postgres cluster configuration.
    #[arg(
        long,
        default_value = "postgres-clusters",
        env = "POSTGRES_CLUSTERS_SECRET"
    )]
    postgres_clusters_secret: String,

    /// Namespace where the operator is deployed (for reading shared secrets).
    #[arg(long, env = "OPERATOR_NAMESPACE")]
    operator_namespace: String,

    /// Port for the validating webhook HTTPS server.
    #[arg(long, default_value = "9443", env = "WEBHOOK_PORT")]
    webhook_port: u16,

    /// Path to the TLS certificate for the webhook server.
    #[arg(
        long,
        default_value = "/tmp/k8s-webhook-server/serving-certs/tls.crt",
        env = "WEBHOOK_TLS_CERT"
    )]
    webhook_tls_cert: String,

    /// Path to the TLS key for the webhook server.
    #[arg(
        long,
        default_value = "/tmp/k8s-webhook-server/serving-certs/tls.key",
        env = "WEBHOOK_TLS_KEY"
    )]
    webhook_tls_key: String,

    /// Bind address for health probe endpoints (/healthz, /readyz).
    #[arg(long, default_value = ":8081", env = "HEALTH_PROBE_BIND_ADDRESS")]
    health_probe_bind_address: String,

    /// Log format: "text" for human-readable, "json" for structured.
    #[arg(long, default_value = "text", env = "LOG_FORMAT")]
    log_format: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,kube=warn,hyper=warn,tower=warn,warp=warn".into());

    if args.log_format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let client = Client::try_default().await?;

    let default_resources = parse_json_opt::<ResourceRequirements>(
        &args.default_odoo_resources,
        "default-odoo-resources",
    )?;
    let default_affinity =
        parse_json_opt::<Affinity>(&args.default_odoo_affinity, "default-odoo-affinity")?;
    let default_tolerations =
        parse_json_vec::<Toleration>(&args.default_odoo_tolerations, "default-odoo-tolerations")?;

    info!(
        image = %args.default_odoo_image,
        ns = %args.operator_namespace,
        resources = default_resources.is_some(),
        affinity = default_affinity.is_some(),
        tolerations = default_tolerations.len(),
        "starting odoo-operator"
    );

    let ctx = Arc::new(controller::odoo_instance::Context {
        client: client.clone(),
        defaults: OperatorDefaults {
            odoo_image: args.default_odoo_image,
            storage_class: args.default_storage_class,
            storage_size: args.default_storage_size,
            ingress_class: args.default_ingress_class,
            ingress_issuer: args.default_ingress_issuer,
            gateway_ref_name: args.default_gateway_ref_name,
            gateway_ref_namespace: args.default_gateway_ref_namespace,
            staging_smtp_host: args.default_staging_smtp_host,
            staging_smtp_port: args.default_staging_smtp_port,
            staging_smtp_encryption: args.default_staging_smtp_encryption,
            resources: default_resources,
            affinity: default_affinity,
            tolerations: default_tolerations,
        },
        operator_namespace: args.operator_namespace,
        postgres_clusters_secret: args.postgres_clusters_secret,
        postgres: Arc::new(PgPostgresManager),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
        reporter: Reporter {
            controller: "odoo-operator".into(),
            instance: std::env::var("POD_NAME").ok(),
        },
    });

    let webhook_addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.webhook_port));
    let webhook_listener = tokio::net::TcpListener::bind(webhook_addr)
        .await
        .expect("failed to bind webhook listener");
    let tls_cert = args.webhook_tls_cert;
    let tls_key = args.webhook_tls_key;

    // Parse health probe bind address (e.g. ":8081" or "0.0.0.0:8081").
    let health_addr: std::net::SocketAddr = args
        .health_probe_bind_address
        .strip_prefix(':')
        .map(|port| format!("0.0.0.0:{port}"))
        .unwrap_or(args.health_probe_bind_address)
        .parse()
        .expect("invalid --health-probe-bind-address");

    let healthz = warp::get()
        .and(warp::path("healthz"))
        .and(warp::path::end())
        .map(|| warp::reply::with_status("ok", warp::http::StatusCode::OK));
    let readyz = warp::get()
        .and(warp::path("readyz"))
        .and(warp::path::end())
        .map(|| warp::reply::with_status("ok", warp::http::StatusCode::OK));
    let health_routes = healthz.or(readyz);

    // Run the OdooInstance controller (which now absorbs all job lifecycle
    // logic via the state machine), webhook server, and health probes.
    tokio::select! {
        _ = controller::odoo_instance::run(ctx.clone()) => {},
        _ = webhook::run(webhook_listener, &tls_cert, &tls_key) => {},
        _ = warp::serve(health_routes).run(health_addr) => {},
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_opt_handles_empty_and_brackets() {
        assert!(parse_json_opt::<Affinity>("", "x").unwrap().is_none());
        assert!(parse_json_opt::<Affinity>("{}", "x").unwrap().is_none());
        assert!(parse_json_opt::<Affinity>("null", "x").unwrap().is_none());
        assert!(parse_json_opt::<Affinity>("  {}  ", "x").unwrap().is_none());
    }

    #[test]
    fn parse_json_opt_parses_affinity() {
        let json = r#"{"nodeAffinity":{"preferredDuringSchedulingIgnoredDuringExecution":[{"weight":100,"preference":{"matchExpressions":[{"key":"bemade.org/compute","operator":"In","values":["true"]}]}}]}}"#;
        let aff = parse_json_opt::<Affinity>(json, "x").unwrap().unwrap();
        let pref = aff
            .node_affinity
            .unwrap()
            .preferred_during_scheduling_ignored_during_execution
            .unwrap();
        assert_eq!(pref.len(), 1);
        assert_eq!(pref[0].weight, 100);
    }

    #[test]
    fn parse_json_opt_parses_resources() {
        let json = r#"{"requests":{"cpu":"400m","memory":"256Mi"},"limits":{"cpu":"2000m","memory":"4Gi"}}"#;
        let r = parse_json_opt::<ResourceRequirements>(json, "x")
            .unwrap()
            .unwrap();
        assert!(r.requests.is_some());
        assert!(r.limits.is_some());
    }

    #[test]
    fn parse_json_vec_handles_empty() {
        assert!(parse_json_vec::<Toleration>("", "x").unwrap().is_empty());
        assert!(parse_json_vec::<Toleration>("[]", "x").unwrap().is_empty());
        assert!(parse_json_vec::<Toleration>("null", "x")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parse_json_vec_parses_tolerations() {
        let json =
            r#"[{"key":"dedicated","operator":"Equal","value":"odoo","effect":"NoSchedule"}]"#;
        let t = parse_json_vec::<Toleration>(json, "x").unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].key.as_deref(), Some("dedicated"));
    }

    #[test]
    fn parse_json_opt_returns_error_on_invalid() {
        assert!(parse_json_opt::<Affinity>("not json", "x").is_err());
    }
}
