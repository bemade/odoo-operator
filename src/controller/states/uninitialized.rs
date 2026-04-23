use async_trait::async_trait;
use kube::api::{Api, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::{Environment, OdooInstance};
use crate::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;
use crate::error::{Error, Result};

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::{controller_owner_ref, cron_depl_name};
use crate::controller::state_machine::{scale_deployment, JobStatus};

/// Uninitialized: waiting for an OdooInitJob, OdooRestoreJob, or
/// OdooStagingRefreshJob to be created. Scale deployment to 0 (not yet
/// initialized).
///
/// Auto-creates one of:
///   - `OdooStagingRefreshJob` when `spec.productionInstanceRef` is set
///     (clones from the named source prod instance), OR
///   - `OdooInitJob` when `spec.init.enabled` is true,
///
/// whichever applies — productionInstanceRef takes precedence because the
/// refresh pipeline brings the DB up initialized and neutralized, replacing
/// the init step. The branches are mutually exclusive.
pub struct Uninitialized;

#[async_trait]
impl State for Uninitialized {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        scale_deployment(&ctx.client, &name, &ns, 0).await?;
        scale_deployment(&ctx.client, cron_depl_name(instance).as_str(), &ns, 0).await?;

        // Only auto-create when no job of any kind is already present and
        // the DB hasn't been initialized yet.
        if snap.db_initialized
            || snap.init_job != JobStatus::Absent
            || snap.refresh_job != JobStatus::Absent
            || snap.restore_job != JobStatus::Absent
        {
            return Ok(());
        }

        if let Some(pref) = instance.spec.production_instance_ref.as_ref() {
            // Reconciler-level guard — belt-and-suspenders with the CRD CEL
            // rule that forbids productionInstanceRef on Production.
            if instance.spec.environment == Environment::Production {
                return Err(Error::Config(
                    "spec.productionInstanceRef is forbidden on production instances".into(),
                ));
            }

            let auto_refresh_name = format!("{name}-auto-refresh");
            let mut source_spec = serde_json::json!({ "instanceName": &pref.name });
            if let Some(src_ns) = pref.namespace.as_ref() {
                source_spec["instanceNamespace"] = serde_json::Value::String(src_ns.clone());
            }
            let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
                "apiVersion": "bemade.org/v1alpha1",
                "kind": "OdooStagingRefreshJob",
                "metadata": {
                    "name": &auto_refresh_name,
                    "namespace": &ns,
                    "ownerReferences": [controller_owner_ref(instance)],
                    "labels": { "bemade.org/auto-refresh": "true" },
                },
                "spec": {
                    "odooInstanceRef": { "name": &name },
                    "source": source_spec,
                    // filestoreMethod, skipFilestore, neutralize all fall
                    // back to their CRD defaults (Auto / false / true).
                }
            }))?;

            let api: Api<OdooStagingRefreshJob> = Api::namespaced(ctx.client.clone(), &ns);
            api.create(&PostParams::default(), &refresh).await?;
            info!(
                %name,
                %auto_refresh_name,
                source = %pref.name,
                "auto-created OdooStagingRefreshJob from spec.productionInstanceRef"
            );
            return Ok(());
        }

        // Fall back to the normal auto-init path.
        let init_spec = &instance.spec.init;
        if init_spec.enabled {
            let auto_init_name = format!("{name}-auto-init");
            let init_job: OdooInitJob = serde_json::from_value(json!({
                "apiVersion": "bemade.org/v1alpha1",
                "kind": "OdooInitJob",
                "metadata": {
                    "name": &auto_init_name,
                    "namespace": &ns,
                    "ownerReferences": [controller_owner_ref(instance)],
                },
                "spec": {
                    "odooInstanceRef": { "name": &name },
                    "modules": init_spec.modules,
                    "demo": init_spec.demo,
                    "webhook": init_spec.webhook,
                }
            }))?;

            let api: Api<OdooInitJob> = Api::namespaced(ctx.client.clone(), &ns);
            api.create(&PostParams::default(), &init_job).await?;
            info!(%name, %auto_init_name, "auto-created OdooInitJob from spec.init");
        }

        Ok(())
    }
}
