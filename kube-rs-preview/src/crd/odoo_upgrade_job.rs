use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::shared::{OdooInstanceRef, Phase, WebhookConfig};

/// OdooUpgradeJob runs a one-shot module upgrade against an OdooInstance.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "bemade.org",
    version = "v1alpha1",
    kind = "OdooUpgradeJob",
    shortname = "upgradejob",
    namespaced,
    status = "OdooUpgradeJobStatus",
    printcolumn = r#"{"name": "Target", "type": "string", "jsonPath": ".spec.odooInstanceRef.name"}"#,
    printcolumn = r#"{"name": "Phase", "type": "string", "jsonPath": ".status.phase"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OdooUpgradeJobSpec {
    pub odoo_instance_ref: OdooInstanceRef,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules_install: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_time: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

/// OdooUpgradeJobStatus defines the observed state of OdooUpgradeJob.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooUpgradeJobStatus {
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

    #[serde(default)]
    pub original_replicas: i32,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
}
