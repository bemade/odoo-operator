use k8s_openapi::api::core::v1::SecretReference;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::shared::{BackupFormat, OdooInstanceRef, Phase, WebhookConfig};

/// BackupDestination specifies where to store the backup artifact.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupDestination {
    pub bucket: String,
    pub object_key: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_credentials_secret_ref: Option<SecretReference>,
}

/// OdooBackupJob runs a one-shot backup of an OdooInstance to S3.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "bemade.org",
    version = "v1alpha1",
    kind = "OdooBackupJob",
    shortname = "backupjob",
    namespaced,
    status = "OdooBackupJobStatus",
    printcolumn = r#"{"name": "Target", "type": "string", "jsonPath": ".spec.odooInstanceRef.name"}"#,
    printcolumn = r#"{"name": "Format", "type": "string", "jsonPath": ".spec.format"}"#,
    printcolumn = r#"{"name": "Phase", "type": "string", "jsonPath": ".status.phase"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OdooBackupJobSpec {
    pub odoo_instance_ref: OdooInstanceRef,
    pub destination: BackupDestination,

    #[serde(default)]
    pub format: BackupFormat,

    #[serde(default = "default_true")]
    pub with_filestore: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

fn default_true() -> bool {
    true
}

/// OdooBackupJobStatus defines the observed state of OdooBackupJob.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooBackupJobStatus {
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
