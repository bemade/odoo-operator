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
use std::collections::BTreeMap;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;
use crate::postgres::PostgresClusterConfig;

use super::super::helpers::{
    cron_depl_name, delete_job_credentials_secret, ensure_job_credentials_secret, env,
    image_pull_secrets, odoo_security_context, pg_tools_image, secret_env, FIELD_MANAGER,
};
use super::super::odoo_instance::{load_postgres_cluster_by_name, Context};
use super::super::state_machine::{scale_deployment, wait_for_pods_gone, ReconcileSnapshot};
use super::State;

const MIGRATE_SCRIPT: &str = include_str!("../../../scripts/migrate-database.sh");

/// How long to wait for the web and cron pods to terminate before giving up on
/// starting the migration.  Generous: an Odoo worker with long-running requests
/// can take a while to drain, and aborting is cheap — the reconcile retries.
const POD_TERMINATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Name of the short-lived Secret holding the two cluster admin passwords the
/// migration Job needs.  The tenant namespace cannot `secretKeyRef` the
/// operator-namespace `postgres-clusters` Secret (secretKeyRef is
/// namespace-local), so the operator materialises the two values it needs here
/// and removes the Secret once the migration settles.
pub fn migration_creds_secret_name(inst_name: &str) -> String {
    format!("{inst_name}-migrate-db-creds")
}

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

    // Scale down both deployments, then wait for the pods to actually go away.
    // Scaling only patches `replicas: 0`; Odoo shuts down gracefully and keeps
    // its connection pool open for several seconds afterwards.  Creating the
    // Job before that has finished lets a still-live pod block the Job's
    // `DROP DATABASE` (issue #172).
    let cron_name = cron_depl_name(instance);
    scale_deployment(client, &inst_name, &ns, 0).await?;
    scale_deployment(client, &cron_name, &ns, 0).await?;
    wait_for_pods_gone(
        client,
        &ns,
        &[&inst_name, &cron_name],
        POD_TERMINATION_TIMEOUT,
    )
    .await?;

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

    // Materialise the cluster admin passwords into a short-lived Secret in the
    // tenant namespace so the Job can reference them rather than carrying them
    // as literal env values in its own manifest.
    let creds_secret = migration_creds_secret_name(&inst_name);
    ensure_job_credentials_secret(
        client,
        &ns,
        &creds_secret,
        instance,
        BTreeMap::from([
            (
                "SRC_ADMIN_PASSWORD".to_string(),
                old_pg.admin_password.clone(),
            ),
            (
                "DST_ADMIN_PASSWORD".to_string(),
                new_pg.admin_password.clone(),
            ),
        ]),
    )
    .await?;

    let migration_env = build_migration_env(&old_pg, &new_pg, &odoo_conf_name, &db, &creds_secret);

    // Detect server major versions on both clusters and pick an image whose
    // pg client tools satisfy `pg_dump/pg_restore >= server_major` on both ends.
    // Failing this query aborts the transition — we can't migrate a cluster
    // we can't reach.
    let src_major = ctx.postgres.detect_server_major_version(&old_pg).await?;
    let dst_major = ctx.postgres.detect_server_major_version(&new_pg).await?;
    let tools_major = src_major.max(dst_major);
    let image = pg_tools_image(tools_major);
    info!(
        %inst_name, src_major, dst_major, %image,
        "selected pg client image for migration"
    );

    // Build the migration Job.
    let job = build_migration_job(&inst_name, &ns, instance, &image, migration_env);
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

    // Drop the short-lived admin-credential Secret along with the Job.
    let _ =
        delete_job_credentials_secret(client, &ns, &migration_creds_secret_name(&inst_name)).await;

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
    creds_secret: &str,
) -> Vec<EnvVar> {
    use super::super::helpers::cm_env;
    vec![
        // Source cluster (admin password via secretKeyRef, never a literal).
        env("SRC_HOST", &old_pg.host),
        env("SRC_PORT", old_pg.port.to_string()),
        env("SRC_ADMIN_USER", &old_pg.admin_user),
        secret_env("SRC_ADMIN_PASSWORD", creds_secret, "SRC_ADMIN_PASSWORD"),
        // Destination cluster (admin password via secretKeyRef, never a literal).
        env("DST_HOST", &new_pg.host),
        env("DST_PORT", new_pg.port.to_string()),
        env("DST_ADMIN_USER", &new_pg.admin_user),
        secret_env("DST_ADMIN_PASSWORD", creds_secret, "DST_ADMIN_PASSWORD"),
        // Odoo role credentials.  The ConfigMap deliberately still points at
        // the *source* cluster for the duration of the migration (see
        // `database_host_cluster`), but the role name and password are the same
        // on both ends, so it remains the right place to read them from.
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
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
