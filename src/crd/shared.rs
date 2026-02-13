use k8s_openapi::api::core::v1::SecretReference;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// BackupFormat specifies the format of the backup artifact.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum BackupFormat {
    #[default]
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "sql")]
    Sql,
    #[serde(rename = "dump")]
    Dump,
}

/// S3Config holds connection details for an S3-compatible object store.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct S3Config {
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

/// OdooInstanceRef is a reference to an OdooInstance resource.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OdooInstanceRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// WebhookConfig defines an optional webhook callback for job completion notifications.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token_secret_ref: Option<SecretKeySelector>,
}

/// SecretKeySelector selects a key from a Secret.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SecretKeySelector {
    pub name: String,
    pub key: String,
}

/// Phase represents the lifecycle state of a job resource.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
}
