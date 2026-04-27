use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::shared::{OdooInstanceRef, Phase, WebhookConfig};

/// FilestoreMethod selects how the filestore is copied from source to target.
/// The `Auto` variant picks `Snapshot` when a VolumeSnapshotClass covers the
/// filestore's StorageClass, falling back to `Copy` otherwise.  `Copy` (F2)
/// is implemented in Phase 1; `Snapshot` (F1) is Phase 2.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum FilestoreMethod {
    #[default]
    Auto,
    Snapshot,
    Copy,
}

/// StagingSource points at the live OdooInstance whose DB and filestore will
/// be cloned.  `instanceNamespace` defaults to the target's namespace when
/// omitted.  v1 requires source and target to be in the same Kubernetes
/// cluster — cross-cluster is a future addition.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StagingSource {
    pub instance_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_namespace: Option<String>,
}

/// OdooStagingRefreshJob populates or refreshes a staging OdooInstance from
/// a live source instance.  DB is streamed via `pg_dump | pg_restore`;
/// filestore is rsync'd (Phase 1) or CSI-cloned from a VolumeSnapshot
/// (Phase 2, opt-in via `filestoreMethod: snapshot`).  Neutralization and
/// mail-server verification from the existing restore pipeline run after
/// both transfers succeed.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "bemade.org",
    version = "v1alpha1",
    kind = "OdooStagingRefreshJob",
    shortname = "staging-refresh",
    namespaced,
    status = "OdooStagingRefreshJobStatus",
    printcolumn = r#"{"name": "Target", "type": "string", "jsonPath": ".spec.odooInstanceRef.name"}"#,
    printcolumn = r#"{"name": "Source", "type": "string", "jsonPath": ".spec.source.instanceName"}"#,
    printcolumn = r#"{"name": "Phase", "type": "string", "jsonPath": ".status.phase"}"#,
    printcolumn = r#"{"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct OdooStagingRefreshJobSpec {
    pub odoo_instance_ref: OdooInstanceRef,
    pub source: StagingSource,

    #[serde(default)]
    pub filestore_method: FilestoreMethod,

    /// DB-only refresh: skip filestore copy entirely.  Rare, but useful
    /// when the filestore is so large that rsyncing it blocks the whole
    /// refresh cycle and the user only needs the DB for the current test.
    #[serde(default)]
    pub skip_filestore: bool,

    /// Whether to run `odoo neutralize` after the copy.  Defaults true.
    /// Should only be set to false for same-domain copies where the
    /// caller has some other guarantee no emails can escape.  Even when
    /// false, the mail-server verification still runs — a surviving
    /// active mail server with a real smtp_host always fails the job.
    #[serde(default = "default_true")]
    pub neutralize: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookConfig>,
}

fn default_true() -> bool {
    true
}

/// OdooStagingRefreshJobStatus captures the multi-Job orchestration state.
/// Three underlying batch/v1 Jobs back a single refresh: one for DB clone,
/// one for filestore copy (unless skipped), and one for neutralize.  Their
/// names are tracked here for troubleshooting.  `rollbackSnapshot` is set
/// only when Phase 2 (CSI snapshot) is in use.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OdooStagingRefreshJobStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,

    /// Temporary DB name used for blue/green cutover.  Pattern:
    /// `<live_db>_refresh_<rfc3339_short>`.  Set before the DB Job starts
    /// and cleared after the rename cutover succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_db_name: Option<String>,

    /// VolumeSnapshot taken from the source filestore PVC for this
    /// refresh.  Phase 2 only; `None` for rsync-based refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot: Option<String>,

    /// VolumeSnapshot of the target's pre-refresh filestore, retained
    /// for 24h so a bad refresh can be rolled back.  Phase 2 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_snapshot: Option<String>,

    /// batch/v1 Job name for the DB clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_job_name: Option<String>,

    /// Recorded terminal phase (`Completed`/`Failed`) of the DB clone Job.
    /// Persisted on first observation so that future reconciles remain
    /// correct after the underlying batch/v1 Job is garbage-collected by
    /// `ttlSecondsAfterFinished` while sibling sub-Jobs are still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_job_phase: Option<Phase>,

    /// batch/v1 Job name for the filestore copy/clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filestore_job_name: Option<String>,

    /// Recorded terminal phase of the filestore clone Job.  See `dbJobPhase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filestore_job_phase: Option<Phase>,

    /// batch/v1 Job name for the neutralize step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neutralize_job_name: Option<String>,

    /// Recorded terminal phase of the neutralize Job.  See `dbJobPhase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neutralize_job_phase: Option<Phase>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition>,
}
