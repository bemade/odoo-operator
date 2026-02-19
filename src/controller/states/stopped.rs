use async_trait::async_trait;
use kube::api::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::cron_depl_name;
use crate::controller::state_machine::scale_deployment;

/// Stopped: user set replicas to 0.
pub struct Stopped;

#[async_trait]
impl State for Stopped {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await?;
        scale_deployment(&ctx.client, cron_depl_name(instance).as_str(), &ns, 0).await
    }
}
