//! odoo-operator — Kubernetes operator for managing Odoo instances.
//!
//! kube-rs preview: all five controllers run as concurrent tokio tasks
//! within a single binary, sharing a common Context.

use std::sync::Arc;

use clap::Parser;
use kube::runtime::events::Reporter;
use kube::Client;
use tracing::info;
use warp::Filter;

use odoo_operator::controller;
use odoo_operator::helpers::OperatorDefaults;
use odoo_operator::postgres::PgPostgresManager;
use odoo_operator::webhook;

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

    info!(
        image = %args.default_odoo_image,
        ns = %args.operator_namespace,
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
            ..Default::default()
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
        _ = webhook::run(webhook_addr, &tls_cert, &tls_key) => {},
        _ = warp::serve(health_routes).run(health_addr) => {},
    }

    Ok(())
}
