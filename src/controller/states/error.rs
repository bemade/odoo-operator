use async_trait::async_trait;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// Error: something went wrong.  No specific outputs.
pub struct Error;

#[async_trait]
impl State for Error {
    async fn ensure(
        &self,
        _instance: &OdooInstance,
        _ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        Ok(())
    }
}
