//! OdooBackupJob controller — creates a two-stage Job (backup init container + S3 upload).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Container, PodAffinityTerm, Volume, VolumeMount},
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
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

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::crd::shared::Phase;
use crate::error::{Error, Result};
use crate::helpers::sanitise_uid;
use crate::notify;

use super::helpers::{
    cm_env, env, odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER,
};
use super::odoo_instance::Context;
const BACKUP_SCRIPT: &str = include_str!("../../scripts/backup.sh");
const UPLOAD_SCRIPT: &str = include_str!("../../scripts/s3-upload.sh");

pub async fn run(ctx: Arc<Context>) {
    let client = ctx.client.clone();
    let backup_jobs: Api<OdooBackupJob> = Api::all(client.clone());
    let jobs: Api<Job> = Api::all(client.clone());

    Controller::new(backup_jobs, WatcherConfig::default())
        .owns(jobs, WatcherConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("backup job reconcile failed: {e:?}");
            }
        })
        .await;
}

async fn reconcile(backup_job: Arc<OdooBackupJob>, ctx: Arc<Context>) -> Result<Action> {
    let ns = backup_job.namespace().unwrap_or_default();
    let name = backup_job.name_any();

    if let Some(ref status) = backup_job.status {
        match status.phase.as_ref() {
            Some(Phase::Completed) | Some(Phase::Failed) => return Ok(Action::await_change()),
            _ => {}
        }
    }

    let job_name = backup_job.status.as_ref().and_then(|s| s.job_name.clone());

    if job_name.is_none() {
        return start_job(&ctx, &backup_job, &ns, &name).await;
    }

    sync_job_status(&ctx.client, &backup_job, &ns, &job_name.unwrap(), &ctx).await
}

fn error_policy(_: Arc<OdooBackupJob>, error: &Error, _: Arc<Context>) -> Action {
    warn!(%error, "backup job error, requeuing in 30s");
    Action::requeue(Duration::from_secs(30))
}

async fn start_job(
    ctx: &Context,
    backup_job: &OdooBackupJob,
    ns: &str,
    name: &str,
) -> Result<Action> {
    let client = &ctx.client;
    let instance_name = &backup_job.spec.odoo_instance_ref.name;
    let instance_ns = backup_job
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

    let dest = &backup_job.spec.destination;
    let format = match backup_job.spec.format {
        crate::crd::shared::BackupFormat::Zip => "zip",
        crate::crd::shared::BackupFormat::Sql => "sql",
        crate::crd::shared::BackupFormat::Dump => "dump",
    };
    let local_filename = if !dest.object_key.is_empty() {
        std::path::Path::new(&dest.object_key)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{instance_name}-backup"))
    } else {
        format!("{instance_name}-backup")
    };

    let with_filestore = if backup_job.spec.with_filestore { "true" } else { "false" };

    let backup_env = vec![
        env("INSTANCE_NAME", instance_name.clone()),
        env("DB_NAME", db.clone()),
        env("BACKUP_FORMAT", format),
        env("BACKUP_WITH_FILESTORE", with_filestore),
        env("LOCAL_FILENAME", local_filename),
        cm_env("HOST", &odoo_conf_name, "db_host"),
        cm_env("PORT", &odoo_conf_name, "db_port"),
        cm_env("USER", &odoo_conf_name, "db_user"),
        cm_env("PASSWORD", &odoo_conf_name, "db_password"),
    ];

    let insecure = if dest.insecure { "true" } else { "false" };
    let mut upload_env = vec![
        env("S3_BUCKET", dest.bucket.clone()),
        env("S3_KEY", dest.object_key.clone()),
        env("S3_ENDPOINT", dest.endpoint.clone()),
        env("S3_INSECURE", insecure),
        env("MC_CONFIG_DIR", "/tmp/.mc"),
    ];

    // Read S3 credentials from referenced secret if present.
    if let Some(ref secret_ref) = dest.s3_credentials_secret_ref {
        let secret_ns = secret_ref.namespace.as_deref().unwrap_or(ns);
        let secret_name = secret_ref.name.as_deref().unwrap_or_default();
        if let Ok((ak, sk)) = notify::read_s3_credentials(client, secret_name, secret_ns).await {
            upload_env.push(env("AWS_ACCESS_KEY_ID", ak));
            upload_env.push(env("AWS_SECRET_ACCESS_KEY", sk));
        }
    }

    let shared_mount = VolumeMount {
        name: "backup".into(),
        mount_path: "/mnt/backup".into(),
        ..Default::default()
    };

    let backup_vol = Volume {
        name: "backup".into(),
        empty_dir: Some(Default::default()),
        ..Default::default()
    };

    let mut init_mounts = odoo_volume_mounts();
    init_mounts.push(shared_mount.clone());

    let pod_affinity = k8s_openapi::api::core::v1::Affinity {
        pod_affinity: Some(k8s_openapi::api::core::v1::PodAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![
                PodAffinityTerm {
                    label_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([("app".into(), instance_name.clone())])),
                        ..Default::default()
                    }),
                    topology_key: "kubernetes.io/hostname".into(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let job = OdooJobBuilder::new(&format!("{name}-"), ns, backup_job, &instance)
        .active_deadline(1800)
        .extra_volumes(vec![backup_vol])
        .affinity(pod_affinity)
        .init_containers(vec![Container {
            name: "backup".into(),
            image: Some(image.into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), BACKUP_SCRIPT.into()]),
            env: Some(backup_env),
            volume_mounts: Some(init_mounts),
            ..Default::default()
        }])
        .containers(vec![Container {
            name: "uploader".into(),
            image: Some("quay.io/minio/mc:latest".into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), UPLOAD_SCRIPT.into()]),
            env: Some(upload_env),
            volume_mounts: Some(vec![shared_mount]),
            ..Default::default()
        }])
        .build();

    let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);
    let created = jobs_api.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();
    info!(%name, %job_name, "created backup job");
    super::odoo_instance::publish_event(
        ctx, backup_job, EventType::Normal, "BackupStarted", "Reconcile",
        Some(format!("Created backup job {job_name}")),
    ).await;

    let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), ns);
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
    backup_job: &OdooBackupJob,
    ns: &str,
    job_name: &str,
    ctx: &Context,
) -> Result<Action> {
    let name = backup_job.name_any();
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);

    let job = match jobs.get(job_name).await {
        Ok(j) => j,
        Err(kube::Error::Api(ref e)) if e.code == 404 => return Ok(Action::await_change()),
        Err(e) => return Err(e.into()),
    };

    let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), ns);
    let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
    let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0);

    if succeeded > 0 {
        info!(%name, "backup job succeeded");
        let now = crate::helpers::utc_now_odoo();
        let patch = json!({"status": {"phase": "Completed", "completionTime": &now}});
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;
        super::odoo_instance::publish_event(
            ctx, backup_job, EventType::Normal, "BackupCompleted", "Reconcile",
            Some("Backup completed successfully".to_string()),
        ).await;

        if let Some(ref wh) = backup_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Completed,
                backup_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                None, Some(&now),
            ).await;
        }
    } else if failed > 0 {
        info!(%name, "backup job failed");
        let now = crate::helpers::utc_now_odoo();
        let msg = "backup job failed";
        let patch = json!({"status": {"phase": "Failed", "message": msg, "completionTime": &now}});
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;
        super::odoo_instance::publish_event(
            ctx, backup_job, EventType::Warning, "BackupFailed", "Reconcile",
            Some("Backup job failed".to_string()),
        ).await;

        if let Some(ref wh) = backup_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Failed,
                backup_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                Some(msg), Some(&now),
            ).await;
        }
    }

    Ok(Action::await_change())
}

