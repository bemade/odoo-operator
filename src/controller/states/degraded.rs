use async_trait::async_trait;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// Degraded: some replicas ready but not all.  No outputs to flip —
/// deployment target is already at spec.replicas from Starting.
pub struct Degraded;

#[async_trait]
impl State for Degraded {
    async fn ensure(
        &self,
        _instance: &OdooInstance,
        _ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        Ok(())
    }
}
