//! MigratingDatabase state — waits for the pg_dump|pg_restore Job to complete.
//!
//! All orchestration (job creation, rollback) is handled by transition actions.
//! The `ensure()` method only keeps both deployments scaled to zero.

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{Container, EnvVar, PodSpec, PodTemplateSpec},
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;
use crate::postgres::PostgresClusterConfig;

use super::super::helpers::{
    cron_depl_name, env, image_pull_secrets, odoo_security_context, odoo_volume_mounts,
    odoo_volumes, FIELD_MANAGER,
};
use super::super::odoo_instance::{load_postgres_cluster_by_name, Context};
use super::super::state_machine::{scale_deployment, ReconcileSnapshot};
use super::State;

const MIGRATE_SCRIPT: &str = include_str!("../../../scripts/migrate-database.sh");

pub struct MigratingDatabase;

#[async_trait]
impl State for MigratingDatabase {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0 during migration.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        Ok(())
    }
}

/// Scale down deployments, create migration Job, store state.
/// Called by the `BeginDatabaseMigration` transition action.
pub async fn begin_database_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    // Scale down both deployments.
    scale_deployment(client, &inst_name, &ns, 0).await?;
    scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

    // Load old cluster config (from status.activeCluster).
    let old_cluster_name = instance
        .status
        .as_ref()
        .and_then(|s| s.active_cluster.as_deref())
        .ok_or_else(|| {
            crate::error::Error::config("cannot begin database migration: activeCluster not set")
        })?;
    let old_pg = load_postgres_cluster_by_name(ctx, old_cluster_name).await?;

    // Load new cluster config (from spec.database.cluster / default).
    let (new_cluster_name, new_pg) =
        super::super::odoo_instance::load_postgres_cluster(ctx, instance).await?;

    // Build env vars with connection details for both clusters.
    let odoo_conf_name = format!("{inst_name}-odoo-conf");
    let db = crate::helpers::db_name(instance);

    let migration_env = build_migration_env(&old_pg, &new_pg, &odoo_conf_name, &db);

    // Build the migration Job.
    let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
    let job = build_migration_job(&inst_name, &ns, instance, image, migration_env);
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    let created = jobs.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();
    info!(%inst_name, %job_name, from = %old_cluster_name, to = %new_cluster_name, "created database migration job");

    // Store migration state.
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    let patch = json!({
        "status": {
            "dbMigrationJobName": &job_name,
            "migrationPreviousCluster": old_cluster_name,
            "message": format!("Migrating database from cluster {old_cluster_name} to {new_cluster_name}"),
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// Rollback: delete job, revert spec to previous cluster, clear status.
/// Called by the `RollbackDatabaseMigration` transition action.
pub async fn rollback_database_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    let prev_cluster = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_previous_cluster.clone())
        .unwrap_or_else(|| "unknown".to_string());

    warn!(%inst_name, %prev_cluster, "rolling back database migration");

    // Delete migration job.
    let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
    if let Some(ref job_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.db_migration_job_name.clone())
    {
        let _ = jobs.delete(job_name, &DeleteParams::background()).await;
    }

    // Revert spec.database.cluster to previous value.
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    if prev_cluster != "unknown" {
        let spec_patch = json!({"spec": {"database": {"cluster": &prev_cluster}}});
        api.patch(
            &inst_name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&spec_patch),
        )
        .await?;
    }

    // Clear migration status.
    let status_patch = json!({
        "status": {
            "dbMigrationJobName": null,
            "migrationPreviousCluster": null,
            "message": format!("Database migration rolled back to cluster {prev_cluster}"),
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&status_patch),
    )
    .await?;
    Ok(())
}

/// Build env vars for the migration Job with both source and destination creds.
fn build_migration_env(
    old_pg: &PostgresClusterConfig,
    new_pg: &PostgresClusterConfig,
    odoo_conf_name: &str,
    db_name: &str,
) -> Vec<EnvVar> {
    use super::super::helpers::cm_env;
    vec![
        // Source cluster (admin creds injected directly).
        env("SRC_HOST", &old_pg.host),
        env("SRC_PORT", old_pg.port.to_string()),
        env("SRC_ADMIN_USER", &old_pg.admin_user),
        env("SRC_ADMIN_PASSWORD", &old_pg.admin_password),
        // Destination cluster (admin creds injected directly).
        env("DST_HOST", &new_pg.host),
        env("DST_PORT", new_pg.port.to_string()),
        env("DST_ADMIN_USER", &new_pg.admin_user),
        env("DST_ADMIN_PASSWORD", &new_pg.admin_password),
        // Odoo role credentials (from ConfigMap, which has already been
        // updated to point to the new cluster — but user/password are the same).
        cm_env("DB_USER", odoo_conf_name, "db_user"),
        cm_env("DB_PASSWORD", odoo_conf_name, "db_password"),
        // Database name.
        env("DB_NAME", db_name),
    ]
}

/// Build the migration batch/v1 Job.
fn build_migration_job(
    inst_name: &str,
    ns: &str,
    instance: &OdooInstance,
    image: &str,
    env_vars: Vec<EnvVar>,
) -> Job {
    Job {
        metadata: ObjectMeta {
            generate_name: Some(format!("{inst_name}-migrate-db-")),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(7200),
            ttl_seconds_after_finished: Some(300),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(
                        [("app".to_string(), inst_name.to_string())]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    security_context: Some(odoo_security_context()),
                    image_pull_secrets: image_pull_secrets(instance),
                    containers: vec![Container {
                        name: "migrate-db".to_string(),
                        image: Some(image.to_string()),
                        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
                        args: Some(vec![MIGRATE_SCRIPT.to_string()]),
                        env: Some(env_vars),
                        volume_mounts: Some(odoo_volume_mounts()),
                        ..Default::default()
                    }],
                    volumes: Some(odoo_volumes(inst_name)),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
