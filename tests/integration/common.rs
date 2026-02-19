//! Shared test harness and helpers for envtest-based integration tests.
//!
//! A single envtest server + controller is shared across all tests in this
//! binary.  Each test gets its own Kubernetes namespace for isolation, so
//! tests can run in parallel.
//!
//! There is no kubelet or scheduler in envtest, so Deployment status and Job
//! completion must be faked by patching status subresources.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use envtest::Environment;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::config::KubeConfigOptions;
use kube::runtime::events::Reporter;
use kube::{Client, Config, CustomResourceExt};
use serde_json::json;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

use odoo_operator::controller::helpers::FIELD_MANAGER;
use odoo_operator::controller::odoo_instance::Context;
use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
use odoo_operator::crd::odoo_init_job::OdooInitJob;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use odoo_operator::crd::odoo_restore_job::OdooRestoreJob;
use odoo_operator::crd::odoo_upgrade_job::OdooUpgradeJob;
use odoo_operator::helpers::OperatorDefaults;
use odoo_operator::postgres::NoopPostgresManager;

pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const POLL: Duration = Duration::from_millis(500);

/// Counter for generating unique namespace names.
static NS_COUNTER: AtomicU32 = AtomicU32::new(0);

// ═══════════════════════════════════════════════════════════════════════════════
// Shared environment (one envtest server + controller per test binary)
// ═══════════════════════════════════════════════════════════════════════════════

/// Holds the envtest server, a kube Client, and the tokio runtime that drives
/// the controller.  All three live for the entire process.
struct SharedEnv {
    client: Client,
    // The runtime keeps the controller task and kube HTTP connections alive
    // across individual `#[tokio::test]` runtimes.
    _runtime: tokio::runtime::Runtime,
    _server: envtest::Server,
}

// SAFETY: envtest::Server is just a String wrapper (kubeconfig) — Send+Sync.
unsafe impl Send for SharedEnv {}
unsafe impl Sync for SharedEnv {}

/// Singleton — initialised on first use, never torn down (process exit cleans up).
static SHARED: OnceLock<SharedEnv> = OnceLock::new();

fn init_shared() -> SharedEnv {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn,odoo_operator=debug"))
        .try_init();

    // Build a multi-threaded runtime that outlives every `#[tokio::test]`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build shared runtime");

    let (client, server) = rt.block_on(async {
        let mut env = Environment::default();
        let env = env
            .with_crds(vec![
                OdooInstance::crd(),
                OdooInitJob::crd(),
                OdooBackupJob::crd(),
                OdooRestoreJob::crd(),
                OdooUpgradeJob::crd(),
            ])
            .expect("failed to configure CRDs");

        let server = env.create().expect("failed to start envtest server");
        let kubeconfig = server.kubeconfig().expect("failed to get kubeconfig");
        let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
            .await
            .expect("failed to build config");
        let client = Client::try_from(config).expect("failed to create client");

        // Create the postgres-clusters secret in the operator namespace ("default").
        let secrets: Api<Secret> = Api::namespaced(client.clone(), "default");
        let pg_secret: Secret = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "postgres-clusters", "namespace": "default" },
            "stringData": {
                "clusters.yaml": serde_yaml::to_string(&json!({
                    "default": {
                        "host": "localhost",
                        "port": 5432,
                        "adminUser": "postgres",
                        "adminPassword": "postgres",
                        "default": true
                    }
                })).unwrap()
            }
        }))
        .unwrap();
        secrets
            .create(&PostParams::default(), &pg_secret)
            .await
            .expect("failed to create postgres-clusters secret");

        (client, server)
    });

    // Spawn the controller on the shared runtime so it outlives test runtimes.
    let ctx = test_context(client.clone());
    rt.spawn(async move {
        odoo_operator::controller::odoo_instance::run(ctx).await;
    });

    SharedEnv {
        client,
        _runtime: rt,
        _server: server,
    }
}

/// Get (or create) the shared envtest environment.
///
/// Initialization runs on a dedicated OS thread to avoid the "cannot start a
/// runtime from within a runtime" panic that would occur if `block_on` were
/// called from inside a `#[tokio::test]` context.
fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        std::thread::spawn(init_shared)
            .join()
            .expect("shared env init thread panicked")
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Per-test context
// ═══════════════════════════════════════════════════════════════════════════════

/// Per-test context: owns a unique namespace and provides a kube Client.
pub struct TestContext {
    pub client: Client,
    pub ns: String,
}

impl TestContext {
    /// Create a new test context with 1 replica (the common case).
    pub async fn new(instance_name: &str) -> Self {
        Self::new_with_replicas(instance_name, 1).await
    }

    /// Create a new test context with a custom replica count.
    pub async fn new_with_replicas(instance_name: &str, replicas: i32) -> Self {
        let env = shared();
        let client = env.client.clone();

        // Unique namespace per test.
        let id = NS_COUNTER.fetch_add(1, Ordering::SeqCst);
        let ns = format!("test-{id}");

        // Create the namespace.
        let ns_api: Api<Namespace> = Api::all(client.clone());
        let ns_obj: Namespace = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": &ns }
        }))
        .unwrap();
        ns_api
            .create(&PostParams::default(), &ns_obj)
            .await
            .expect("failed to create test namespace");

        // Create the OdooInstance.
        let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
        let inst: OdooInstance =
            serde_json::from_value(test_instance_json(instance_name, &ns, replicas)).unwrap();
        api.create(&PostParams::default(), &inst)
            .await
            .expect("failed to create OdooInstance");

        Self { client, ns }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

fn test_instance_json(name: &str, ns: &str, replicas: i32) -> serde_json::Value {
    json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "replicas": replicas,
            "cron": {
                "replicas": 1,
            },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["test.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": {
                "storageSize": "1Gi",
                "storageClass": "standard",
            },
        }
    })
}

fn test_defaults() -> OperatorDefaults {
    OperatorDefaults {
        odoo_image: "odoo:18.0".into(),
        storage_class: "standard".into(),
        storage_size: "2Gi".into(),
        ingress_issuer: "letsencrypt".into(),
        ingress_class: "nginx".into(),
        resources: None,
        affinity: None,
        tolerations: vec![],
    }
}

fn test_context(client: Client) -> Arc<Context> {
    Arc::new(Context {
        client,
        defaults: test_defaults(),
        operator_namespace: "default".into(),
        postgres_clusters_secret: "".into(),
        postgres: Arc::new(NoopPostgresManager),
        http_client: reqwest::Client::new(),
        reporter: Reporter {
            controller: "odoo-operator-test".into(),
            instance: None,
        },
    })
}

/// Poll until a condition is true, or timeout.
pub async fn wait_for<F, Fut>(timeout: Duration, interval: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    loop {
        if check().await {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Wait until the OdooInstance reaches the expected phase.
pub async fn wait_for_phase(
    client: &Client,
    ns: &str,
    name: &str,
    expected: OdooInstancePhase,
) -> bool {
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    wait_for(TIMEOUT, POLL, || {
        let api = api.clone();
        let expected = expected.clone();
        let name = name.to_string();
        async move {
            api.get_status(&name)
                .await
                .ok()
                .and_then(|i| i.status)
                .and_then(|s| s.phase)
                == Some(expected)
        }
    })
    .await
}

/// Wait for a Deployment to exist, then patch its status to simulate readyReplicas.
pub async fn fake_deployment_ready(client: &Client, ns: &str, name: &str, replicas: i32) {
    let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = deps.clone();
            let n = name.to_string();
            async move { api.get(&n).await.is_ok() }
        })
        .await,
        "deployment {name} never appeared"
    );
    let patch = json!({ "status": { "readyReplicas": replicas, "replicas": replicas } });
    deps.patch_status(
        name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .expect("failed to patch deployment status");
}

/// Continuously patch Deployment readyReplicas in the background.
/// Returns a JoinHandle that should be aborted when no longer needed.
pub fn keep_deployment_ready(
    client: Client,
    ns: String,
    name: String,
    replicas: i32,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let deps: Api<Deployment> = Api::namespaced(client, &ns);
        let patch = json!({ "status": { "readyReplicas": replicas, "replicas": replicas } });
        loop {
            let _ = deps
                .patch_status(
                    &name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

/// Wait for a job CRD to have its `status.jobName` set, then return the job name.
pub async fn wait_for_k8s_job_name<T>(client: &Client, ns: &str, crd_name: &str) -> String
where
    T: kube::Resource<DynamicType = (), Scope = kube::core::NamespaceResourceScope>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + serde::Serialize,
{
    let api: Api<T> = Api::namespaced(client.clone(), ns);
    let start = std::time::Instant::now();
    loop {
        if let Ok(obj) = api.get(crd_name).await {
            let val = serde_json::to_value(&obj).unwrap();
            if let Some(name) = val.pointer("/status/jobName").and_then(|v| v.as_str()) {
                return name.to_string();
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "job CRD {crd_name} never got a jobName in status"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Patch a batch/v1 Job's status to simulate success (succeeded=1).
pub async fn fake_job_succeeded(client: &Client, ns: &str, job_name: &str) {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "status": { "succeeded": 1 } });
    jobs.patch_status(
        job_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .expect("failed to patch job status");
}

/// Patch a batch/v1 Job's status to simulate failure (failed=1).
pub async fn fake_job_failed(client: &Client, ns: &str, job_name: &str) {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "status": { "failed": 1 } });
    jobs.patch_status(
        job_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .expect("failed to patch job status (failure)");
}

/// Patch the OdooInstance spec (e.g. replicas).
pub async fn patch_instance_spec(
    client: &Client,
    ns: &str,
    name: &str,
    spec_patch: serde_json::Value,
) {
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "spec": spec_patch });
    api.patch(
        name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .expect("failed to patch instance spec");
}

/// Fast-track an instance from Uninitialized → Running via a completed init
/// job and faked deployment readyReplicas.  Returns the `keep_deployment_ready`
/// handle (caller must abort it when done).
pub async fn fast_track_to_running(ctx: &TestContext, init_job_name: &str) -> JoinHandle<()> {
    let (client, ns) = (&ctx.client, &ctx.ns);
    // Derive instance name from the OdooInstance already in this namespace.
    let instances: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let list = instances.list(&Default::default()).await.unwrap();
    let instance_name = list.items[0].metadata.name.as_deref().unwrap();
    let replicas = list.items[0].spec.replicas;

    let init_api: Api<OdooInitJob> = Api::namespaced(client.clone(), ns);
    let init_job: OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": init_job_name, "namespace": ns },
        "spec": { "odooInstanceRef": { "name": instance_name } }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &init_job)
        .await
        .unwrap();

    assert!(
        wait_for_phase(client, ns, instance_name, OdooInstancePhase::Initializing).await,
        "expected Initializing"
    );
    let k8s_job = wait_for_k8s_job_name::<OdooInitJob>(client, ns, init_job_name).await;
    fake_job_succeeded(client, ns, &k8s_job).await;

    assert!(
        wait_for_phase(client, ns, instance_name, OdooInstancePhase::Starting).await,
        "expected Starting"
    );

    fake_deployment_ready(client, ns, instance_name, replicas).await;
    let handle = keep_deployment_ready(client.clone(), ns.clone(), instance_name.into(), replicas);

    assert!(
        wait_for_phase(client, ns, instance_name, OdooInstancePhase::Running).await,
        "expected Running"
    );

    handle
}

pub async fn check_deployment_scale(
    client: &Client,
    ns: &str,
    name: &str,
    replicas: i32,
) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let deployment = deployments.get(name).await?;
    let actual = deployment.spec.and_then(|s| s.replicas).unwrap_or(0);
    if actual != replicas {
        anyhow::bail!(
            "deployment {} has the wrong replica count: {} instead of {}",
            name,
            actual,
            replicas
        );
    }
    Ok(())
}
