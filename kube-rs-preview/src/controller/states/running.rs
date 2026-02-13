use async_trait::async_trait;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// Running: steady state.  No outputs to flip — deployment already at target.
pub struct Running;

#[async_trait]
impl State for Running {
    async fn ensure(
        &self,
        _instance: &OdooInstance,
        _ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        Ok(())
    }
}
