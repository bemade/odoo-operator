use async_trait::async_trait;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};

/// Provisioning: initial phase.  The ensure_* calls (Secret, PVC, ConfigMap,
/// Service, Ingress, Deployment) will be migrated here.
pub struct Provisioning;

#[async_trait]
impl State for Provisioning {
    async fn ensure(
        &self,
        _instance: &OdooInstance,
        _ctx: &Context,
        _snap: &ReconcileSnapshot,
    ) -> Result<()> {
        // TODO: absorb ensure_* calls from reconcile_instance
        Ok(())
    }
}
