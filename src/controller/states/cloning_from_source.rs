use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{
        Container, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, TypedObjectReference,
        Volume, VolumeMount,
    },
};
use kube::{
    api::{Api, DeleteParams, Patch, PatchParams, PostParams, ResourceExt},
    core::ErrorResponse,
};
use kube_custom_resources_rs::snapshot_storage_k8s_io::v1::volumesnapshots::{
    VolumeSnapshot, VolumeSnapshotSource, VolumeSnapshotSpec,
};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_staging_refresh_job::FilestoreMethod;
use crate::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;
use crate::crd::shared::Phase;
use crate::error::{Error, Result};
use crate::{controller::child_resources, crd::odoo_instance::OdooInstance};

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{
    cm_env, controller_owner_ref, cron_depl_name, env, odoo_volume_mounts, pg_tools_image,
    staging_mail_env_vars, OdooJobBuilder, FIELD_MANAGER,
};
use crate::controller::state_machine::scale_deployment;
use crate::helpers::sha256_hex;

const CLONE_DB_SCRIPT: &str = include_str!("../../../scripts/clone-db.sh");
const CLONE_FILESTORE_SCRIPT: &str = include_str!("../../../scripts/clone-filestore.sh");
const NEUTRALIZE_SCRIPT: &str = include_str!("../../../scripts/neutralize.sh");

/// Number of hex characters of the image's sha256 we persist in the refresh
/// CR's `neutralizeJobImageHash` to detect spec drift on retry.  16 chars
/// (64 bits) is overkill for accidental-collision resistance but keeps the
/// field readable in `kubectl get -o yaml`.
const IMAGE_HASH_LEN: usize = 16;

/// Compute the short hash recorded in `neutralizeJobImageHash`.  The
/// neutralize retry path treats a mismatch between this and the live image
/// as a signal to recreate the failed Job with the corrected spec.
pub(crate) fn neutralize_image_hash(image: &str) -> String {
    let full = sha256_hex(image);
    full[..IMAGE_HASH_LEN.min(full.len())].to_string()
}

/// Outcome of `ensure_source_snapshot` — whether the dataSource is ready
/// to be referenced by a new PVC.
enum SourceSnapshotState {
    /// Snapshot exists, `readyToUse: true` — safe to recreate the dest PVC
    /// with `dataSourceRef → VolumeSnapshot/<name>`.
    Ready { name: String },
    /// Snapshot exists but `readyToUse` is `false` or `None` — caller should
    /// requeue and check again next reconcile.  The CSI driver will mark
    /// it ready when the underlying snapshot operation completes (instant
    /// for CephFS / RBD, seconds-to-minutes for JuiceFS depending on
    /// metadata engine + file count).
    Pending { name: String },
}

/// Ensure a `VolumeSnapshot` of the source filestore PVC exists and report
/// its readiness.
///
/// The snapshot is the **source of truth** for the staging refresh's
/// filestore — it pins a point-in-time view of production at the moment
/// the refresh started, so the new dest PVC can be cloned from it via
/// `dataSourceRef → VolumeSnapshot`.  This is the universal path: CephFS
/// CSI accepts both PVC-and snapshot dataSources for clones, but JuiceFS
/// CSI accepts **only** VolumeSnapshot — so always going through a
/// snapshot is the only way to support both backends with the same code
/// path.
///
/// Snapshot naming is keyed on the refresh CR's name (one snapshot per
/// refresh, persisted in `status.source_snapshot` for idempotency across
/// reconciles).  The snapshot is owned by the refresh CR so K8s GC cleans
/// it up if the refresh is deleted; on a successful refresh, the
/// `CompleteRefreshJob` transition action deletes it explicitly.
///
/// `volumeSnapshotClassName` is left unset — the cluster's default
/// snapshot class for the source PVC's CSI driver is used.  Each driver
/// in the cluster should have exactly one default class (annotated
/// `snapshot.storage.kubernetes.io/is-default-class: "true"`); without
/// that the API server returns an error which surfaces in the CR
/// `message` field for the user to fix.
async fn ensure_source_snapshot(
    ctx: &Context,
    refresh: &OdooStagingRefreshJob,
    source_pvc_name: &str,
) -> Result<SourceSnapshotState> {
    let ns = refresh.namespace().unwrap_or_default();
    let snapshots: Api<VolumeSnapshot> = Api::namespaced(ctx.client.clone(), &ns);

    // Convention: <refresh-crd>-src.  Deterministic per refresh CR so
    // we can always recompute the name even if status.sourceSnapshot
    // hasn't been persisted yet (or got cleared by an admin force-delete
    // of the snapshot mid-flight).
    let snap_name = format!("{}-src", refresh.name_any());

    // Idempotent create-or-adopt.  Three cases:
    //   1. Snapshot exists with this name and we're already tracking it
    //      → 409 from create, status.sourceSnapshot already set, no-op.
    //   2. Snapshot exists from a prior reconcile that crashed before
    //      patching status → 409 + status.sourceSnapshot empty.  Adopt
    //      and persist.
    //   3. Snapshot doesn't exist (fresh refresh, OR admin force-deleted
    //      the snapshot to recover from a stuck state) → create + persist.
    // This handles the case where status.sourceSnapshot points at a name
    // that no longer exists in the cluster: previous code returned
    // Pending forever because it only created on `recorded == None`.
    let snap = VolumeSnapshot {
        metadata: kube::api::ObjectMeta {
            name: Some(snap_name.clone()),
            namespace: Some(ns.clone()),
            owner_references: Some(vec![controller_owner_ref(refresh)]),
            ..Default::default()
        },
        spec: VolumeSnapshotSpec {
            source: VolumeSnapshotSource {
                persistent_volume_claim_name: Some(source_pvc_name.to_string()),
                volume_snapshot_content_name: None,
            },
            volume_snapshot_class_name: None,
        },
        status: None,
    };
    let created_now = match snapshots.create(&PostParams::default(), &snap).await {
        Ok(_) => {
            info!(
                crd_name = %refresh.name_any(),
                snapshot = %snap_name,
                source_pvc = %source_pvc_name,
                "created source VolumeSnapshot for staging refresh"
            );
            true
        }
        Err(kube::Error::Api(ErrorResponse { code: 409, .. })) => false,
        Err(e) => return Err(Error::Kube(e)),
    };

    let recorded = refresh
        .status
        .as_ref()
        .and_then(|s| s.source_snapshot.as_deref());
    if created_now || recorded != Some(snap_name.as_str()) {
        patch_refresh_status(
            &ctx.client,
            &ns,
            &refresh.name_any(),
            &json!({ "status": { "sourceSnapshot": &snap_name } }),
        )
        .await?;
    }

    // Check readiness.  `readyToUse: true` means the CSI driver has
    // finished snapshot creation; the snapshot can be referenced by a new
    // PVC.  Anything else (None, false, transient lookup miss) → not
    // ready, requeue.
    match snapshots.get_opt(&snap_name).await? {
        Some(s) => {
            let ready = s
                .status
                .as_ref()
                .and_then(|st| st.ready_to_use)
                .unwrap_or(false);
            if ready {
                Ok(SourceSnapshotState::Ready { name: snap_name })
            } else {
                Ok(SourceSnapshotState::Pending { name: snap_name })
            }
        }
        None => {
            // Race: we just created it but the read-after-write hasn't
            // settled.  Pending; next reconcile will see it.
            Ok(SourceSnapshotState::Pending { name: snap_name })
        }
    }
}

/// Delete the source `VolumeSnapshot` recorded on a refresh CR's status.
/// Best-effort — 404 is the expected case once K8s GC has run.
pub async fn delete_source_snapshot(ctx: &Context, ns: &str, snapshot_name: &str) -> Result<()> {
    let snapshots: Api<VolumeSnapshot> = Api::namespaced(ctx.client.clone(), ns);
    match snapshots
        .delete(snapshot_name, &DeleteParams::background())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ErrorResponse { code: 404, .. })) => Ok(()),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// CloningFromSource: orchestrates the three-Job refresh pipeline.
///
/// Step 1: scale staging web+cron to 0 (same as Initializing / Restoring).
/// Step 2: if not yet created, spawn the DB clone Job and the filestore
///         clone Job in parallel.
/// Step 3: when both succeed, spawn the neutralize Job.  The aggregate
///         `refresh_job` in the snapshot goes `Succeeded` when the
///         neutralize Job succeeds, and the state machine transitions
///         CloningFromSource → Starting.
///
/// V1 constraint: source and target instances must be in the same
/// Kubernetes namespace.  Cross-namespace filestore copy requires a
/// network-based rsync path that's deferred to a follow-up.
pub struct CloningFromSource;

#[async_trait]
impl State for CloningFromSource {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        scale_deployment(&ctx.client, &inst_name, &ns, 0).await?;
        scale_deployment(&ctx.client, cron_depl_name(instance).as_str(), &ns, 0).await?;

        let Some(refresh) = snap.active_refresh_job.as_ref() else {
            return Ok(());
        };

        let source_ns = refresh
            .spec
            .source
            .instance_namespace
            .as_deref()
            .unwrap_or(ns.as_str());
        if source_ns != ns.as_str() {
            // V1 limitation — surfaced in the CR status for the user.
            patch_refresh_message(
                &ctx.client,
                &ns,
                &refresh.name_any(),
                "cross-namespace staging refresh is not supported in v1 — \
                 source and target must share a namespace",
            )
            .await?;
            return Err(Error::Config(
                "staging refresh source instance must be in the same namespace as target".into(),
            ));
        }

        let source_name = &refresh.spec.source.instance_name;
        let source_instances: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), source_ns);
        let source_instance = match source_instances.get(source_name).await {
            Ok(inst) => inst,
            Err(e) => {
                patch_refresh_message(
                    &ctx.client,
                    &ns,
                    &refresh.name_any(),
                    &format!("source OdooInstance {source_name} not found: {e}"),
                )
                .await?;
                return Err(Error::Kube(e));
            }
        };

        let source_db = crate::helpers::db_name(&source_instance);
        let target_db = crate::helpers::db_name(instance);
        let source_conf = format!("{source_name}-odoo-conf");
        let target_conf = format!("{inst_name}-odoo-conf");
        let image = instance
            .spec
            .image
            .as_deref()
            .unwrap_or(&ctx.defaults.odoo_image);

        // ── Step 2a: DB clone Job ─────────────────────────────────────
        if refresh
            .status
            .as_ref()
            .and_then(|s| s.db_job_name.as_deref())
            .is_none()
        {
            // Postgres identifiers are limited to 63 bytes.  The Odoo DB
            // name is `odoo_<36-char-uuid>` = 41 chars, leaving 22 bytes
            // for the refresh suffix.  An 8-hex-char sha256 prefix of
            // the refresh CR's UID gives us uniqueness without exceeding
            // the limit and is stable across reconciles on the same CR
            // (important so the trap-drop at the start of clone-db.sh
            // can clean up a leftover temp DB from a failed prior run).
            let uid = refresh.metadata.uid.as_deref().unwrap_or("norun");
            let short = {
                use sha2::{Digest, Sha256};
                let h = Sha256::digest(uid.as_bytes());
                let hex = format!("{h:x}");
                hex[..8].to_string()
            };
            let temp_db = format!("{target_db}_refresh_{short}");

            // Detect PG server majors on both source and target clusters and
            // run clone-db.sh with a pg client image whose tools are ≥ both
            // server majors. Using the Odoo image's pg_dump (16.x) against a
            // PG18 server fails outright. Failure to probe aborts the refresh
            // — we can't clone a cluster we can't reach.
            let (_, src_pg) =
                super::super::odoo_instance::load_postgres_cluster(ctx, &source_instance).await?;
            let (_, tgt_pg) =
                super::super::odoo_instance::load_postgres_cluster(ctx, instance).await?;
            let src_major = ctx.postgres.detect_server_major_version(&src_pg).await?;
            let tgt_major = ctx.postgres.detect_server_major_version(&tgt_pg).await?;
            let db_image = pg_tools_image(src_major.max(tgt_major));
            info!(
                crd_name = %refresh.name_any(),
                src_major, tgt_major, %db_image,
                "selected pg client image for staging refresh DB clone"
            );

            let job = build_db_clone_job(
                &refresh.name_any(),
                &ns,
                &db_image,
                instance,
                refresh,
                &source_conf,
                &target_conf,
                &source_db,
                &target_db,
                &temp_db,
            );
            let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
            let created = jobs_api.create(&PostParams::default(), &job).await?;
            let k8s_job_name = created.name_any();
            info!(
                crd_name = %refresh.name_any(),
                %k8s_job_name,
                "created DB clone job"
            );
            patch_refresh_status(
                &ctx.client,
                &ns,
                &refresh.name_any(),
                &json!({
                    "status": {
                        "phase": "Running",
                        "dbJobName": &k8s_job_name,
                        "tempDbName": &temp_db,
                        "startTime": crate::helpers::utc_now_odoo(),
                    }
                }),
            )
            .await?;
        }

        // ── Step 2b: filestore step (unless skipped) ──────────────────
        // Mode-agnostic gate: the start guard and completion check both
        // ride on `filestore_phase`, set to Running once kickoff succeeds
        // and Completed/Failed when the underlying work terminates.
        if !refresh.spec.skip_filestore
            && refresh
                .status
                .as_ref()
                .and_then(|s| s.filestore_phase.as_ref())
                .is_none()
        {
            let source_pvc = format!("{source_name}-filestore-pvc");
            let use_copy = refresh.spec.filestore_method == FilestoreMethod::Copy
                || (refresh.spec.filestore_method == FilestoreMethod::Auto
                    && !can_refresh_with_snapshot(&source_instance, instance));
            if use_copy {
                let job = build_filestore_clone_job(
                    &refresh.name_any(),
                    &ns,
                    image,
                    instance,
                    refresh,
                    &source_pvc,
                    &source_db,
                    &target_db,
                );
                let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
                let created = jobs_api.create(&PostParams::default(), &job).await?;
                let k8s_job_name = created.name_any();
                info!(
                    crd_name = %refresh.name_any(),
                    %k8s_job_name,
                    "created filestore clone job"
                );
                patch_refresh_status(
                    &ctx.client,
                    &ns,
                    &refresh.name_any(),
                    &json!({
                        "status": {
                            "filestoreJobName": &k8s_job_name,
                            "filestorePhase": Phase::Running,
                        }
                    }),
                )
                .await?;
            } else {
                // Snapshot path: take a VolumeSnapshot of the source PVC,
                // wait for it to be ready, then recreate the dest PVC with
                // dataSourceRef → VolumeSnapshot.  Going via a snapshot is
                // the universal CSI path: CephFS accepts both PVC and
                // snapshot dataSources, but JuiceFS only accepts snapshot
                // (`only VolumeSnapshot data source is supported, got
                // PersistentVolumeClaim`), so PVC→PVC clone breaks JuiceFS
                // refreshes outright.  Single code path → both backends.
                //
                // Done in three reconcile-amenable steps so we don't
                // block the controller:
                //   tick A: ensure source snapshot exists; if not Ready,
                //           bail (next tick checks again).  Snapshot
                //           creation is instant on CephFS, may take
                //           seconds-to-minutes on JuiceFS depending on
                //           file count and metadata engine speed.
                //   tick B: snapshot Ready → delete the existing dest PVC.
                //           If still terminating, bail.
                //   tick C: dest PVC is gone → create it with
                //           dataSourceRef → VolumeSnapshot, patch
                //           filestorePhase Running.  Settle block then
                //           waits for Bound.
                // Patching Running before all three steps complete would
                // let the settle block see a stale `Bound` and mark the
                // step Completed before the new clone is actually live.
                let source_pvc = format!("{source_name}-filestore-pvc");
                let snap_state = ensure_source_snapshot(ctx, refresh, &source_pvc).await?;
                let snap_name = match snap_state {
                    SourceSnapshotState::Ready { name } => name,
                    SourceSnapshotState::Pending { name } => {
                        info!(
                            crd_name = %refresh.name_any(),
                            snapshot = %name,
                            "source VolumeSnapshot not yet readyToUse; waiting"
                        );
                        return Ok(());
                    }
                };

                let target_pvc = format!("{inst_name}-filestore-pvc");
                let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
                // Background delete (the default): PVCs don't have
                // OwnerReference dependents, so foreground propagation
                // would only add a `foregroundDeletion` finalizer that
                // kube-controller-manager has to strip — pointless on a
                // real cluster and a deadlock in envtest, which has no
                // GC controller.
                if let Err(e) = pvcs
                    .delete(target_pvc.as_str(), &DeleteParams::default())
                    .await
                {
                    if !matches!(&e, kube::Error::Api(ErrorResponse { code: 404, .. })) {
                        return Err(e.into());
                    }
                }
                match pvcs.get_opt(target_pvc.as_str()).await? {
                    Some(existing) if existing.metadata.deletion_timestamp.is_some() => {
                        info!(
                            crd_name = %refresh.name_any(),
                            pvc = %target_pvc,
                            "old staging filestore PVC still terminating; waiting before recreate"
                        );
                        return Ok(());
                    }
                    _ => {}
                }
                let oref = controller_owner_ref(instance);
                let snap_data_source = TypedObjectReference {
                    api_group: Some("snapshot.storage.k8s.io".to_string()),
                    kind: "VolumeSnapshot".to_string(),
                    name: snap_name.clone(),
                    namespace: None,
                };
                child_resources::ensure_filestore_pvc(
                    &ctx.client,
                    &ns,
                    &inst_name,
                    instance,
                    ctx,
                    &oref,
                    Some(snap_data_source),
                )
                .await?;
                info!(
                    crd_name = %refresh.name_any(),
                    pvc = %target_pvc,
                    snapshot = %snap_name,
                    "kicked off filestore snapshot/clone PVC recreate"
                );
                patch_refresh_status(
                    &ctx.client,
                    &ns,
                    &refresh.name_any(),
                    &json!({
                        "status": {
                            "filestorePhase": Phase::Running,
                        }
                    }),
                )
                .await?;
            }
        }

        // ── Step 2c: settle filestore_phase from underlying signal ────
        if matches!(
            refresh
                .status
                .as_ref()
                .and_then(|s| s.filestore_phase.as_ref()),
            Some(Phase::Running)
        ) {
            let terminal = if refresh
                .status
                .as_ref()
                .and_then(|s| s.filestore_job_name.as_deref())
                .is_some()
            {
                // Copy path: mirror Job's recorded terminal phase.
                match refresh
                    .status
                    .as_ref()
                    .and_then(|s| s.filestore_job_phase.as_ref())
                {
                    Some(Phase::Completed) => Some(Phase::Completed),
                    Some(Phase::Failed) => Some(Phase::Failed),
                    _ => None,
                }
            } else {
                // Snapshot path: new PVC bound ⇒ Completed.  Defensively
                // require no `deletionTimestamp` so a still-terminating
                // old PVC can't masquerade as the recreated one.
                let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
                let target_pvc = format!("{inst_name}-filestore-pvc");
                match pvcs.get(target_pvc.as_str()).await {
                    Ok(pvc)
                        if pvc.metadata.deletion_timestamp.is_none()
                            && pvc.status.as_ref().and_then(|s| s.phase.as_deref())
                                == Some("Bound") =>
                    {
                        Some(Phase::Completed)
                    }
                    _ => None,
                }
            };
            if let Some(p) = terminal {
                patch_refresh_status(
                    &ctx.client,
                    &ns,
                    &refresh.name_any(),
                    &json!({"status": {"filestorePhase": p}}),
                )
                .await?;
            }
        }

        // ── Step 3: neutralize (after DB + filestore succeed) ─────────
        let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
        let db_done = sub_job_succeeded(
            &jobs_api,
            refresh
                .status
                .as_ref()
                .and_then(|s| s.db_job_name.as_deref()),
            refresh
                .status
                .as_ref()
                .and_then(|s| s.db_job_phase.as_ref()),
        )
        .await;
        let fs_done = refresh.spec.skip_filestore
            || matches!(
                refresh
                    .status
                    .as_ref()
                    .and_then(|s| s.filestore_phase.as_ref()),
                Some(Phase::Completed)
            );
        if db_done
            && fs_done
            && refresh
                .status
                .as_ref()
                .and_then(|s| s.neutralize_job_name.as_deref())
                .is_none()
            && refresh.spec.neutralize
        {
            let job = build_neutralize_job(
                &refresh.name_any(),
                &ns,
                image,
                instance,
                refresh,
                &target_conf,
                &target_db,
                &ctx.defaults,
            );
            let created = jobs_api.create(&PostParams::default(), &job).await?;
            let k8s_job_name = created.name_any();
            info!(
                crd_name = %refresh.name_any(),
                %k8s_job_name,
                "created neutralize job"
            );
            patch_refresh_status(
                &ctx.client,
                &ns,
                &refresh.name_any(),
                &json!({
                    "status": {
                        "neutralizeJobName": &k8s_job_name,
                        "neutralizeJobImageHash": neutralize_image_hash(image),
                    }
                }),
            )
            .await?;
        } else if db_done && fs_done && !refresh.spec.neutralize {
            // Neutralize disabled — still need a completion signal for the
            // aggregate rollup.  Mark the neutralize sub-job as an empty
            // succeeded sentinel by setting its name to a distinguishable
            // value that resolve_k8s_job_status returns `Failed` for (404).
            // Simpler: short-circuit by patching the CR phase ourselves.
            // (Rarely used in practice since neutralize defaults true.)
            patch_refresh_status(
                &ctx.client,
                &ns,
                &refresh.name_any(),
                &json!({
                    "status": {
                        "neutralizeJobName": format!("{}-neutralize-skipped", refresh.name_any()),
                    }
                }),
            )
            .await?;
        }

        Ok(())
    }
}

/// Has a refresh sub-Job reached a successful terminal state?
///
/// Prefers the recorded `Phase::Completed` on the parent CR (set by the
/// snapshot builder once a terminal status is observed) so this gate
/// keeps returning `true` even after the underlying batch/v1 Job is
/// garbage-collected by `ttlSecondsAfterFinished` while siblings are
/// still in flight.  Falls back to a live K8s lookup when no record
/// exists yet.
async fn sub_job_succeeded(
    jobs_api: &Api<Job>,
    name: Option<&str>,
    recorded_phase: Option<&Phase>,
) -> bool {
    if matches!(recorded_phase, Some(Phase::Completed)) {
        return true;
    }
    let Some(name) = name else {
        return false;
    };
    match jobs_api.get(name).await {
        Ok(job) => job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) > 0,
        Err(_) => false,
    }
}

async fn patch_refresh_status(
    client: &kube::Client,
    ns: &str,
    name: &str,
    patch: &serde_json::Value,
) -> Result<()> {
    let api: Api<OdooStagingRefreshJob> = Api::namespaced(client.clone(), ns);
    api.patch_status(
        name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(patch),
    )
    .await?;
    Ok(())
}

async fn patch_refresh_message(
    client: &kube::Client,
    ns: &str,
    name: &str,
    msg: &str,
) -> Result<()> {
    patch_refresh_status(client, ns, name, &json!({ "status": { "message": msg } })).await
}

#[allow(clippy::too_many_arguments)]
fn build_db_clone_job(
    crd_name: &str,
    ns: &str,
    image: &str,
    instance: &OdooInstance,
    refresh: &OdooStagingRefreshJob,
    source_conf: &str,
    target_conf: &str,
    source_db: &str,
    target_db: &str,
    temp_db: &str,
) -> Job {
    let envs = vec![
        cm_env("SRC_HOST", source_conf, "db_host"),
        cm_env("SRC_PORT", source_conf, "db_port"),
        cm_env("SRC_USER", source_conf, "db_user"),
        cm_env("SRC_PASSWORD", source_conf, "db_password"),
        env("SRC_DB", source_db),
        cm_env("TGT_HOST", target_conf, "db_host"),
        cm_env("TGT_PORT", target_conf, "db_port"),
        cm_env("TGT_USER", target_conf, "db_user"),
        cm_env("TGT_PASSWORD", target_conf, "db_password"),
        env("TGT_DB", target_db),
        env("TEMP_DB", temp_db),
    ];
    OdooJobBuilder::new(&format!("{crd_name}-db-"), ns, refresh, instance)
        .active_deadline(3600)
        .without_standard_volumes()
        .containers(vec![Container {
            name: "clone-db".into(),
            image: Some(image.into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), CLONE_DB_SCRIPT.into()]),
            env: Some(envs),
            ..Default::default()
        }])
        .build()
}

#[allow(clippy::too_many_arguments)]
fn build_filestore_clone_job(
    crd_name: &str,
    ns: &str,
    image: &str,
    instance: &OdooInstance,
    refresh: &OdooStagingRefreshJob,
    source_pvc: &str,
    source_db: &str,
    target_db: &str,
) -> Job {
    // Target filestore is already in the standard odoo_volumes() set; we
    // just need to add a second mount for the source PVC (read-only).
    let src_vol = Volume {
        name: "source-filestore".into(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: source_pvc.into(),
            read_only: Some(true),
        }),
        ..Default::default()
    };

    let src_mount = VolumeMount {
        name: "source-filestore".into(),
        mount_path: "/src".into(),
        read_only: Some(true),
        ..Default::default()
    };
    // Standard target mount is /var/lib/odoo (via odoo_volume_mounts())
    let envs = vec![
        env("SRC_FILESTORE", "/src"),
        env("TGT_FILESTORE", "/var/lib/odoo"),
        env("SRC_DB", source_db),
        env("TGT_DB", target_db),
    ];
    let mut mounts = odoo_volume_mounts();
    mounts.push(src_mount);
    OdooJobBuilder::new(&format!("{crd_name}-fs-"), ns, refresh, instance)
        .active_deadline(7200)
        .extra_volumes(vec![src_vol])
        .containers(vec![Container {
            name: "clone-filestore".into(),
            image: Some(image.into()),
            command: Some(vec![
                "/bin/bash".into(),
                "-c".into(),
                CLONE_FILESTORE_SCRIPT.into(),
            ]),
            env: Some(envs),
            volume_mounts: Some(mounts),
            ..Default::default()
        }])
        .build()
}

#[allow(clippy::too_many_arguments)]
fn build_neutralize_job(
    crd_name: &str,
    ns: &str,
    image: &str,
    instance: &OdooInstance,
    refresh: &OdooStagingRefreshJob,
    target_conf: &str,
    target_db: &str,
    defaults: &crate::helpers::OperatorDefaults,
) -> Job {
    let mut envs = vec![
        env("DB_NAME", target_db),
        cm_env("HOST", target_conf, "db_host"),
        cm_env("PORT", target_conf, "db_port"),
        cm_env("USER", target_conf, "db_user"),
        cm_env("PASSWORD", target_conf, "db_password"),
    ];
    envs.extend(staging_mail_env_vars(instance, defaults));
    OdooJobBuilder::new(&format!("{crd_name}-neut-"), ns, refresh, instance)
        // Neutralize is a small set of SQL UPDATEs (disable cron jobs,
        // swap ir_mail_server, blank API keys / passwords).  Realistic
        // wall time is seconds; 5 min is generous headroom for a slow
        // DB.  The previous 30 min was masking image-pull failures as
        // long-running jobs instead of failing fast — fixed in concert
        // with the neutralize-image-hash retry path so an image typo
        // surfaces in 5 min instead of 30.
        .active_deadline(300)
        // Allow K8s' built-in exponential backoff to absorb transient pod
        // failures (script crashes, OOM, network blips during DB connect).
        // ImagePullBackOff is *not* covered by backoffLimit (the Pod never
        // exits) — that's the spec-drift retry path's responsibility, gated
        // on `neutralizeJobImageHash`.
        .backoff_limit(5)
        .containers(vec![Container {
            name: "neutralize".into(),
            image: Some(image.into()),
            command: Some(vec![
                "/bin/bash".into(),
                "-c".into(),
                NEUTRALIZE_SCRIPT.into(),
            ]),
            env: Some(envs),
            volume_mounts: Some(odoo_volume_mounts()),
            ..Default::default()
        }])
        .build()
}

/// Spec-drift retry for the neutralize step.
///
/// When an OdooInstance is stuck in `InitFailed` because the staging refresh's
/// neutralize Job hit a terminal failure (typically `ImagePullBackOff` →
/// `DeadlineExceeded`), and the user has since corrected `spec.image`, this
/// detects the drift via the `neutralizeJobImageHash` recorded on the
/// failed refresh CR and resets it so the operator's normal recovery path
/// (`InitFailed → CloningFromSource`) can re-create the neutralize Job with
/// the corrected spec on the next reconcile tick.
///
/// Scope is intentionally narrow: only the neutralize sub-Job, only on a
/// hash mismatch.  DB and filestore failures rely on `backoffLimit` for
/// transient issues and require explicit user action otherwise.  This keeps
/// the retry loop bounded — repeated reconciles with the same broken image
/// don't churn (hash matches → nothing to do), only a real spec edit does.
///
/// Returns `true` if a reset was performed (caller may want to log).
pub(crate) async fn maybe_retry_failed_neutralize(
    instance: &OdooInstance,
    ctx: &Context,
) -> Result<bool> {
    let ns = instance.namespace().unwrap_or_default();
    let instance_name = instance.name_any();
    let image = instance
        .spec
        .image
        .as_deref()
        .unwrap_or(&ctx.defaults.odoo_image);
    let current_hash = neutralize_image_hash(image);

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(ctx.client.clone(), &ns);
    let list = refreshes.list(&kube::api::ListParams::default()).await?;

    // We only retry refresh jobs whose *neutralize* sub-step terminally
    // failed — that's the only failure mode where (a) the DB and filestore
    // work that preceded it is still good (so a retry doesn't redo the
    // expensive clone) and (b) a spec.image edit is the realistic
    // user-facing fix.  The aggregate `phase == Failed` is implied by the
    // rollup whenever any sub-job is Failed, so we don't need to gate on
    // it separately.
    //
    // Picking the most-recently-created candidate covers the case where
    // the user has historical Failed refresh CRs lying around from
    // previous attempts.
    let candidate = list
        .items
        .into_iter()
        .filter(|j| j.spec.odoo_instance_ref.name == instance_name)
        .filter(|j| {
            matches!(
                j.status
                    .as_ref()
                    .and_then(|s| s.neutralize_job_phase.as_ref()),
                Some(Phase::Failed)
            )
        })
        .max_by(|a, b| a.creation_timestamp().cmp(&b.creation_timestamp()));

    let Some(refresh) = candidate else {
        return Ok(false);
    };

    let recorded_hash = refresh
        .status
        .as_ref()
        .and_then(|s| s.neutralize_job_image_hash.as_deref());

    if recorded_hash == Some(current_hash.as_str()) {
        // Same image as the failed attempt — nothing changed, don't churn.
        return Ok(false);
    }

    let crd_name = refresh.name_any();
    let job_name = refresh
        .status
        .as_ref()
        .and_then(|s| s.neutralize_job_name.as_deref());

    // Best-effort cleanup of the underlying batch/v1 Job.  It's normally
    // already gone (TTL or DeadlineExceeded), so 404 is the expected case.
    if let Some(name) = job_name {
        let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
        match jobs_api.delete(name, &DeleteParams::background()).await {
            Ok(_) => {}
            Err(kube::Error::Api(ErrorResponse { code: 404, .. })) => {}
            Err(e) => return Err(Error::Kube(e)),
        }
    }

    // Clear the neutralize fields and reset the aggregate phase from
    // Failed back to Running.  The snapshot rollup will then re-classify
    // refresh_job as Active, the InitFailed → CloningFromSource transition
    // fires, and the regular ensure() path creates a fresh neutralize Job
    // from current spec.  The DB and filestore steps stay completed —
    // we don't redo them.
    patch_refresh_status(
        &ctx.client,
        &ns,
        &crd_name,
        &json!({
            "status": {
                "phase": Phase::Running,
                "neutralizeJobName": Option::<String>::None,
                "neutralizeJobPhase": Option::<Phase>::None,
                "neutralizeJobImageHash": Option::<String>::None,
                "message": "neutralize image changed; retrying with corrected spec",
            }
        }),
    )
    .await?;

    info!(
        crd_name = %crd_name,
        new_image = %image,
        new_hash = %current_hash,
        old_hash = ?recorded_hash,
        "neutralize spec-drift detected; cleared status to retry"
    );
    Ok(true)
}

fn can_refresh_with_snapshot(source_instance: &OdooInstance, dest_instance: &OdooInstance) -> bool {
    sc(source_instance).is_some() && sc(source_instance) == sc(dest_instance)
}

fn sc(instance: &OdooInstance) -> Option<&str> {
    instance
        .spec
        .filestore
        .as_ref()
        .and_then(|s| s.storage_class.as_deref())
}
