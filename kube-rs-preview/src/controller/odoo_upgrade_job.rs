//! OdooUpgradeJob controller — creates a Job that runs `odoo -u <modules>`.

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

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::crd::shared::Phase;
use crate::error::{Error, Result};
use crate::helpers::sanitise_uid;
use crate::notify;

use super::helpers::{odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER};
use super::odoo_instance::Context;

pub async fn run(ctx: Arc<Context>) {
    let client = ctx.client.clone();
    let upgrade_jobs: Api<OdooUpgradeJob> = Api::all(client.clone());
    let jobs: Api<Job> = Api::all(client.clone());

    Controller::new(upgrade_jobs, WatcherConfig::default())
        .owns(jobs, WatcherConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("upgrade job reconcile failed: {e:?}");
            }
        })
        .await;
}

async fn reconcile(upgrade_job: Arc<OdooUpgradeJob>, ctx: Arc<Context>) -> Result<Action> {
    let ns = upgrade_job.namespace().unwrap_or_default();
    let name = upgrade_job.name_any();

    if let Some(ref status) = upgrade_job.status {
        match status.phase.as_ref() {
            Some(Phase::Completed) | Some(Phase::Failed) => return Ok(Action::await_change()),
            _ => {}
        }
    }

    // Respect scheduledTime.
    if let Some(ref scheduled) = upgrade_job.spec.scheduled_time {
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(scheduled) {
            let t_utc: chrono::DateTime<chrono::Utc> = t.into();
            let now = chrono::Utc::now();
            if t_utc > now {
                let wait = (t_utc - now).to_std().unwrap_or(Duration::from_secs(60));
                info!(%name, ?wait, "upgrade scheduled in the future, requeueing");
                return Ok(Action::requeue(wait));
            }
        }
    }

    let job_name = upgrade_job.status.as_ref().and_then(|s| s.job_name.clone());

    if job_name.is_none() {
        return start_job(&ctx, &upgrade_job, &ns, &name).await;
    }

    sync_job_status(&ctx.client, &upgrade_job, &ns, &job_name.unwrap(), &ctx).await
}

fn error_policy(_: Arc<OdooUpgradeJob>, error: &Error, _: Arc<Context>) -> Action {
    warn!(%error, "upgrade job error, requeuing in 30s");
    Action::requeue(Duration::from_secs(30))
}

async fn start_job(
    ctx: &Context,
    upgrade_job: &OdooUpgradeJob,
    ns: &str,
    name: &str,
) -> Result<Action> {
    let client = &ctx.client;
    let instance_name = &upgrade_job.spec.odoo_instance_ref.name;
    let instance_ns = upgrade_job
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

    // Build -u and -i argument lists.
    let mut args = vec!["-d".to_string(), db, "--no-http".to_string(), "--stop-after-init".to_string()];
    if !upgrade_job.spec.modules.is_empty() {
        args.push("-u".to_string());
        args.push(upgrade_job.spec.modules.join(","));
    }
    if !upgrade_job.spec.modules_install.is_empty() {
        args.push("-i".to_string());
        args.push(upgrade_job.spec.modules_install.join(","));
    }
    if upgrade_job.spec.modules.is_empty() && upgrade_job.spec.modules_install.is_empty() {
        args.push("-u".to_string());
        args.push("all".to_string());
    }

    let job = OdooJobBuilder::new(&format!("{name}-"), ns, upgrade_job, &instance)
        .active_deadline(3600)
        .containers(vec![Container {
            name: "odoo-upgrade".into(),
            image: Some(image.into()),
            command: Some(vec!["/entrypoint.sh".into(), "odoo".into()]),
            args: Some(args),
            volume_mounts: Some(odoo_volume_mounts()),
            ..Default::default()
        }])
        .build();

    let jobs_api: Api<Job> = Api::namespaced(client.clone(), ns);
    let created = jobs_api.create(&PostParams::default(), &job).await?;
    let job_name = created.name_any();
    info!(%name, %job_name, "created upgrade job");
    super::odoo_instance::publish_event(
        ctx, upgrade_job, EventType::Normal, "UpgradeStarted", "Reconcile",
        Some(format!("Created upgrade job {job_name}")),
    ).await;

    let api: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), ns);
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
    upgrade_job: &OdooUpgradeJob,
    ns: &str,
    job_name: &str,
    ctx: &Context,
) -> Result<Action> {
    let name = upgrade_job.name_any();
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);

    let job = match jobs.get(job_name).await {
        Ok(j) => j,
        Err(kube::Error::Api(ref e)) if e.code == 404 => return Ok(Action::await_change()),
        Err(e) => return Err(e.into()),
    };

    let api: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), ns);
    let succeeded = job.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
    let failed = job.status.as_ref().and_then(|s| s.failed).unwrap_or(0);

    if succeeded > 0 || failed > 0 {
        let now = crate::helpers::utc_now_odoo();
        let (phase_str, phase_enum, message) = if succeeded > 0 {
            info!(%name, "upgrade job succeeded");
            super::odoo_instance::publish_event(
                ctx, upgrade_job, EventType::Normal, "UpgradeCompleted", "Reconcile",
                Some("Upgrade completed successfully".to_string()),
            ).await;
            ("Completed", Phase::Completed, None)
        } else {
            info!(%name, "upgrade job failed");
            super::odoo_instance::publish_event(
                ctx, upgrade_job, EventType::Warning, "UpgradeFailed", "Reconcile",
                Some("Upgrade job failed".to_string()),
            ).await;
            ("Failed", Phase::Failed, Some("upgrade job failed"))
        };

        let mut patch_val = json!({
            "status": {
                "phase": phase_str,
                "completionTime": &now,
            }
        });
        if let Some(msg) = message {
            patch_val["status"]["message"] = json!(msg);
        }
        api.patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch_val)).await?;

        if let Some(ref wh) = upgrade_job.spec.webhook {
            notify::notify_job_webhook(
                client, &ctx.http_client, ns, wh, &phase_enum,
                upgrade_job.status.as_ref().and_then(|s| s.job_name.as_deref()),
                message, Some(&now),
            ).await;
        }
    }

    Ok(Action::await_change())
}
