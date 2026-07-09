use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::controller::gc::garbage_collect_job_crs;
use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
use odoo_operator::crd::odoo_instance::OdooInstance;
use odoo_operator::crd::shared::Phase;

/// Create an OdooBackupJob CR referencing `instance`, then optionally drive its
/// status.phase to a terminal value.
async fn make_backup(api: &Api<OdooBackupJob>, name: &str, instance: &str, phase: Option<Phase>) {
    let cr: OdooBackupJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooBackupJob",
        "metadata": { "name": name },
        "spec": {
            "odooInstanceRef": { "name": instance },
            "destination": {
                "bucket": "b", "objectKey": "k", "endpoint": "http://localhost:9000",
            },
        }
    }))
    .unwrap();
    api.create(&PostParams::default(), &cr)
        .await
        .expect("create backup CR");

    if let Some(p) = phase {
        let phase_str = format!("{p:?}");
        api.patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(json!({ "status": { "phase": phase_str } })),
        )
        .await
        .expect("patch backup CR status");
    }
}

async fn backup_names(api: &Api<OdooBackupJob>) -> Vec<String> {
    api.list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|o| o.metadata.name.unwrap())
        .collect()
}

/// GC keeps the newest `limit` terminal CRs per instance, deletes older
/// terminal ones, and never touches non-terminal CRs or other instances' CRs.
#[tokio::test]
async fn gc_prunes_terminal_job_crs() {
    let tc = TestContext::new_ns().await;
    let (client, ns) = (tc.client.clone(), tc.ns.clone());
    let ctx = test_context_with_limit(client.clone(), 2);
    let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), &ns);

    // 4 terminal CRs for the target instance (limit is 2 → expect 2 deleted).
    for i in 0..4 {
        make_backup(
            &api,
            &format!("gc-done-{i}"),
            "gc-inst",
            Some(Phase::Completed),
        )
        .await;
    }
    // A still-running CR for the same instance — must survive.
    make_backup(&api, "gc-running", "gc-inst", Some(Phase::Running)).await;
    // A CR with no status at all — non-terminal, must survive.
    make_backup(&api, "gc-nostatus", "gc-inst", None).await;
    // A terminal CR for a different instance — out of scope, must survive.
    make_backup(&api, "other-done", "other-inst", Some(Phase::Completed)).await;

    // Minimal instance object — GC only reads its namespace + name.
    let inst: OdooInstance = serde_json::from_value(test_instance_json("gc-inst", &ns, 1)).unwrap();

    garbage_collect_job_crs(&inst, &ctx).await.expect("gc");

    let names = backup_names(&api).await;

    // Exactly 2 terminal CRs remain for gc-inst.
    let terminal_remaining = names.iter().filter(|n| n.starts_with("gc-done-")).count();
    assert_eq!(
        terminal_remaining, 2,
        "expected 2 terminal CRs kept, got {names:?}"
    );

    // Non-terminal and other-instance CRs are untouched.
    assert!(
        names.contains(&"gc-running".to_string()),
        "running CR was deleted"
    );
    assert!(
        names.contains(&"gc-nostatus".to_string()),
        "statusless CR was deleted"
    );
    assert!(
        names.contains(&"other-done".to_string()),
        "other instance CR was deleted"
    );
}

/// A history limit of 0 disables GC entirely.
#[tokio::test]
async fn gc_disabled_when_limit_zero() {
    let tc = TestContext::new_ns().await;
    let (client, ns) = (tc.client.clone(), tc.ns.clone());
    let ctx = test_context_with_limit(client.clone(), 0);
    let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), &ns);

    for i in 0..3 {
        make_backup(
            &api,
            &format!("keep-{i}"),
            "gc-inst",
            Some(Phase::Completed),
        )
        .await;
    }

    let inst: OdooInstance = serde_json::from_value(test_instance_json("gc-inst", &ns, 1)).unwrap();

    garbage_collect_job_crs(&inst, &ctx).await.expect("gc");

    assert_eq!(backup_names(&api).await.len(), 3, "GC ran despite limit 0");
}

/// End-to-end proof that the CRD's selectableFields are declared correctly and
/// the API server filters server-side: the reconcile loop's "active CRs for
/// this instance" query must return exactly the non-terminal CRs owned by the
/// target instance — including a just-created CR with no status yet (phase "").
#[tokio::test]
async fn server_side_selector_filters_by_owner_and_phase() {
    let tc = TestContext::new_ns().await;
    let (client, ns) = (tc.client.clone(), tc.ns.clone());
    let api: Api<OdooBackupJob> = Api::namespaced(client.clone(), &ns);

    make_backup(&api, "a-running", "inst-a", Some(Phase::Running)).await;
    make_backup(&api, "a-done", "inst-a", Some(Phase::Completed)).await;
    make_backup(&api, "a-nostatus", "inst-a", None).await;
    make_backup(&api, "b-running", "inst-b", Some(Phase::Running)).await;

    // Same selector gather() builds for the active-CR list.
    let selector = "spec.odooInstanceRef.name=inst-a,status.phase!=Completed,status.phase!=Failed";
    let mut got: Vec<String> = api
        .list(&ListParams::default().fields(selector))
        .await
        .expect("field-selector list must be accepted (selectableFields declared)")
        .items
        .into_iter()
        .map(|o| o.metadata.name.unwrap())
        .collect();
    got.sort();

    assert_eq!(
        got,
        vec!["a-nostatus".to_string(), "a-running".to_string()],
        "server-side selector should return inst-a's non-terminal CRs only \
         (excl. a-done by phase, b-running by owner)"
    );
}
