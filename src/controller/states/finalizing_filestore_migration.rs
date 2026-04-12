//! FinalizingFilestoreMigration state — rsync is done, PVC rebind in progress.
//!
//! The `ensure()` method keeps deployments at zero and drives the idempotent
//! PVC rebind sequence (delete old PVC, patch PV, create final PVC).
//! Once the final PVC exists with the correct StorageClass, the transition
//! guard fires and clears migration status.

use async_trait::async_trait;
use kube::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::super::helpers::cron_depl_name;
use super::super::odoo_instance::Context;
use super::super::state_machine::{
    finalize_filestore_pvc_rebind, scale_deployment, ReconcileSnapshot,
};
use super::State;

pub struct FinalizingFilestoreMigration;

#[async_trait]
impl State for FinalizingFilestoreMigration {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        // Drive the PVC rebind — idempotent, retries on each reconcile tick.
        finalize_filestore_pvc_rebind(instance, ctx).await?;

        Ok(())
    }
}
