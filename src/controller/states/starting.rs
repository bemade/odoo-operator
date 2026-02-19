use async_trait::async_trait;
use kube::api::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::cron_depl_name;
use crate::controller::state_machine::scale_deployment;

/// Starting: scale deployment to spec.replicas, waiting for pods to be ready.
pub struct Starting;

#[async_trait]
impl State for Starting {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        let replicas = instance.spec.replicas.max(1);
        let cron_replicas = instance.spec.cron.replicas;
        scale_deployment(&ctx.client, &name, &ns, replicas).await?;
        scale_deployment(
            &ctx.client,
            cron_depl_name(instance).as_str(),
            &ns,
            cron_replicas,
        )
        .await
    }
}
