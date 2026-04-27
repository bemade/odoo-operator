use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Container, PersistentVolumeClaimVolumeSource, Volume, VolumeMount},
};
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;
use crate::crd::shared::Phase;
use crate::error::{Error, Result};

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{
    cm_env, cron_depl_name, env, odoo_volume_mounts, staging_mail_env_vars, OdooJobBuilder,
    FIELD_MANAGER,
};
use crate::controller::state_machine::scale_deployment;

const CLONE_DB_SCRIPT: &str = include_str!("../../../scripts/clone-db.sh");
const CLONE_FILESTORE_SCRIPT: &str = include_str!("../../../scripts/clone-filestore.sh");
const NEUTRALIZE_SCRIPT: &str = include_str!("../../../scripts/neutralize.sh");

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
        let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");

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
            let job = build_db_clone_job(
                &refresh.name_any(),
                &ns,
                image,
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

        // ── Step 2b: filestore clone Job (unless skipped) ─────────────
        if !refresh.spec.skip_filestore
            && refresh
                .status
                .as_ref()
                .and_then(|s| s.filestore_job_name.as_deref())
                .is_none()
        {
            let source_pvc = format!("{source_name}-filestore-pvc");
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
                    }
                }),
            )
            .await?;
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
            || sub_job_succeeded(
                &jobs_api,
                refresh
                    .status
                    .as_ref()
                    .and_then(|s| s.filestore_job_name.as_deref()),
                refresh
                    .status
                    .as_ref()
                    .and_then(|s| s.filestore_job_phase.as_ref()),
            )
            .await;
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
        .containers(vec![Container {
            name: "clone-db".into(),
            image: Some(image.into()),
            command: Some(vec![
                "/bin/bash".into(),
                "-c".into(),
                CLONE_DB_SCRIPT.into(),
            ]),
            env: Some(envs),
            volume_mounts: Some(odoo_volume_mounts()),
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
        .active_deadline(1800)
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
