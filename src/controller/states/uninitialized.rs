use async_trait::async_trait;
use kube::api::{Api, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::controller_owner_ref;
use crate::controller::state_machine::{scale_deployment, JobStatus};

/// Uninitialized: waiting for an OdooInitJob or OdooRestoreJob to be created.
/// Scale deployment to 0 (not yet initialized).
///
/// When `spec.init.enabled` is true (the default), automatically creates an
/// OdooInitJob CR so the state machine can transition to Initializing without
/// external intervention.
pub struct Uninitialized;

#[async_trait]
impl State for Uninitialized {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await?;

        // Auto-create an OdooInitJob if auto-init is enabled and no job exists yet.
        let init_spec = &instance.spec.init;
        if init_spec.enabled && snap.init_job == JobStatus::Absent && !snap.db_initialized {
            let auto_init_name = format!("{name}-auto-init");
            let init_job: OdooInitJob = serde_json::from_value(json!({
                "apiVersion": "bemade.org/v1alpha1",
                "kind": "OdooInitJob",
                "metadata": {
                    "name": &auto_init_name,
                    "namespace": &ns,
                    "ownerReferences": [controller_owner_ref(instance)],
                },
                "spec": {
                    "odooInstanceRef": { "name": &name },
                    "modules": init_spec.modules,
                    "demo": init_spec.demo,
                    "webhook": init_spec.webhook,
                }
            }))?;

            let api: Api<OdooInitJob> = Api::namespaced(ctx.client.clone(), &ns);
            api.create(&PostParams::default(), &init_job).await?;
            info!(%name, %auto_init_name, "auto-created OdooInitJob from spec.init");
        }

        Ok(())
    }
}
