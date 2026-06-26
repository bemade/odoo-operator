use std::collections::BTreeMap;

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{
        Container, PersistentVolumeClaimVolumeSource, PodAffinityTerm, SecurityContext, Volume,
        VolumeMount,
    },
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::events::EventType;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;
use crate::notify;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{cm_env, env, pg_tools_image, OdooJobBuilder, FIELD_MANAGER};

const DUMP_SCRIPT: &str = include_str!("../../../scripts/backup-dump.sh");
const PACKAGE_SCRIPT: &str = include_str!("../../../scripts/backup-package.sh");
const UPLOAD_SCRIPT: &str = include_str!("../../../scripts/backup-upload.sh");

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
        let db = crate::helpers::db_name(instance);
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

        // Detect server major to pick a pg client image whose pg_dump is ≥
        // the running server.  Failure aborts — we can't dump a cluster we
        // can't reach.
        let (_, tgt_pg) = super::super::odoo_instance::load_postgres_cluster(ctx, instance).await?;
        let major = ctx.postgres.detect_server_major_version(&tgt_pg).await?;
        let dump_image = pg_tools_image(major);

        let dump_env = vec![
            cm_env("HOST", &odoo_conf_name, "db_host"),
            cm_env("PORT", &odoo_conf_name, "db_port"),
            cm_env("USER", &odoo_conf_name, "db_user"),
            cm_env("PASSWORD", &odoo_conf_name, "db_password"),
            env("DB_NAME", db.clone()),
            env("BACKUP_FORMAT", format),
        ];

        let package_env = vec![
            env("INSTANCE_NAME", instance_name.clone()),
            env("DB_NAME", db.clone()),
            env("BACKUP_FORMAT", format),
            env("BACKUP_WITH_FILESTORE", with_filestore),
            env("LOCAL_FILENAME", local_filename),
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

        // Volumes: workspace (scratch emptyDir) + filestore PVC.  No odoo-conf —
        // neither container needs it (creds are injected via cm_env).
        let workspace_mount = VolumeMount {
            name: "workspace".into(),
            mount_path: "/workspace".into(),
            ..Default::default()
        };
        let filestore_mount_ro = VolumeMount {
            name: "filestore".into(),
            mount_path: "/var/lib/odoo".into(),
            read_only: Some(true),
            ..Default::default()
        };
        let workspace_vol = Volume {
            name: "workspace".into(),
            empty_dir: Some(Default::default()),
            ..Default::default()
        };
        let filestore_vol = Volume {
            name: "filestore".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: format!("{instance_name}-filestore-pvc"),
                read_only: Some(true),
            }),
            ..Default::default()
        };

        // The package init container runs `apk add zip` (zip format only)
        // which needs root.  The upload container stays non-root.
        let root_sc = SecurityContext {
            run_as_user: Some(0),
            ..Default::default()
        };

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

        info!(%crd_name, %dump_image, server_major = %major, "selected pg client image for backup dump");

        let mut builder = OdooJobBuilder::new(&format!("{crd_name}-"), &ns, backup_job, instance)
            .active_deadline(5400)
            .without_standard_volumes()
            .extra_volumes(vec![workspace_vol, filestore_vol])
            .init_containers(vec![
                Container {
                    name: "dump".into(),
                    image: Some(dump_image),
                    command: Some(vec!["/bin/sh".into(), "-c".into(), DUMP_SCRIPT.into()]),
                    env: Some(dump_env),
                    volume_mounts: Some(vec![workspace_mount.clone()]),
                    ..Default::default()
                },
                Container {
                    name: "package".into(),
                    image: Some("alpine:latest".into()),
                    command: Some(vec!["/bin/sh".into(), "-c".into(), PACKAGE_SCRIPT.into()]),
                    env: Some(package_env),
                    volume_mounts: Some(vec![workspace_mount.clone(), filestore_mount_ro]),
                    security_context: Some(root_sc),
                    ..Default::default()
                },
            ])
            .containers(vec![Container {
                name: "upload".into(),
                image: Some("quay.io/minio/mc:latest".into()),
                command: Some(vec!["/bin/sh".into(), "-c".into(), UPLOAD_SCRIPT.into()]),
                env: Some(upload_env),
                volume_mounts: Some(vec![workspace_mount]),
                ..Default::default()
            }]);

        // Co-locate the backup pod with a running Odoo web pod so a node-bound
        // (RWO) filestore PVC, already mounted there, can also be mounted
        // read-only here.  When the instance is stopped there are no Odoo pods
        // and the PVC is unattached, so the backup pod can bind it on any node;
        // applying the required affinity would instead leave it Pending forever.
        if snap.ready_replicas > 0 {
            builder = builder.affinity(pod_affinity);
        }

        let job = builder.build();

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
