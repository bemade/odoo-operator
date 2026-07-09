//! Garbage collection of finished job CRs.
//!
//! Job CRs (`OdooBackupJob`, `OdooUpgradeJob`, …) are created by external
//! systems (CI, the Odoo backup scheduler) and only reap when their owning
//! `OdooInstance` is deleted — Kubernetes' `TTLAfterFinished` controller acts
//! on `batch/v1` Jobs, not custom resources. Left unbounded they accumulate
//! indefinitely (hundreds per busy namespace), and every reconcile's
//! `ReconcileSnapshot::gather()` does an unfiltered `list()` + client-side
//! filter over all of them, so the per-reconcile cost grows without bound.
//!
//! This module bounds that history the way a Kubernetes CronJob bounds its own
//! Job history: keep the newest `limit` **terminal** (`Completed`/`Failed`) CRs
//! per instance per type and delete the rest. Only terminal CRs are ever
//! considered, so an in-flight job is never touched.

use k8s_openapi::NamespaceResourceScope;
use kube::api::{DeleteParams, ListParams};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use tracing::{info, warn};

use crate::crd::odoo_backup_job::OdooBackupJob;
use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_restore_job::OdooRestoreJob;
use crate::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::crd::shared::Phase;
use crate::error::Result;

use super::odoo_instance::Context;

/// Prune terminal job CRs of type `K` for one instance, keeping the newest
/// `limit` by creation timestamp and deleting the rest. Returns the number of
/// CRs deleted. Deletion is best-effort: an already-gone CR (404) is not an
/// error, and any other per-object delete failure is logged and skipped so a
/// single bad object can't abort the sweep.
async fn prune<K>(
    client: &Client,
    ns: &str,
    instance_name: &str,
    limit: usize,
    ref_name: impl Fn(&K) -> Option<&str>,
    phase_of: impl Fn(&K) -> Option<Phase>,
) -> Result<usize>
where
    K: Resource<DynamicType = (), Scope = NamespaceResourceScope>
        + Clone
        + DeserializeOwned
        + Debug,
{
    let api: Api<K> = Api::namespaced(client.clone(), ns);
    let mut terminal: Vec<K> = api
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|o| ref_name(o) == Some(instance_name))
        .filter(|o| matches!(phase_of(o), Some(Phase::Completed) | Some(Phase::Failed)))
        .collect();

    if terminal.len() <= limit {
        return Ok(0);
    }

    // Newest first. A missing creation timestamp (never happens for a
    // server-persisted object) sorts oldest, so it's deleted first.
    terminal.sort_by(|a, b| {
        let ta = a.meta().creation_timestamp.as_ref().map(|t| t.0);
        let tb = b.meta().creation_timestamp.as_ref().map(|t| t.0);
        tb.cmp(&ta)
    });

    let mut deleted = 0;
    for obj in terminal.into_iter().skip(limit) {
        let name = obj.name_any();
        match api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => deleted += 1,
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(%name, %ns, %e, "failed to GC terminal job CR"),
        }
    }
    Ok(deleted)
}

/// Bound the finished-job-CR history for one instance across all job CR types.
/// A `job_history_limit` of 0 disables GC. Non-fatal: callers should log and
/// continue on error so a GC hiccup never blocks reconciliation.
pub async fn garbage_collect_job_crs(instance: &OdooInstance, ctx: &Context) -> Result<()> {
    let limit = ctx.job_history_limit;
    if limit == 0 {
        return Ok(());
    }
    let Some(ns) = instance.namespace() else {
        return Ok(());
    };
    let name = instance.name_any();
    let client = &ctx.client;

    let mut total = 0;
    total += prune::<OdooBackupJob>(
        client,
        &ns,
        &name,
        limit,
        |o| Some(o.spec.odoo_instance_ref.name.as_str()),
        |o| o.status.as_ref().and_then(|s| s.phase.clone()),
    )
    .await?;
    total += prune::<OdooUpgradeJob>(
        client,
        &ns,
        &name,
        limit,
        |o| Some(o.spec.odoo_instance_ref.name.as_str()),
        |o| o.status.as_ref().and_then(|s| s.phase.clone()),
    )
    .await?;
    total += prune::<OdooRestoreJob>(
        client,
        &ns,
        &name,
        limit,
        |o| Some(o.spec.odoo_instance_ref.name.as_str()),
        |o| o.status.as_ref().and_then(|s| s.phase.clone()),
    )
    .await?;
    total += prune::<OdooInitJob>(
        client,
        &ns,
        &name,
        limit,
        |o| Some(o.spec.odoo_instance_ref.name.as_str()),
        |o| o.status.as_ref().and_then(|s| s.phase.clone()),
    )
    .await?;
    total += prune::<OdooStagingRefreshJob>(
        client,
        &ns,
        &name,
        limit,
        |o| Some(o.spec.odoo_instance_ref.name.as_str()),
        |o| o.status.as_ref().and_then(|s| s.phase.clone()),
    )
    .await?;

    if total > 0 {
        info!(%name, %ns, deleted = total, limit, "garbage-collected terminal job CRs");
    }
    Ok(())
}
