use async_trait::async_trait;
use kube::api::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::state_machine::scale_deployment;

/// Upgrading: upgrade job running, deployment must be down.
/// On entry: scale to 0, create the K8s Job, patch CRD status.
pub struct Upgrading;

#[async_trait]
impl State for Upgrading {
    async fn on_enter(&self, instance: &OdooInstance, ctx: &Context, _snap: &ReconcileSnapshot) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await
        // TODO: absorb upgrade job controller logic (create K8s Job, patch CRD)
    }
}
