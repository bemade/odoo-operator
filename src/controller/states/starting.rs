use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ResourceExt};
use tracing::{info, warn};

use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::cron_depl_name;
use crate::controller::state_machine::scale_deployment;

/// Starting: scale deployment to spec.replicas, waiting for pods to be ready.
///
/// Recovery path for stuck volume mounts: if the snapshot reports any pods
/// belonging to this instance that have persistent FailedMount /
/// FailedAttachVolume events, delete them so the deployment controller
/// recreates them.  A fresh pod usually picks up the volume cleanly because
/// kubelet requests a new CSI attach rather than reusing the stale
/// VolumeAttachment.  This is the level-1 recovery; a VolumeAttachment-level
/// cleanup is deferred until we observe that pod deletion alone isn't
/// sufficient over multiple reconciles.
pub struct Starting;

#[async_trait]
impl State for Starting {
    async fn ensure(
        &self,
        instance: &OdooInstance,
        ctx: &Context,
        snap: &ReconcileSnapshot,
    ) -> Result<()> {
        let ns = instance.namespace().unwrap_or_default();
        let name = instance.name_any();
        let replicas = instance.spec.replicas.max(1);
        let cron_replicas = instance.spec.cron.replicas;
        scale_deployment(&ctx.client, &name, &ns, replicas).await?;
        scale_deployment(
            &ctx.client,
            cron_depl_name(instance).as_str(),
            &ns,
            cron_replicas,
        )
        .await?;

        if !snap.stuck_mount_pods.is_empty() {
            recover_stuck_mounts(&ctx.client, &ns, &snap.stuck_mount_pods).await;
        }

        Ok(())
    }
}

/// Delete pods that kubelet reported mount failures on.  This is safe:
/// the pod is owned by a Deployment (via ReplicaSet), so the deployment
/// controller immediately recreates it.  A new pod gets a new CSI attach
/// request, which in most cases unsticks the previous stale VolumeAttachment.
async fn recover_stuck_mounts(client: &kube::Client, ns: &str, pod_names: &[String]) {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    for pod_name in pod_names {
        info!(
            %ns,
            %pod_name,
            "deleting pod stuck on volume mount — deployment will recreate it"
        );
        match pods.delete(pod_name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(ref e)) if e.code == 404 => {
                // Already gone — racing with a recreate, fine.
            }
            Err(e) => {
                warn!(
                    %ns,
                    %pod_name,
                    error = %e,
                    "failed to delete stuck pod"
                );
            }
        }
    }
}
