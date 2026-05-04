use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{
        Container, PersistentVolumeClaimVolumeSource, SecurityContext, Volume, VolumeMount,
    },
};
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::events::EventType;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_restore_job::{OdooRestoreJob, RestoreSourceType};
use crate::crd::shared::BackupFormat;
use crate::error::Result;
use crate::notify;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{
    cm_env, cron_depl_name, env, pg_tools_image, staging_mail_env_vars, OdooJobBuilder,
    FIELD_MANAGER,
};
use crate::controller::state_machine::scale_deployment;

const S3_DOWNLOAD_SCRIPT: &str = include_str!("../../../scripts/s3-download.sh");
const ODOO_DOWNLOAD_SCRIPT: &str = include_str!("../../../scripts/odoo-download.sh");
const EXTRACT_SCRIPT: &str = include_str!("../../../scripts/restore-extract.sh");
const LOAD_DB_SCRIPT: &str = include_str!("../../../scripts/restore-load-db.sh");
const NEUTRALIZE_SCRIPT: &str = include_str!("../../../scripts/restore-neutralize.sh");

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
        scale_deployment(&ctx.client, cron_depl_name(instance).as_str(), &ns, 0).await?;

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
        let odoo_image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
        let db = crate::helpers::db_name(instance);
        let odoo_conf_name = format!("{instance_name}-odoo-conf");

        // Backup-format string is shared by extract + load-db.  For Odoo
        // pull-from-URL (which can only deliver zip or custom dump), we map
        // BackupFormat::Sql onto a custom dump too.
        let format_str = match restore_job.spec.format {
            BackupFormat::Dump => "dump",
            BackupFormat::Sql => "sql",
            BackupFormat::Zip => "zip",
        };

        // Where the downloader writes.  For zip we land at a generic
        // /workspace/artifact (the extract container picks it up).  For
        // dump/sql we write directly to the file the load-db step expects.
        let output_file = match restore_job.spec.format {
            BackupFormat::Dump => "/workspace/dump.dump",
            BackupFormat::Sql => "/workspace/dump.sql",
            BackupFormat::Zip => "/workspace/artifact",
        };

        let workspace_mount = VolumeMount {
            name: "workspace".into(),
            mount_path: "/workspace".into(),
            ..Default::default()
        };
        let filestore_mount_rw = VolumeMount {
            name: "filestore".into(),
            mount_path: "/var/lib/odoo".into(),
            ..Default::default()
        };

        // Detect target server major and pick a pg client image whose
        // pg_restore is ≥ the running server.  Failure aborts.
        let (_, tgt_pg) = super::super::odoo_instance::load_postgres_cluster(ctx, instance).await?;
        let major = ctx.postgres.detect_server_major_version(&tgt_pg).await?;
        let pg_image = pg_tools_image(major);
        info!(%crd_name, %pg_image, server_major = %major, "selected pg client image for restore");

        let mut init_containers = vec![];
        let src = &restore_job.spec.source;

        // ── init: download ──────────────────────────────────────────────
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
                        volume_mounts: Some(vec![workspace_mount.clone()]),
                        ..Default::default()
                    });
                }
            }
            RestoreSourceType::Odoo => {
                if let Some(ref odoo_src) = src.odoo {
                    // Odoo's master/dump endpoint only delivers zip or custom
                    // dump.  Map our BackupFormat onto whichever it speaks.
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
                        volume_mounts: Some(vec![workspace_mount.clone()]),
                        ..Default::default()
                    });
                }
            }
        }

        // The extract container runs `apk add unzip` which needs root.
        let root_sc = SecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        };

        // ── init: extract (zip only) ────────────────────────────────────
        if restore_job.spec.format == BackupFormat::Zip {
            init_containers.push(Container {
                name: "extract".into(),
                image: Some("alpine:latest".into()),
                command: Some(vec!["/bin/sh".into(), "-c".into(), EXTRACT_SCRIPT.into()]),
                env: Some(vec![
                    env("DB_NAME", db.clone()),
                    env("INPUT_FILE", "/workspace/artifact"),
                ]),
                volume_mounts: Some(vec![workspace_mount.clone(), filestore_mount_rw.clone()]),
                security_context: Some(root_sc.clone()),
                ..Default::default()
            });
        }

        // ── init: load-db ───────────────────────────────────────────────
        let load_env = vec![
            cm_env("HOST", &odoo_conf_name, "db_host"),
            cm_env("PORT", &odoo_conf_name, "db_port"),
            cm_env("USER", &odoo_conf_name, "db_user"),
            cm_env("PASSWORD", &odoo_conf_name, "db_password"),
            env("DB_NAME", db.clone()),
            env("BACKUP_FORMAT", format_str),
        ];
        init_containers.push(Container {
            name: "load-db".into(),
            image: Some(pg_image),
            command: Some(vec!["/bin/sh".into(), "-c".into(), LOAD_DB_SCRIPT.into()]),
            env: Some(load_env),
            volume_mounts: Some(vec![workspace_mount.clone()]),
            ..Default::default()
        });

        // ── main: neutralize (Odoo image) or alpine no-op ───────────────
        let main_container = if restore_job.spec.neutralize {
            let mut neut_env = vec![
                cm_env("HOST", &odoo_conf_name, "db_host"),
                cm_env("PORT", &odoo_conf_name, "db_port"),
                cm_env("USER", &odoo_conf_name, "db_user"),
                cm_env("PASSWORD", &odoo_conf_name, "db_password"),
                env("DB_NAME", db.clone()),
            ];
            neut_env.extend(staging_mail_env_vars(instance, &ctx.defaults));
            Container {
                name: "neutralize".into(),
                image: Some(odoo_image.into()),
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    NEUTRALIZE_SCRIPT.into(),
                ]),
                env: Some(neut_env),
                ..Default::default()
            }
        } else {
            Container {
                name: "noop".into(),
                image: Some("alpine:latest".into()),
                command: Some(vec!["/bin/true".into()]),
                ..Default::default()
            }
        };

        // Volumes: workspace scratch + filestore PVC.  No odoo-conf —
        // creds flow through cm_env.
        let workspace_vol = Volume {
            name: "workspace".into(),
            empty_dir: Some(Default::default()),
            ..Default::default()
        };
        let filestore_vol = Volume {
            name: "filestore".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: format!("{instance_name}-filestore-pvc"),
                read_only: Some(false),
            }),
            ..Default::default()
        };

        let job = OdooJobBuilder::new(&format!("{crd_name}-"), &ns, restore_job, instance)
            .active_deadline(3600)
            .without_standard_volumes()
            .extra_volumes(vec![workspace_vol, filestore_vol])
            .init_containers(init_containers)
            .containers(vec![main_container])
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
