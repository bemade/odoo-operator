use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Container;
use kube::api::{Api, Patch, PatchParams, PostParams, ResourceExt};
use serde_json::json;
use tracing::info;

use crate::crd::odoo_init_job::OdooInitJob;
use crate::crd::odoo_instance::OdooInstance;
use crate::error::Result;

use super::{Context, ReconcileSnapshot, State};
use crate::controller::helpers::FIELD_MANAGER;
use crate::controller::state_machine::scale_deployment;

use crate::controller::helpers::{cron_depl_name, odoo_volume_mounts, OdooJobBuilder};

/// Initializing: init job is running, deployment must be scaled down.
/// On entry: scale to 0, create the K8s Job if the CRD hasn't started one
/// yet, patch the OdooInitJob CRD status to Running.
pub struct Initializing;

#[async_trait]
impl State for Initializing {
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

        if let Some(ref init_job) = snap.active_init_job {
            let crd_name = init_job.name_any();
            if init_job
                .status
                .as_ref()
                .and_then(|s| s.job_name.as_ref())
                .is_none()
            {
                let image = instance.spec.image.as_deref().unwrap_or("odoo:18.0");
                let db = crate::helpers::db_name(instance);
                let modules = if init_job.spec.modules.is_empty() {
                    vec!["base".to_string()]
                } else {
                    init_job.spec.modules.clone()
                };
                let job = build_init_job(&crd_name, &ns, image, &db, &modules, instance, init_job);
                let jobs: Api<k8s_openapi::api::batch::v1::Job> =
                    Api::namespaced(ctx.client.clone(), &ns);
                let created = jobs.create(&PostParams::default(), &job).await?;
                let k8s_job_name = created.name_any();
                info!(%crd_name, %k8s_job_name, "created init job");

                let api: Api<OdooInitJob> = Api::namespaced(ctx.client.clone(), &ns);
                let patch = json!({
                    "status": {
                        "phase": "Running",
                        "jobName": k8s_job_name,
                        "startTime": crate::helpers::utc_now_odoo(),
                    }
                });
                api.patch_status(
                    &crd_name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Merge(&patch),
                )
                .await?;
            }
        }
        Ok(())
    }
}

pub fn build_init_job(
    cr_name: &str,
    ns: &str,
    image: &str,
    db_name: &str,
    modules: &[String],
    instance: &OdooInstance,
    init_job: &OdooInitJob,
) -> Job {
    OdooJobBuilder::new(&format!("{cr_name}-"), ns, init_job, instance)
        .active_deadline(3600)
        .containers(vec![{
            let mut odoo_args = vec![
                "-i".to_string(),
                modules.join(","),
                "-d".to_string(),
                db_name.to_string(),
                "--no-http".to_string(),
                "--stop-after-init".to_string(),
            ];
            // Demo data: Odoo >= 19 needs `--with-demo` (demo is off by default),
            // while Odoo <= 18 has no such option and loads demo by default. The
            // operator can't know the image's Odoo version, so the demo=true path
            // probes it at runtime (only here) and adds `--with-demo` solely when
            // the binary is >= 19; on <= 18 the flag is omitted and demo loads by
            // default. The non-demo path stays a plain exec, unchanged.
            let (command, args) = if init_job.spec.demo {
                let mut argv = vec![
                    "maj=$(odoo --version 2>/dev/null | grep -oE '[0-9]+' | head -n1); \
                     flag=''; [ \"${maj:-0}\" -ge 19 ] && flag='--with-demo'; \
                     exec /entrypoint.sh odoo \"$@\" $flag"
                        .to_string(),
                    "sh".to_string(), // $0 for the `sh -c` invocation
                ];
                argv.append(&mut odoo_args);
                (vec!["/bin/sh".to_string(), "-c".to_string()], argv)
            } else {
                odoo_args.push("--without-demo=all".to_string());
                (
                    vec!["/entrypoint.sh".to_string(), "odoo".to_string()],
                    odoo_args,
                )
            };
            Container {
                name: "init".to_string(),
                image: Some(image.to_string()),
                command: Some(command),
                args: Some(args),
                volume_mounts: Some(odoo_volume_mounts()),
                ..Default::default()
            }
        }])
        .build()
}
