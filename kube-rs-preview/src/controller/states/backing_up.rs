use std::collections::BTreeMap;

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Container, PodAffinityTerm, Volume, VolumeMount},
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::events::EventType;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;
use crate::helpers::sanitise_uid;
use crate::notify;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{cm_env, env, odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER};

const BACKUP_SCRIPT: &str = include_str!("../../../scripts/backup.sh");
const UPLOAD_SCRIPT: &str = include_str!("../../../scripts/s3-upload.sh");

/// BackingUp: backup job running, deployment stays up (non-disruptive).
///
/// Every tick: ensure K8s Job exists (create if missing).  No scaling —
/// deployment stays up.  Job completion/failure is detected by the snapshot
/// and handled by transition actions.
pub struct BackingUp;

#[async_trait]
impl State for BackingUp {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let backup_job = match snap.active_backup_job {
            Some(ref bj) => bj,
            None => return Ok(()),
        };

        // Only create the K8s Job if the CRD hasn't started one yet.
        if backup_job
            .status
            .as_ref()
            .and_then(|s| s.job_name.as_ref())
            .is_some()
        {
            return Ok(());
        }

        let ns = instance.namespace().unwrap_or_default();
        let crd_name = backup_job.name_any();
        let client = &ctx.client;
        let instance_name = instance.name_any();
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

        let with_filestore = if backup_job.spec.with_filestore {
            "true"
        } else {
            "false"
        };

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

        if let Some(ref secret_ref) = dest.s3_credentials_secret_ref {
            let secret_ns = secret_ref.namespace.as_deref().unwrap_or(&ns);
            let secret_name = secret_ref.name.as_deref().unwrap_or_default();
            if let Ok((ak, sk)) = notify::read_s3_credentials(client, secret_name, secret_ns).await
            {
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
                required_during_scheduling_ignored_during_execution: Some(vec![PodAffinityTerm {
                    label_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([("app".into(), instance_name.clone())])),
                        ..Default::default()
                    }),
                    topology_key: "kubernetes.io/hostname".into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let job = OdooJobBuilder::new(&format!("{crd_name}-"), &ns, backup_job, instance)
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

        let jobs_api: Api<Job> = Api::namespaced(client.clone(), &ns);
        let created = jobs_api.create(&PostParams::default(), &job).await?;
        let k8s_job_name = created.name_any();
        info!(%crd_name, %k8s_job_name, "created backup job");

        crate::controller::odoo_instance::publish_event(
            ctx,
            backup_job,
            EventType::Normal,
            "BackupStarted",
            "Reconcile",
            Some(format!("Created backup job {k8s_job_name}")),
        )
        .await;

        let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), &ns);
        let patch = json!({
            "status": {
                "phase": "Running",
                "jobName": k8s_job_name,
                "startTime": crate::helpers::utc_now_odoo(),
            }
        });
        api.patch_status(
            &crd_name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await?;

        Ok(())
    }
}
