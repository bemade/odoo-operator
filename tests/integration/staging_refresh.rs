use k8s_openapi::api::batch::v1::Job;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::{Duration, Instant};

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};
use odoo_operator::crd::odoo_staging_refresh_job::OdooStagingRefreshJob;
use odoo_operator::crd::shared::Phase;

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(200);

/// Wait for a specific status field on an OdooStagingRefreshJob CR to be
/// set to a non-empty string, then return its value.  The staging refresh
/// has three sub-Jobs (dbJobName, filestoreJobName, neutralizeJobName) so
/// the generic wait_for_k8s_job_name helper (looks at `jobName`) isn't
/// directly usable.
async fn wait_for_refresh_sub_job(
    client: &kube::Client,
    ns: &str,
    crd_name: &str,
    field: &str,
) -> String {
    let api: Api<OdooStagingRefreshJob> = Api::namespaced(client.clone(), ns);
    let start = Instant::now();
    loop {
        if let Ok(obj) = api.get(crd_name).await {
            let v = serde_json::to_value(&obj).unwrap();
            if let Some(name) = v
                .pointer(&format!("/status/{field}"))
                .and_then(|x| x.as_str())
            {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "refresh CR {crd_name} never populated status.{field}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Happy-path staging refresh: source runs, target gets cloned, neutralize
/// succeeds, target lands in Starting with dbInitialized=true.  Exercises
/// the CloningFromSource phase, the three sub-Job orchestration, and the
/// CompleteRefreshJob transition action.
#[tokio::test]
async fn staging_refresh_happy_path() -> anyhow::Result<()> {
    // Bring source up to Running.  TestContext creates a namespace + a
    // source OdooInstance called "source-inst".
    let ctx = TestContext::new("source-inst").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "source-init").await;

    // Target OdooInstance: same namespace (v1 constraint), init disabled.
    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "target-inst", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["target.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "standard" },
            "init": { "enabled": false },
        }
    }))
    .unwrap();
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances.create(&PostParams::default(), &target).await?;
    assert!(
        wait_for_phase(c, ns, "target-inst", OdooInstancePhase::Uninitialized).await,
        "expected target Uninitialized"
    );

    // Create the refresh CR — this drives target into CloningFromSource.
    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "target-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "target-inst" },
            "source": { "instanceName": "source-inst" },
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;

    assert!(
        wait_for_phase(c, ns, "target-inst", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource after refresh job created"
    );

    // DB + filestore Jobs are spawned in parallel.  Fake both Succeeded.
    let db_job = wait_for_refresh_sub_job(c, ns, "target-refresh", "dbJobName").await;
    let fs_job = wait_for_refresh_sub_job(c, ns, "target-refresh", "filestoreJobName").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    let succeed = |job_name: String| {
        let jobs = jobs.clone();
        async move {
            let patch = json!({ "status": { "succeeded": 1 } });
            jobs.patch_status(
                &job_name,
                &PatchParams::apply("odoo-operator-test"),
                &Patch::Merge(&patch),
            )
            .await
            .expect("patch job status");
        }
    };
    succeed(db_job).await;
    succeed(fs_job).await;

    // Neutralize Job spawns after both succeed.
    let neut_job = wait_for_refresh_sub_job(c, ns, "target-refresh", "neutralizeJobName").await;
    succeed(neut_job).await;

    // Transition: CloningFromSource → Starting, dbInitialized=true.
    assert!(
        wait_for_phase(c, ns, "target-inst", OdooInstancePhase::Starting).await,
        "expected Starting after all refresh sub-jobs succeeded"
    );
    let target_after = instances.get("target-inst").await?;
    assert!(
        target_after
            .status
            .map(|s| s.db_initialized)
            .unwrap_or(false),
        "dbInitialized must be true after successful refresh"
    );

    source_ready.abort();
    Ok(())
}

/// Failure path: DB clone Job fails → refresh aggregate Failed → target
/// transitions to InitFailed.  Kept deliberately simple (doesn't test
/// filestore-only or neutralize-only failures — the aggregate logic treats
/// any sub-job Failed the same).
#[tokio::test]
async fn staging_refresh_db_failure_goes_to_init_failed() -> anyhow::Result<()> {
    let ctx = TestContext::new("src-fail").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "src-fail-init").await;

    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "tgt-fail", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["tgt-fail.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "standard" },
            "init": { "enabled": false },
        }
    }))
    .unwrap();
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances.create(&PostParams::default(), &target).await?;

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "tgt-fail-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "tgt-fail" },
            "source": { "instanceName": "src-fail" },
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;

    assert!(
        wait_for_phase(c, ns, "tgt-fail", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource"
    );

    let db_job = wait_for_refresh_sub_job(c, ns, "tgt-fail-refresh", "dbJobName").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    let patch = json!({ "status": { "failed": 1 } });
    jobs.patch_status(
        &db_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&patch),
    )
    .await?;

    assert!(
        wait_for_phase(c, ns, "tgt-fail", OdooInstancePhase::InitFailed).await,
        "expected InitFailed after DB sub-job failed"
    );

    // dbInitialized remains false — the refresh never finished and never
    // called MarkDbInitialized.
    let tgt = instances.get("tgt-fail").await?;
    assert!(
        !tgt.status.map(|s| s.db_initialized).unwrap_or(true),
        "dbInitialized must remain false on failed refresh"
    );

    // Discourage unused-result warnings from ListParams import.
    let _ = ListParams::default();
    source_ready.abort();
    Ok(())
}

/// Regression: a sub-Job that finishes and is then garbage-collected by
/// `ttlSecondsAfterFinished` (or any other deletion path) must not flip
/// the aggregate refresh into Failed while siblings are still running.
///
/// Production incident: durpro-staging refresh hit InitFailed at 30 min
/// because the DB clone (~14 min) + 15 min TTL caused its batch/v1 Job
/// to disappear while the filestore rsync was still in flight.  The
/// snapshot builder saw 404 on the DB Job, returned `Failed`, and the
/// state machine fired CloningFromSource → InitFailed.
///
/// Fix verified here: the operator records each sub-Job's terminal
/// phase on the parent CR (`status.dbJobPhase` etc.) the first time it
/// observes success; subsequent reconciles read that authoritative
/// record instead of re-fetching the (now-deleted) Job.
#[tokio::test]
async fn staging_refresh_survives_subjob_gc() -> anyhow::Result<()> {
    let ctx = TestContext::new("src-gc").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "src-gc-init").await;

    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "tgt-gc", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["tgt-gc.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "standard" },
            "init": { "enabled": false },
        }
    }))
    .unwrap();
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    instances.create(&PostParams::default(), &target).await?;

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "tgt-gc-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "tgt-gc" },
            "source": { "instanceName": "src-gc" },
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;
    assert!(
        wait_for_phase(c, ns, "tgt-gc", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource"
    );

    // Step 1: DB and filestore Jobs spawn in parallel; succeed only the DB.
    let db_job = wait_for_refresh_sub_job(c, ns, "tgt-gc-refresh", "dbJobName").await;
    let fs_job = wait_for_refresh_sub_job(c, ns, "tgt-gc-refresh", "filestoreJobName").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    jobs.patch_status(
        &db_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    // Step 2: wait for the operator to record the DB sub-Job's terminal
    // phase on the parent CR.  This is the canonical record that survives
    // GC of the underlying Job.
    let start = Instant::now();
    loop {
        let r = refreshes.get("tgt-gc-refresh").await?;
        if matches!(
            r.status.as_ref().and_then(|s| s.db_job_phase.as_ref()),
            Some(Phase::Completed)
        ) {
            break;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "operator never recorded dbJobPhase=Completed"
        );
        tokio::time::sleep(POLL).await;
    }

    // Step 3: simulate TTL-based GC by deleting the (succeeded) DB Job.
    jobs.delete(&db_job, &DeleteParams::default()).await?;

    // Step 4: succeed the filestore Job.  The operator must now consult
    // the recorded dbJobPhase rather than re-fetching the deleted DB Job;
    // otherwise the aggregate would roll up to Failed and the target would
    // transition to InitFailed.
    jobs.patch_status(
        &fs_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    // Step 5: neutralize Job spawns iff db_done && fs_done held true,
    // proving the recorded phase carried the DB clone's success forward.
    let neut_job = wait_for_refresh_sub_job(c, ns, "tgt-gc-refresh", "neutralizeJobName").await;
    jobs.patch_status(
        &neut_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    assert!(
        wait_for_phase(c, ns, "tgt-gc", OdooInstancePhase::Starting).await,
        "expected Starting after refresh succeeded despite DB Job being GC'd"
    );

    source_ready.abort();
    Ok(())
}
