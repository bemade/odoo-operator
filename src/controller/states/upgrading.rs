use async_trait::async_trait;
use k8s_openapi::api::{batch::v1::Job, core::v1::Container};
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use kube::runtime::events::EventType;
use serde_json::json;
use tracing::info;

use crate::crd::odoo_instance::OdooInstance;
use crate::crd::odoo_upgrade_job::OdooUpgradeJob;
use crate::error::Result;
use crate::helpers::sanitise_uid;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{odoo_volume_mounts, OdooJobBuilder, FIELD_MANAGER};
use crate::controller::state_machine::scale_deployment;

/// Upgrading: upgrade job running, deployment must be down.
///
/// Every tick: ensure deployment scaled to 0, ensure K8s Job exists (create if
/// missing).  Job completion/failure is detected by the snapshot and handled
/// by transition actions.
pub struct Upgrading;

#[async_trait]
impl State for Upgrading {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        scale_deployment(&ctx.client, &inst_name, &ns, 0).await?;

        let upgrade_job = match snap.active_upgrade_job {
            Some(ref uj) => uj,
            None => return Ok(()),
        };

        // Only create the K8s Job if the CRD hasn't started one yet.
        if upgrade_job
            .status
            .as_ref()
            .and_then(|s| s.job_name.as_ref())
            .is_some()
        {
            return Ok(());
        }

        let crd_name = upgrade_job.name_any();
        let client = &ctx.client;
        let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
        let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
        let db = format!("odoo_{}", sanitise_uid(uid));

        let mut args = vec![
            "-d".to_string(),
            db,
            "--no-http".to_string(),
            "--stop-after-init".to_string(),
        ];
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

        let job = OdooJobBuilder::new(&format!("{crd_name}-"), &ns, upgrade_job, instance)
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

        let jobs_api: Api<Job> = Api::namespaced(client.clone(), &ns);
        let created = jobs_api.create(&PostParams::default(), &job).await?;
        let k8s_job_name = created.name_any();
        info!(%crd_name, %k8s_job_name, "created upgrade job");

        crate::controller::odoo_instance::publish_event(
            ctx,
            upgrade_job,
            EventType::Normal,
            "UpgradeStarted",
            "Reconcile",
            Some(format!("Created upgrade job {k8s_job_name}")),
        )
        .await;

        let api: Api<OdooUpgradeJob> = Api::namespaced(client.clone(), &ns);
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
