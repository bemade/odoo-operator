use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::shared::{BackupFormat, OdooInstanceRef, Phase, S3Config, WebhookConfig};

/// RestoreSourceType identifies the origin of the backup artifact.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum RestoreSourceType {
    #[serde(rename = "s3")]
    S3,
    #[serde(rename = "odoo")]
    Odoo,
}

/// OdooLiveSource downloads a backup from a running Odoo instance via its web backup endpoint.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooLiveSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_password: Option<String>,
}

/// RestoreSource defines where to download the backup artifact from.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSource {
    #[serde(rename = "type")]
    pub source_type: RestoreSourceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odoo: Option<OdooLiveSource>,
}

/// OdooRestoreJob restores an OdooInstance from a backup artifact.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "bemade.org",
    version = "v1alpha1",
    kind = "OdooRestoreJob",
    shortname = "restore",
    namespaced,
    status = "OdooRestoreJobStatus",
    printcolumn = r#"{"name": "Target", "type": "string", "jsonPath": ".spec.odooInstanceRef.name"}"#,
    printcolumn = r#"{"name": "Source", "type": "string", "jsonPath": ".spec.source.type"}"#,
    printcolumn = r#"{"name": "Phase", "type": "string", "jsonPath": ".status.phase"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OdooRestoreJobSpec {
    pub odoo_instance_ref: OdooInstanceRef,
    pub source: RestoreSource,

    #[serde(default)]
    pub format: BackupFormat,

    #[serde(default = "default_true")]
    pub neutralize: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

fn default_true() -> bool {
    true
}

/// OdooRestoreJobStatus defines the observed state of OdooRestoreJob.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooRestoreJobStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
}
