use k8s_openapi::api::core::v1::{Affinity, ResourceRequirements, Toleration};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Spec sub-types ────────────────────────────────────────────────────────────

/// IngressSpec defines how the OdooInstance should be exposed via an Ingress resource.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct IngressSpec {
    pub hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

/// FilestoreSpec defines persistent storage for the Odoo filestore.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilestoreSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

/// DatabaseSpec identifies which PostgreSQL cluster to use for this instance.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DatabaseSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
}

/// DeploymentStrategyType specifies the update strategy for the Odoo Deployment.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum DeploymentStrategyType {
    #[default]
    Recreate,
    RollingUpdate,
}

/// RollingUpdateSpec configures the RollingUpdate deployment strategy parameters.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdateSpec {
    #[serde(default = "default_25_percent")]
    pub max_unavailable: String,
    #[serde(default = "default_25_percent")]
    pub max_surge: String,
}

fn default_25_percent() -> String {
    "25%".to_string()
}

/// StrategySpec defines the Deployment update strategy.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StrategySpec {
    #[serde(default, rename = "type")]
    pub strategy_type: DeploymentStrategyType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_update: Option<RollingUpdateSpec>,
}

/// OdooWebhookConfig defines an optional webhook callback for status change notifications.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OdooWebhookConfig {
    pub url: String,
}

/// ProbesSpec configures the HTTP health check paths for Kubernetes probes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProbesSpec {
    #[serde(default = "default_health_path")]
    pub startup_path: String,
    #[serde(default = "default_health_path")]
    pub liveness_path: String,
    #[serde(default = "default_health_path")]
    pub readiness_path: String,
}

fn default_health_path() -> String {
    "/web/health".to_string()
}

// ── CRD ───────────────────────────────────────────────────────────────────────

/// OdooInstance is the Schema for the odooinstances API.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "bemade.org",
    version = "v1alpha1",
    kind = "OdooInstance",
    shortname = "odoo",
    namespaced,
    status = "OdooInstanceStatus",
    scale = r#"{"specReplicasPath": ".spec.replicas", "statusReplicasPath": ".status.readyReplicas"}"#,
    printcolumn = r#"{"name": "Image", "type": "string", "jsonPath": ".spec.image"}"#,
    printcolumn = r#"{"name": "Replicas", "type": "string", "jsonPath": ".status.readyReplicas"}"#,
    printcolumn = r#"{"name": "Phase", "type": "string", "jsonPath": ".status.phase"}"#,
    printcolumn = r#"{"name": "URL", "type": "string", "jsonPath": ".status.url"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OdooInstanceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_secret: Option<String>,

    pub admin_password: String,

    #[serde(default = "default_replicas")]
    pub replicas: i32,

    pub ingress: IngressSpec,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filestore: Option<FilestoreSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<std::collections::BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<StrategySpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<OdooWebhookConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probes: Option<ProbesSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Affinity>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,
}

fn default_replicas() -> i32 {
    1
}

// ── Status ────────────────────────────────────────────────────────────────────

/// OdooInstancePhase represents the lifecycle state of an OdooInstance.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum OdooInstancePhase {
    Provisioning,
    Uninitialized,
    Initializing,
    InitFailed,
    Starting,
    Running,
    Degraded,
    Stopped,
    Upgrading,
    Restoring,
    BackingUp,
    Error,
}

impl std::fmt::Display for OdooInstancePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Provisioning => "Provisioning",
            Self::Uninitialized => "Uninitialized",
            Self::Initializing => "Initializing",
            Self::InitFailed => "InitFailed",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Degraded => "Degraded",
            Self::Stopped => "Stopped",
            Self::Upgrading => "Upgrading",
            Self::Restoring => "Restoring",
            Self::BackingUp => "BackingUp",
            Self::Error => "Error",
        };
        write!(f, "{s}")
    }
}

/// OdooInstanceStatus defines the observed state of OdooInstance.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooInstanceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<OdooInstancePhase>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(default)]
    pub ready: bool,

    #[serde(default)]
    pub ready_replicas: i32,

    #[serde(default)]
    pub db_initialized: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_replicas: Option<i32>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
}
