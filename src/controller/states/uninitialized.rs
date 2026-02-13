use async_trait::async_trait;
use kube::api::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::state_machine::scale_deployment;

/// Uninitialized: waiting for an OdooInitJob or OdooRestoreJob to be created.
/// Scale deployment to 0 (not yet initialized).
pub struct Uninitialized;

#[async_trait]
impl State for Uninitialized {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await
    }
}
