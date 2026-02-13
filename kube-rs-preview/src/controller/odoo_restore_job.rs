//! OdooRestoreJob controller — downloads a backup artifact and restores it.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Container, Volume, VolumeMount},
};
use kube::{
    api::{Api, Patch, PatchParams, PostParams, ResourceExt},
    runtime::{
        controller::{Action, Controller},
        events::EventType,
        watcher::Config as WatcherConfig,
    },
    Client,
};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_restore_job::{OdooRestoreJob, RestoreSourceType};
use crate::crd::shared::{BackupFormat, Phase};
use crate::error::{Error, Result};
use crate::helpers::sanitise_uid;
use crate::notify;

use super::helpers::{
    cm_env, env, odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER,
};
use super::odoo_instance::Context;
const S3_DOWNLOAD_SCRIPT: &str = include_str!("../../scripts/s3-download.sh");
const ODOO_DOWNLOAD_SCRIPT: &str = include_str!("../../scripts/odoo-download.sh");
const RESTORE_SCRIPT: &str = include_str!("../../scripts/restore.sh");

pub async fn run(ctx: Arc<Context>) {
    let client = ctx.client.clone();
    let restore_jobs: Api<OdooRestoreJob> = Api::all(client.clone());
    let jobs: Api<Job> = Api::all(client.clone());

    Controller::new(restore_jobs, WatcherConfig::default())
        .owns(jobs, WatcherConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("restore job reconcile failed: {e:?}");
            }
        })
        .await;
}

async fn reconcile(restore_job: Arc<OdooRestoreJob>, ctx: Arc<Context>) -> Result<Action> {
    let ns = restore_job.namespace().unwrap_or_default();
    let name = restore_job.name_any();

    if let Some(ref status) = restore_job.status {
        match status.phase.as_ref() {
            Some(Phase::Completed) | Some(Phase::Failed) => return Ok(Action::await_change()),
            _ => {}
        }
    }

    let job_name = restore_job.status.as_ref().and_then(|s| s.job_name.clone());

    if job_name.is_none() {
        return start_job(&ctx, &restore_job, &ns, &name).await;
    }

    sync_job_status(&ctx.client, &restore_job, &ns, &job_name.unwrap(), &ctx).await
}

fn error_policy(_: Arc<OdooRestoreJob>, error: &Error, _: Arc<Context>) -> Action {
    warn!(%error, "restore job error, requeuing in 30s");
    Action::requeue(Duration::from_secs(30))
}

async fn start_job(
    ctx: &Context,
    restore_job: &OdooRestoreJob,
    ns: &str,
    name: &str,
) -> Result<Action> {
    let client = &ctx.client;
    let instance_name = &restore_job.spec.odoo_instance_ref.name;
    let instance_ns = restore_job
        .spec
        .odoo_instance_ref
        .namespace
        .as_deref()
        .unwrap_or(ns);

    let instances: Api<OdooInstance> = Api::namespaced(client.clone(), instance_ns);
    let instance = instances.get(instance_name).await?;

    let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
    let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
    let db = format!("odoo_{}", sanitise_uid(uid));
    let odoo_conf_name = format!("{instance_name}-odoo-conf");

    let _format_str = match restore_job.spec.format {
        BackupFormat::Zip => "zip",
        BackupFormat::Sql => "sql",
        BackupFormat::Dump => "dump",
    };
    let neutralize = if restore_job.spec.neutralize { "True" } else { "False" };

    let output_file = match restore_job.spec.format {
        BackupFormat::Dump => "/mnt/backup/dump.dump",
        BackupFormat::Sql => "/mnt/backup/dump.sql",
        BackupFormat::Zip => "/mnt/backup/backup.zip",
    };

    let shared_mount = VolumeMount {
        name: "backup".into(),
        mount_path: "/mnt/backup".into(),
        ..Default::default()
    };

    let db_env = vec![
        env("DB_NAME", db.clone()),
        env("NEUTRALIZE", neutralize),
        cm_env("HOST", &odoo_conf_name, "db_host"),
        cm_env("PORT", &odoo_conf_name, "db_port"),
        cm_env("USER", &odoo_conf_name, "db_user"),
        cm_env("PASSWORD", &odoo_conf_name, "db_password"),
    ];

    // Build download init container based on source type.
    let mut init_containers = vec![];
    let src = &restore_job.spec.source;

    match src.source_type {
        RestoreSourceType::S3 => {
            if let Some(ref s3) = src.s3 {
                let insecure = if s3.insecure { "true" } else { "false" };
                let mut dl_env = vec![
                    env("S3_BUCKET", s3.bucket.clone()),
                    env("S3_KEY", s3.object_key.clone()),
                    env("S3_ENDPOINT", s3.endpoint.clone()),
                    env("S3_INSECURE", insecure),
                    env("OUTPUT_FILE", output_file),
                    env("MC_CONFIG_DIR", "/tmp/.mc"),
                ];
                // Read S3 credentials from referenced secret if present.
                if let Some(ref secret_ref) = s3.s3_credentials_secret_ref {
                    let secret_ns = secret_ref.namespace.as_deref().unwrap_or(ns);
                    let secret_name = secret_ref.name.as_deref().unwrap_or_default();
                    if let Ok((ak, sk)) = notify::read_s3_credentials(client, secret_name, secret_ns).await {
                        dl_env.push(env("AWS_ACCESS_KEY_ID", ak));
                        dl_env.push(env("AWS_SECRET_ACCESS_KEY", sk));
                    }
                }
                init_containers.push(Container {
                    name: "download".into(),
                    image: Some("quay.io/minio/mc:latest".into()),
                    command: Some(vec!["/bin/sh".into(), "-c".into(), S3_DOWNLOAD_SCRIPT.into()]),
                    env: Some(dl_env),
                    volume_mounts: Some(vec![shared_mount.clone()]),
                    ..Default::default()
                });
            }
        }
        RestoreSourceType::Odoo => {
            if let Some(ref odoo_src) = src.odoo {
                let backup_format = if restore_job.spec.format != BackupFormat::Zip { "dump" } else { "zip" };
                let dl_env = vec![
                    env("ODOO_URL", odoo_src.url.clone()),
                    env("SOURCE_DB", odoo_src.source_database.clone().unwrap_or_default()),
                    env("MASTER_PASSWORD", odoo_src.master_password.clone().unwrap_or_default()),
                    env("BACKUP_FORMAT", backup_format),
                    env("OUTPUT_FILE", output_file),
                ];
                init_containers.push(Container {
                    name: "download".into(),
                    image: Some("curlimages/curl:latest".into()),
                    command: Some(vec!["/bin/sh".into(), "-c".into(), ODOO_DOWNLOAD_SCRIPT.into()]),
                    env: Some(dl_env),
                    volume_mounts: Some(vec![shared_mount.clone()]),
                    ..Default::default()
                });
            }
        }
    }

    let backup_vol = Volume {
        name: "backup".into(),
        empty_dir: Some(Default::default()),
        ..Default::default()
    };

    let mut restore_mounts = odoo_volume_mounts();
    restore_mounts.push(shared_mount);

    let job = OdooJobBuilder::new(&format!("{name}-"), ns, restore_job, &instance)
        .active_deadline(3600)
        .extra_volumes(vec![backup_vol])
        .init_containers(init_containers)
        .containers(vec![Container {
            name: "restore".into(),
            image: Some(image.into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), RESTORE_SCRIPT.into()]),
            env: Some(db_env),
            volume_mounts: Some(restore_mounts),
            ..Default::default()
        }])
        .build();

    let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);
    let created = jobs_api.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();
    info!(%name, %job_name, "created restore job");
    super::odoo_instance::publish_event(
        ctx, restore_job, EventType::Normal, "RestoreStarted", "Reconcile",
        Some(format!("Created restore job {job_name}")),
    ).await;

    let api: Api<OdooRestoreJob> = Api::namespaced(client.clone(), ns);
    let patch = json!({
        "status": {
            "phase": "Running",
            "jobName": job_name,
            "startTime": crate::helpers::utc_now_odoo(),
        }
    });
    api.patch_status(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;

    Ok(Action::requeue(Duration::from_secs(10)))
}

async fn sync_job_status(
    client: &Client,
    restore_job: &OdooRestoreJob,
    ns: &str,
    job_name: &str,
    ctx: &Context,
) -> Result<Action> {
    let name = restore_job.name_any();
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);

    let job = match jobs.get(job_name).await {
        Ok(j) => j,
        Err(kube::Error::Api(ref e)) if e.code == 404 => return Ok(Action::await_change()),
        Err(e) => return Err(e.into()),
    };

    let api: Api<OdooRestoreJob> = Api::namespaced(client.clone(), ns);
    let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
    let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0);

    if succeeded > 0 {
        info!(%name, "restore job succeeded");
        super::odoo_instance::publish_event(
            ctx, restore_job, EventType::Normal, "RestoreCompleted", "Reconcile",
            Some("Restore completed successfully".to_string()),
        ).await;
        let now = crate::helpers::utc_now_odoo();
        let patch = json!({"status": {"phase": "Completed", "completionTime": &now}});
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;

        if let Some(ref wh) = restore_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Completed,
                restore_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                None, Some(&now),
            ).await;
        }
    } else if failed > 0 {
        info!(%name, "restore job failed");
        super::odoo_instance::publish_event(
            ctx, restore_job, EventType::Warning, "RestoreFailed", "Reconcile",
            Some("Restore job failed".to_string()),
        ).await;
        let now = crate::helpers::utc_now_odoo();
        let msg = "restore job failed";
        let patch = json!({"status": {"phase": "Failed", "message": msg, "completionTime": &now}});
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;

        if let Some(ref wh) = restore_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Failed,
                restore_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                Some(msg), Some(&now),
            ).await;
        }
    }

    Ok(Action::await_change())
}

