use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
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
            "filestoreMethod": "copy",
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

    // Copy path: filestore_phase should be Running before the Job succeeds
    // (set when the Job was created), and walk to Completed once the Job's
    // terminal phase is recorded and the settle block mirrors it forward.
    wait_for_filestore_phase(c, ns, "target-refresh", Phase::Running).await;
    succeed(fs_job).await;
    wait_for_filestore_phase(c, ns, "target-refresh", Phase::Completed).await;

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
            "filestoreMethod": "copy",
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
            "filestoreMethod": "copy",
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

/// Wait until `status.filestorePhase` on the refresh CR matches `expected`.
async fn wait_for_filestore_phase(
    client: &kube::Client,
    ns: &str,
    crd_name: &str,
    expected: Phase,
) {
    let api: Api<OdooStagingRefreshJob> = Api::namespaced(client.clone(), ns);
    let start = Instant::now();
    loop {
        if let Ok(obj) = api.get(crd_name).await {
            if obj.status.as_ref().and_then(|s| s.filestore_phase.as_ref()) == Some(&expected) {
                return;
            }
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "refresh CR {crd_name} never reached filestorePhase={expected:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Patch a PVC's status to `Bound` so the operator's snapshot-path settle
/// block can observe it.  envtest has no PV controller, so PVCs never bind
/// on their own.  Also strips the `kubernetes.io/pvc-protection` finalizer
/// — envtest's API server adds it automatically but has no pvc-protection-
/// controller to strip it after pods detach, so a test PVC would otherwise
/// linger forever in `Terminating` after `delete()`.
async fn fake_pvc_bound(client: &kube::Client, ns: &str, name: &str) {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let start = Instant::now();
    loop {
        if pvcs.get_opt(name).await.ok().flatten().is_some() {
            break;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "PVC {name} never appeared so it could be marked Bound"
        );
        tokio::time::sleep(POLL).await;
    }
    let mut current = pvcs.get(name).await.expect("get PVC");
    if current.metadata.finalizers.is_some() {
        current.metadata.finalizers = Some(vec![]);
        pvcs.replace(name, &PostParams::default(), &current)
            .await
            .expect("strip pvc-protection finalizer");
    }
    let patch = json!({ "status": { "phase": "Bound" } });
    pvcs.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .expect("patch PVC status");
}

/// Snapshot path: when source and target share a StorageClass and
/// `filestoreMethod: snapshot` is requested, the operator must NOT spawn a
/// filestore Job.  Instead, it deletes the staging PVC, recreates it (which
/// would clone from the source PVC in a real cluster), and waits for the
/// new PVC to bind.  The settle block then patches `filestore_phase: Completed`
/// — which is the gate that lets neutralize spawn.  Regression for the
/// "fs_done never trips" stall in the snapshot path.
#[tokio::test]
async fn staging_refresh_snapshot_path_completes_via_pvc_bound() -> anyhow::Result<()> {
    let ctx = TestContext::new("src-snap").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "src-snap-init").await;

    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "tgt-snap", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["tgt-snap.example.com"],
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
        wait_for_phase(c, ns, "tgt-snap", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    // Strip pvc-protection on the existing staging PVC so the operator's
    // snapshot-path delete can actually complete in envtest.
    fake_pvc_bound(c, ns, "tgt-snap-filestore-pvc").await;

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "tgt-snap-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "tgt-snap" },
            "source": { "instanceName": "src-snap" },
            "filestoreMethod": "snapshot",
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;

    assert!(
        wait_for_phase(c, ns, "tgt-snap", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource"
    );

    // DB Job runs as usual on the snapshot path.
    let db_job = wait_for_refresh_sub_job(c, ns, "tgt-snap-refresh", "dbJobName").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    jobs.patch_status(
        &db_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    // filestorePhase should reach Running once the snapshot kickoff patches
    // status (PVC delete + ensure_filestore_pvc complete).
    wait_for_filestore_phase(c, ns, "tgt-snap-refresh", Phase::Running).await;

    // No filestore Job exists on the snapshot path.
    let after = refreshes.get("tgt-snap-refresh").await?;
    assert!(
        after
            .status
            .as_ref()
            .and_then(|s| s.filestore_job_name.as_deref())
            .is_none(),
        "snapshot path must not create a filestore Job"
    );

    // Fake the recreated staging PVC into Bound; the settle block on the
    // next reconcile flips filestorePhase to Completed.
    fake_pvc_bound(c, ns, "tgt-snap-filestore-pvc").await;
    wait_for_filestore_phase(c, ns, "tgt-snap-refresh", Phase::Completed).await;

    // Neutralize spawns once both DB and filestore are Completed.
    let neut_job = wait_for_refresh_sub_job(c, ns, "tgt-snap-refresh", "neutralizeJobName").await;
    jobs.patch_status(
        &neut_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    assert!(
        wait_for_phase(c, ns, "tgt-snap", OdooInstancePhase::Starting).await,
        "expected Starting after snapshot-path refresh succeeded"
    );

    source_ready.abort();
    Ok(())
}

// Note: snapshot-path delete idempotency (operator's `delete()` returning 404
// when the PVC is already gone) is exercised across multiple reconcile ticks
// of `staging_refresh_snapshot_path_completes_via_pvc_bound` — the operator
// re-enters the snapshot branch on every tick until `filestore_phase` is
// Running, so the second tick already hits the 404 path with no error.
// A standalone test was attempted but is fundamentally racy with the
// operator's own ensure_filestore_pvc reconcile recreating the PVC under
// the test's feet; not worth the flakiness for one swallowed error.

/// Auto mode falls back to Copy when source and target StorageClasses
/// differ — CSI volume cloning requires both PVCs to use the same SC, so
/// snapshot is not viable.  Verifies the fallback creates a filestore Job
/// (Copy path) instead of going through the snapshot branch.
#[tokio::test]
async fn staging_refresh_auto_falls_back_to_copy_on_sc_mismatch() -> anyhow::Result<()> {
    // Use the standard test source (storageClass=standard) so
    // fast_track_to_running works as in the other tests.
    let ctx = TestContext::new("src-mix").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "src-mix-init").await;
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);

    // Target with a different storageClass.
    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "tgt-mix", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["tgt-mix.example.com"],
                "issuer": "letsencrypt",
                "class": "nginx",
            },
            "filestore": { "storageSize": "1Gi", "storageClass": "fast-ssd" },
            "init": { "enabled": false },
        }
    }))
    .unwrap();
    instances.create(&PostParams::default(), &target).await?;

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "tgt-mix-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "tgt-mix" },
            "source": { "instanceName": "src-mix" },
            // omit filestoreMethod → Auto (default)
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;

    assert!(
        wait_for_phase(c, ns, "tgt-mix", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource"
    );

    // Auto + SC mismatch ⇒ Copy path ⇒ filestoreJobName is set.
    let _fs_job = wait_for_refresh_sub_job(c, ns, "tgt-mix-refresh", "filestoreJobName").await;
    wait_for_filestore_phase(c, ns, "tgt-mix-refresh", Phase::Running).await;

    let _ = ListParams::default();
    source_ready.abort();
    Ok(())
}

/// Race regression: on a real cluster, foreground-deleting a PVC returns
/// before the object actually disappears — pvc-protection and CSI
/// finalizers keep it `Bound` with a `deletionTimestamp` until kubelet
/// finishes unmounting consumer pods.  If the snapshot kickoff patches
/// `filestorePhase: Running` while the OLD PVC still lingers `Bound`, the
/// settle block on the next reconcile will see `phase: Bound`, prematurely
/// flip `filestorePhase: Completed`, and let neutralize race against a
/// PVC that's about to vanish.  Reproduced here by adding a custom
/// finalizer to the staging PVC so it sticks around `Bound` after delete.
#[tokio::test]
async fn staging_refresh_snapshot_does_not_complete_while_old_pvc_terminates() -> anyhow::Result<()>
{
    let ctx = TestContext::new("src-snap3").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let source_ready = fast_track_to_running(&ctx, "src-snap3-init").await;

    let target: OdooInstance = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "tgt-snap3", "namespace": ns },
        "spec": {
            "replicas": 1,
            "cron": { "replicas": 1 },
            "adminPassword": "admin",
            "image": "odoo:18.0",
            "ingress": {
                "hosts": ["tgt-snap3.example.com"],
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
        wait_for_phase(c, ns, "tgt-snap3", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );

    // Wait for the operator to provision the staging PVC, then fake it
    // Bound and pin a custom finalizer so a subsequent delete() leaves
    // it lingering with deletionTimestamp set — the exact production
    // condition we're guarding against.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(c.clone(), ns);
    let pvc_name = "tgt-snap3-filestore-pvc";
    let start = Instant::now();
    while pvcs.get_opt(pvc_name).await?.is_none() {
        assert!(
            start.elapsed() < TIMEOUT,
            "staging PVC {pvc_name} never appeared"
        );
        tokio::time::sleep(POLL).await;
    }
    fake_pvc_bound(c, ns, pvc_name).await;
    let finalizer_patch = json!({
        "metadata": { "finalizers": ["odoo-operator-test/hold"] }
    });
    pvcs.patch(
        pvc_name,
        &PatchParams::default(),
        &Patch::Merge(&finalizer_patch),
    )
    .await?;

    let refreshes: Api<OdooStagingRefreshJob> = Api::namespaced(c.clone(), ns);
    let refresh: OdooStagingRefreshJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooStagingRefreshJob",
        "metadata": { "name": "tgt-snap3-refresh", "namespace": ns },
        "spec": {
            "odooInstanceRef": { "name": "tgt-snap3" },
            "source": { "instanceName": "src-snap3" },
            "filestoreMethod": "snapshot",
        }
    }))
    .unwrap();
    refreshes.create(&PostParams::default(), &refresh).await?;
    assert!(
        wait_for_phase(c, ns, "tgt-snap3", OdooInstancePhase::CloningFromSource).await,
        "expected CloningFromSource"
    );

    // Succeed the DB Job so the refresh's only outstanding work is the
    // filestore step — keeps the assertion focused.
    let db_job = wait_for_refresh_sub_job(c, ns, "tgt-snap3-refresh", "dbJobName").await;
    let jobs: Api<Job> = Api::namespaced(c.clone(), ns);
    jobs.patch_status(
        &db_job,
        &PatchParams::apply("odoo-operator-test"),
        &Patch::Merge(&json!({ "status": { "succeeded": 1 } })),
    )
    .await?;

    // Critical assertion: while the old PVC has deletionTimestamp set
    // and is still phase=Bound, filestorePhase MUST NOT reach Completed.
    // Poll for several reconcile cycles to give the operator chances to
    // misbehave.
    let race_window = Duration::from_secs(5);
    let race_start = Instant::now();
    while race_start.elapsed() < race_window {
        let r = refreshes.get("tgt-snap3-refresh").await?;
        let fs_phase = r
            .status
            .as_ref()
            .and_then(|s| s.filestore_phase.as_ref())
            .cloned();
        assert_ne!(
            fs_phase,
            Some(Phase::Completed),
            "filestorePhase flipped to Completed while old PVC was still terminating \
             (deletionTimestamp set, phase=Bound) — premature completion race"
        );
        // The PVC must still be present with a deletionTimestamp — proves
        // the test setup is exercising the race window we care about.
        let live = pvcs.get(pvc_name).await?;
        assert!(
            live.metadata.deletion_timestamp.is_some()
                || live.status.as_ref().and_then(|s| s.phase.as_deref()) != Some("Bound"),
            "test invariant: PVC should still be Bound+terminating during the race window"
        );
        tokio::time::sleep(POLL).await;
    }

    // Recovery (PVC actually disappears, fresh one binds, filestorePhase
    // → Completed) is covered by `staging_refresh_snapshot_path_completes_
    // via_pvc_bound`.  envtest's finalizer-driven deletion path is flaky
    // enough that simulating it here brings little extra coverage.

    source_ready.abort();
    Ok(())
}
