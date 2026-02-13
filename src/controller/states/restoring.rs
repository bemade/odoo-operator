use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Container, Volume, VolumeMount},
};
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::events::EventType;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_restore_job::{OdooRestoreJob, RestoreSourceType};
use crate::crd::shared::BackupFormat;
use crate::error::Result;
use crate::helpers::sanitise_uid;
use crate::notify;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{cm_env, env, odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER};
use crate::controller::state_machine::scale_deployment;

const S3_DOWNLOAD_SCRIPT: &str = include_str!("../../../scripts/s3-download.sh");
const ODOO_DOWNLOAD_SCRIPT: &str = include_str!("../../../scripts/odoo-download.sh");
const RESTORE_SCRIPT: &str = include_str!("../../../scripts/restore.sh");

/// Restoring: restore job running, deployment must be down.
///
/// Every tick: ensure deployment scaled to 0, ensure K8s Job exists (create if
/// missing).  Job completion/failure is detected by the snapshot and handled
/// by transition actions — not here.
pub struct Restoring;

#[async_trait]
impl State for Restoring {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        scale_deployment(&ctx.client, &inst_name, &ns, 0).await?;

        let restore_job = match snap.active_restore_job {
            Some(ref rj) => rj,
            None => return Ok(()),
        };

        // Only create the K8s Job if the CRD hasn't started one yet.
        if restore_job
            .status
            .as_ref()
            .and_then(|s| s.job_name.as_ref())
            .is_some()
        {
            return Ok(());
        }

        let crd_name = restore_job.name_any();
        let client = &ctx.client;
        let instance_name = instance.name_any();
        let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
        let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
        let db = format!("odoo_{}", sanitise_uid(uid));
        let odoo_conf_name = format!("{instance_name}-odoo-conf");

        let neutralize = if restore_job.spec.neutralize {
            "True"
        } else {
            "False"
        };
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
                    if let Some(ref secret_ref) = s3.s3_credentials_secret_ref {
                        let secret_ns = secret_ref.namespace.as_deref().unwrap_or(&ns);
                        let secret_name = secret_ref.name.as_deref().unwrap_or_default();
                        if let Ok((ak, sk)) =
                            notify::read_s3_credentials(client, secret_name, secret_ns).await
                        {
                            dl_env.push(env("AWS_ACCESS_KEY_ID", ak));
                            dl_env.push(env("AWS_SECRET_ACCESS_KEY", sk));
                        }
                    }
                    init_containers.push(Container {
                        name: "download".into(),
                        image: Some("quay.io/minio/mc:latest".into()),
                        command: Some(vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            S3_DOWNLOAD_SCRIPT.into(),
                        ]),
                        env: Some(dl_env),
                        volume_mounts: Some(vec![shared_mount.clone()]),
                        ..Default::default()
                    });
                }
            }
            RestoreSourceType::Odoo => {
                if let Some(ref odoo_src) = src.odoo {
                    let backup_format = if restore_job.spec.format != BackupFormat::Zip {
                        "dump"
                    } else {
                        "zip"
                    };
                    let dl_env = vec![
                        env("ODOO_URL", odoo_src.url.clone()),
                        env(
                            "SOURCE_DB",
                            odoo_src.source_database.clone().unwrap_or_default(),
                        ),
                        env(
                            "MASTER_PASSWORD",
                            odoo_src.master_password.clone().unwrap_or_default(),
                        ),
                        env("BACKUP_FORMAT", backup_format),
                        env("OUTPUT_FILE", output_file),
                    ];
                    init_containers.push(Container {
                        name: "download".into(),
                        image: Some("curlimages/curl:latest".into()),
                        command: Some(vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            ODOO_DOWNLOAD_SCRIPT.into(),
                        ]),
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

        let job = OdooJobBuilder::new(&format!("{crd_name}-"), &ns, restore_job, instance)
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

        let jobs_api: Api<Job> = Api::namespaced(client.clone(), &ns);
        let created = jobs_api.create(&PostParams::default(), &job).await?;
        let k8s_job_name = created.name_any();
        info!(%crd_name, %k8s_job_name, "created restore job");

        crate::controller::odoo_instance::publish_event(
            ctx,
            restore_job,
            EventType::Normal,
            "RestoreStarted",
            "Reconcile",
            Some(format!("Created restore job {k8s_job_name}")),
        )
        .await;

        let api: Api<OdooRestoreJob> = Api::namespaced(client.clone(), &ns);
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
