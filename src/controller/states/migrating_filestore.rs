//! MigratingFilestore state — waits for the rsync Job to complete.
//!
//! All orchestration (job creation, PVC rebind, rollback) is handled by
//! transition actions in `state_machine.rs`.  The `ensure()` method only
//! keeps both deployments scaled to zero.

use async_trait::async_trait;

use kube::ResourceExt;

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::super::helpers::cron_depl_name;
use super::super::odoo_instance::Context;
use super::super::state_machine::{scale_deployment, ReconcileSnapshot};
use super::State;

pub struct MigratingFilestore;

#[async_trait]
impl State for MigratingFilestore {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        _snapshot: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let inst_name = instance.name_any();
        let client = &ctx.client;

        // Keep both deployments at 0 during migration.
        scale_deployment(client, &inst_name, &ns, 0).await?;
        scale_deployment(client, &cron_depl_name(instance), &ns, 0).await?;

        Ok(())
    }
}
