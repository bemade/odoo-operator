use async_trait::async_trait;
use tracing::info;

use crate::controller::helpers::cron_depl_name;
use crate::controller::state_machine::scale_deployment;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// Running: steady state.  Ensures the Deployment replica count matches
/// `spec.replicas` so that live scaling (without a phase transition) works.
pub struct Running;

#[async_trait]
impl State for Running {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let name = instance.metadata.name.as_deref().unwrap_or_default();
        let ns = instance.metadata.namespace.as_deref().unwrap_or_default();
        let desired = instance.spec.replicas;

        if snap.deployment_replicas != desired {
            info!(%name, from = snap.deployment_replicas, to = desired, "scaling deployment");
            scale_deployment(&ctx.client, name, ns, desired).await?;
        }

        let desired = instance.spec.cron.replicas;
        let cron_depl_name = cron_depl_name(instance);
        info!(
            desired = desired,
            actual = snap.cron_deployment_replicas,
            "cron replicas while reconciling running state."
        );
        if snap.cron_deployment_replicas != desired {
            info!(%cron_depl_name, from = snap.cron_deployment_replicas, to = desired, "scaling cron deployment");
            scale_deployment(&ctx.client, cron_depl_name.as_str(), ns, desired).await?;
        }
        Ok(())
    }
}
