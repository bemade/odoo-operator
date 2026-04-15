//! FinalizingDatabaseMigration state — migration Job succeeded, switching over.
//!
//! The `ensure()` method keeps deployments at zero.  The transition actions
//! clean up the migration Job, delete the old role on the source cluster,
//! and update `status.activeCluster` so the mismatch guard clears.

use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, DeleteParams, Patch, PatchParams, ResourceExt};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;
use crate::helpers::odoo_username;

use super::super::helpers::{cron_depl_name, FIELD_MANAGER};
use super::super::odoo_instance::{load_postgres_cluster_by_name, Context};
use super::super::state_machine::{scale_deployment, ReconcileSnapshot};
use super::State;

pub struct FinalizingDatabaseMigration;

#[async_trait]
impl State for FinalizingDatabaseMigration {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0 until transition fires.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        Ok(())
    }
}

/// Transition action: delete migration Job, clean up old cluster, update activeCluster.
/// Called by `CompleteDatabaseMigration` when entering this phase.
pub async fn complete_database_migration(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let client = &ctx.client;

    // 1. Delete the migration Job.
    if let Some(ref job_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.db_migration_job_name.clone())
    {
        let jobs: Api<Job> = Api::namespaced(client.clone(), &ns);
        match jobs.delete(job_name, &DeleteParams::background()).await {
            Ok(_) => info!(%inst_name, %job_name, "deleted database migration job"),
            Err(e) => warn!(%inst_name, %job_name, %e, "failed to delete migration job"),
        }
    }

    // 2. Clean up old role/database on the source cluster (best-effort).
    if let Some(ref old_cluster_name) = instance
        .status
        .as_ref()
        .and_then(|s| s.migration_previous_cluster.clone())
    {
        match load_postgres_cluster_by_name(ctx, old_cluster_name).await {
            Ok(old_pg) => {
                let username = odoo_username(&ns, &inst_name);
                if let Err(e) = ctx.postgres.delete_role(&old_pg, &username).await {
                    warn!(
                        %inst_name, %old_cluster_name, %e,
                        "failed to delete old role on source cluster — manual cleanup may be needed"
                    );
                } else {
                    info!(%inst_name, %old_cluster_name, "deleted old role on source cluster");
                }
            }
            Err(e) => {
                warn!(
                    %inst_name, %old_cluster_name, %e,
                    "failed to load old cluster config — cannot clean up old role"
                );
            }
        }
    }

    // 3. Update activeCluster to the new cluster.
    let (new_cluster_name, _) =
        super::super::odoo_instance::load_postgres_cluster(ctx, instance).await?;
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), &ns);
    let patch = json!({
        "status": {
            "activeCluster": &new_cluster_name,
        }
    });
    api.patch_status(
        &inst_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    info!(%inst_name, %new_cluster_name, "updated activeCluster after database migration");

    Ok(())
}

/// Transition action: clear migration status fields.
/// Called by `ClearDatabaseMigrationStatus` when leaving this phase.
pub async fn clear_database_migration_status(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let inst_name = instance.name_any();
    let api: Api<OdooInstance> = Api::namespaced(ctx.client.clone(), &ns);
    let patch = json!({
        "status": {
            "dbMigrationJobName": null,
            "migrationPreviousCluster": null,
            "message": null,
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
