use async_trait::async_trait;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// BackingUp: backup job running, deployment stays up (non-disruptive).
/// No scaling needed — deployment is already up from Running.
pub struct BackingUp;

#[async_trait]
impl State for BackingUp {
    async fn on_enter(&self, _instance: &OdooInstance, _ctx: &Context, _snap: &ReconcileSnapshot) -> Result<()> {
        // TODO: absorb backup job controller logic (create K8s Job, patch CRD)
        Ok(())
    }
}
