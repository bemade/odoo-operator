//! Shared webhook notification helper used by all job controllers.

use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::{api::Api, Client};
use serde_json::json;
use tracing::warn;

use crate::crd::shared::{Phase, WebhookConfig};
use crate::error::Result;

/// Fire a webhook notification for a job completion/failure.
/// Reads the bearer token from the webhook config (inline or from a Secret).
/// Runs in a spawned task so it doesn't block reconciliation.
#[allow(clippy::too_many_arguments)]
pub async fn notify_job_webhook(
    client: &Client,
    http_client: &reqwest::Client,
    ns: &str,
    webhook: &WebhookConfig,
    phase: &Phase,
    job_name: Option<&str>,
    message: Option<&str>,
    completion_time: Option<&str>,
) {
    let token = resolve_token(client, ns, webhook).await;

    let mut data = json!({
        "phase": phase,
    });
    if let Some(jn) = job_name {
        data["jobName"] = json!(jn);
    }
    if let Some(msg) = message {
        data["message"] = json!(msg);
    }
    if let Some(ct) = completion_time {
        data["completionTime"] = json!(ct);
    }

    let url = webhook.url.clone();
    let http = http_client.clone();

    tokio::spawn(async move {
        let mut req = http.post(&url).json(&data).timeout(Duration::from_secs(10));

        if let Some(tok) = token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(resp) => {
                tracing::info!(%url, status = %resp.status(), "webhook notification sent");
            }
            Err(e) => {
                warn!(%url, %e, "webhook notification failed");
            }
        }
    });
}

/// Resolve the bearer token: prefer inline `token`, fall back to secret ref.
async fn resolve_token(client: &Client, ns: &str, wh: &WebhookConfig) -> Option<String> {
    if let Some(ref token) = wh.token {
        if !token.is_empty() {
            return Some(token.clone());
        }
    }

    if let Some(ref secret_ref) = wh.secret_token_secret_ref {
        let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
        match secrets.get(&secret_ref.name).await {
            Ok(secret) => {
                if let Some(data) = secret.data {
                    if let Some(val) = data.get(&secret_ref.key) {
                        return Some(String::from_utf8_lossy(&val.0).to_string());
                    }
                }
            }
            Err(e) => {
                warn!(secret = %secret_ref.name, %e, "failed to read webhook token secret");
            }
        }
    }

    None
}

/// Read S3 credentials (accessKey, secretKey) from a referenced Secret.
pub async fn read_s3_credentials(
    client: &Client,
    secret_name: &str,
    secret_namespace: &str,
) -> Result<(String, String)> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), secret_namespace);
    let secret = secrets.get(secret_name).await.map_err(|e| {
        crate::error::Error::config(format!(
            "reading S3 credentials secret {secret_namespace}/{secret_name}: {e}"
        ))
    })?;

    let data = secret.data.unwrap_or_default();
    let access_key = data
        .get("accessKey")
        .map(|v| String::from_utf8_lossy(&v.0).to_string())
        .ok_or_else(|| {
            crate::error::Error::config(format!(
                "secret {secret_namespace}/{secret_name} missing 'accessKey' key"
            ))
        })?;
    let secret_key = data
        .get("secretKey")
        .map(|v| String::from_utf8_lossy(&v.0).to_string())
        .ok_or_else(|| {
            crate::error::Error::config(format!(
                "secret {secret_namespace}/{secret_name} missing 'secretKey' key"
            ))
        })?;

    Ok((access_key, secret_key))
}
