//! OdooInitJob controller — creates a batch/v1 Job that runs `odoo -i <modules>`.
//!
//! Mirrors the Go OdooInitJobReconciler: looks up the referenced OdooInstance,
//! scales it down, creates the init Job, and watches it to completion.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::Container,
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

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::crd::shared::Phase;
use crate::error::{Error, Result};
use crate::helpers::sanitise_uid;
use crate::notify;

use super::helpers::{odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER};
use super::odoo_instance::Context;

/// Start the OdooInitJob controller.
pub async fn run(ctx: Arc<Context>) {
    let client = ctx.client.clone();
    let init_jobs: Api<OdooInitJob> = Api::all(client.clone());
    let jobs: Api<Job> = Api::all(client.clone());

    Controller::new(init_jobs, WatcherConfig::default())
        .owns(jobs, WatcherConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok(_) => {}
                Err(e) => warn!("init job reconcile failed: {e:?}"),
            }
        })
        .await;
}

async fn reconcile(init_job: Arc<OdooInitJob>, ctx: Arc<Context>) -> Result<Action> {
    let ns = init_job.namespace().unwrap_or_default();
    let name = init_job.name_any();
    let client = &ctx.client;

    // Terminal states — nothing further to reconcile.
    if let Some(ref status) = init_job.status {
        match status.phase.as_ref() {
            Some(Phase::Completed) | Some(Phase::Failed) => return Ok(Action::await_change()),
            _ => {}
        }
    }

    let job_name = init_job
        .status
        .as_ref()
        .and_then(|s| s.job_name.clone());

    if job_name.is_none() {
        return start_job(&ctx, &init_job, &ns, &name).await;
    }

    sync_job_status(client, &init_job, &ns, &job_name.unwrap(), &ctx).await
}

fn error_policy(_obj: Arc<OdooInitJob>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(%error, "init job reconcile error, requeuing in 30s");
    Action::requeue(Duration::from_secs(30))
}

async fn start_job(
    ctx: &Context,
    init_job: &OdooInitJob,
    ns: &str,
    name: &str,
) -> Result<Action> {
    let client = &ctx.client;
    let instance_name = &init_job.spec.odoo_instance_ref.name;
    let instance_ns = init_job
        .spec
        .odoo_instance_ref
        .namespace
        .as_deref()
        .unwrap_or(ns);

    // Look up the OdooInstance.
    let instances: Api<OdooInstance> = Api::namespaced(client.clone(), instance_ns);
    let instance = instances.get(instance_name).await.map_err(|e| {
        Error::reconcile(format!("OdooInstance {instance_name} not found: {e}"))
    })?;

    // Build and create the Job.
    let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
    let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
    let db = format!("odoo_{}", sanitise_uid(uid));

    let modules = if init_job.spec.modules.is_empty() {
        vec!["base".to_string()]
    } else {
        init_job.spec.modules.clone()
    };

    let job = build_init_job(name, ns, image, &db, &modules, &instance, init_job);
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let created = jobs.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();

    info!(%name, %job_name, "created init job");
    super::odoo_instance::publish_event(
        ctx, init_job, EventType::Normal, "InitStarted", "Reconcile",
        Some(format!("Created init job {job_name}")),
    ).await;

    // Patch status to Running.
    let api: Api<OdooInitJob> = Api::namespaced(client.clone(), ns);
    let patch = json!({
        "status": {
            "phase": "Running",
            "jobName": job_name,
            "startTime": crate::helpers::utc_now_odoo(),
        }
    });
    api.patch_status(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
        .await?;

    Ok(Action::requeue(Duration::from_secs(10)))
}

async fn sync_job_status(
    client: &Client,
    init_job: &OdooInitJob,
    ns: &str,
    job_name: &str,
    ctx: &Context,
) -> Result<Action> {
    let name = init_job.name_any();
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);

    let job = match jobs.get(job_name).await {
        Ok(j) => j,
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            info!(%job_name, "job not found, may have been deleted");
            return Ok(Action::await_change());
        }
        Err(e) => return Err(e.into()),
    };

    let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
    let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0);

    let api: Api<OdooInitJob> = Api::namespaced(client.clone(), ns);

    if succeeded > 0 {
        info!(%name, "init job succeeded");
        super::odoo_instance::publish_event(
            ctx, init_job, EventType::Normal, "InitCompleted", "Reconcile",
            Some("Init job completed successfully".to_string()),
        ).await;
        let now = crate::helpers::utc_now_odoo();
        let patch = json!({
            "status": {
                "phase": "Completed",
                "completionTime": &now,
            }
        });
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
            .await?;

        if let Some(ref wh) = init_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Completed,
                init_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                None, Some(&now),
            ).await;
        }

        return Ok(Action::await_change());
    }

    if failed > 0 {
        info!(%name, "init job failed");
        super::odoo_instance::publish_event(
            ctx, init_job, EventType::Warning, "InitFailed", "Reconcile",
            Some("Init job failed".to_string()),
        ).await;
        let now = crate::helpers::utc_now_odoo();
        let msg = "init job failed";
        let patch = json!({
            "status": {
                "phase": "Failed",
                "message": msg,
                "completionTime": &now,
            }
        });
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch))
            .await?;

        if let Some(ref wh) = init_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &Phase::Failed,
                init_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                Some(msg), Some(&now),
            ).await;
        }

        return Ok(Action::await_change());
    }

    // Still running — Owns(&Job) will trigger reconciliation on status change.
    Ok(Action::await_change())
}

pub fn build_init_job(
    cr_name: &str,
    ns: &str,
    image: &str,
    db_name: &str,
    modules: &[String],
    instance: &OdooInstance,
    init_job: &OdooInitJob,
) -> Job {
    OdooJobBuilder::new(&format!("{cr_name}-"), ns, init_job, instance)
        .containers(vec![Container {
            name: "init".to_string(),
            image: Some(image.to_string()),
            command: Some(vec!["/entrypoint.sh".into(), "odoo".into()]),
            args: Some(vec![
                "-i".into(),
                modules.join(","),
                "-d".into(),
                db_name.to_string(),
                "--no-http".into(),
                "--stop-after-init".into(),
            ]),
            volume_mounts: Some(odoo_volume_mounts()),
            ..Default::default()
        }])
        .build()
}
