use async_trait::async_trait;
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::FIELD_MANAGER;
use crate::controller::odoo_init_job::build_init_job;
use crate::controller::state_machine::scale_deployment;

/// Initializing: init job is running, deployment must be scaled down.
/// On entry: scale to 0, create the K8s Job if the CRD hasn't started one
/// yet, patch the OdooInitJob CRD status to Running.
pub struct Initializing;

#[async_trait]
impl State for Initializing {
    async fn on_enter(&self, instance: &OdooInstance, ctx: &Context, snap: &ReconcileSnapshot) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await?;

        if let Some(ref init_job) = snap.active_init_job {
            let crd_name = init_job.name_any();
            if init_job.status.as_ref().and_then(|s| s.job_name.as_ref()).is_none() {
                let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
                let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
                let db = format!("odoo_{}", crate::helpers::sanitise_uid(uid));
                let modules = if init_job.spec.modules.is_empty() {
                    vec!["base".to_string()]
                } else {
                    init_job.spec.modules.clone()
                };
                let job = build_init_job(&crd_name, &ns, image, &db, &modules, instance, init_job);
                let jobs: Api<k8s_openapi::api::batch::v1::Job> =
                    Api::namespaced(ctx.client.clone(), &ns);
                let created = jobs.create(&PostParams::default(), &job).await?;
                let k8s_job_name = created.name_any();
                info!(%crd_name, %k8s_job_name, "created init job");

                let api: Api<OdooInitJob> = Api::namespaced(ctx.client.clone(), &ns);
                let patch = json!({
                    "status": {
                        "phase": "Running",
                        "jobName": k8s_job_name,
                        "startTime": crate::helpers::utc_now_odoo(),
                    }
                });
                api.patch_status(&crd_name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(&patch)).await?;
            }
        }
        Ok(())
    }
}
