//! Regression test for the orphan-role bug (issue #119).
//!
//! When `cleanup_instance` calls `delete_role` and it fails (e.g. the role
//! still owns dependent objects or grants), the operator must NOT remove the
//! `bemade.org/postgres-cleanup` finalizer.  Stripping the finalizer lets the
//! API server delete the OdooInstance while postgres-side state lingers,
//! orphaning the role and blocking same-name re-create.

use kube::api::{Api, DeleteParams};

use super::common::*;
use odoo_operator::crd::odoo_instance::OdooInstance;
use odoo_operator::helpers::odoo_username;

const FINALIZER: &str = "bemade.org/postgres-cleanup";

/// On `delete_role` failure the postgres-cleanup finalizer must remain on
/// the OdooInstance and the object must continue to exist (deletionTimestamp
/// set but not yet gone) so the controller keeps retrying.  Today the
/// operator strips the finalizer regardless and the instance disappears.
#[tokio::test]
async fn finalizer_retained_when_delete_role_fails() -> anyhow::Result<()> {
    let ctx = TestContext::new("test-cleanup-fail").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let instances: Api<OdooInstance> = Api::namespaced(c.clone(), ns);

    // Wait for the operator's first reconcile to add the postgres-cleanup
    // finalizer to the instance.  Without this, a fast delete races the
    // reconciler and kube tears the object down before we can assert
    // anything meaningful.
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = instances.clone();
            async move {
                api.get("test-cleanup-fail")
                    .await
                    .ok()
                    .and_then(|i| i.metadata.finalizers)
                    .is_some_and(|f| f.iter().any(|s| s == FINALIZER))
            }
        })
        .await,
        "postgres-cleanup finalizer never appeared on the instance"
    );

    // Arm the mock so the controller's next cleanup attempt fails.
    let username = odoo_username(ns, "test-cleanup-fail");
    mock_pg().fail_delete_role(
        &username,
        "role still owns dependent grants in cluster (test injection)",
    );

    // Trigger deletion.  In the buggy code path the operator's cleanup
    // handler logs the failure as a warn and returns Ok — the kube-rs
    // finalizer helper then strips the finalizer and the API server
    // removes the object.
    instances
        .delete("test-cleanup-fail", &DeleteParams::default())
        .await
        .expect("failed to issue delete");

    // Give the controller enough time to attempt cleanup at least once.
    // POLL is 500ms; sleeping ~3s leaves room for the watch event +
    // reconcile + mock call.  We deliberately don't wait_for a positive
    // signal here — we are asserting the *absence* of a (bad) transition.
    tokio::time::sleep(TIMEOUT / 10).await;

    // The instance must still exist — deletionTimestamp set, but the
    // finalizer should be holding deletion open.
    let still_there = instances.get_opt("test-cleanup-fail").await?;
    let inst = still_there.expect(
        "OdooInstance was deleted from the API server even though postgres-side \
         cleanup failed — finalizer was incorrectly removed",
    );

    assert!(
        inst.metadata.deletion_timestamp.is_some(),
        "expected deletionTimestamp to be set after delete()"
    );

    let finalizers = inst.metadata.finalizers.unwrap_or_default();
    assert!(
        finalizers.iter().any(|s| s == FINALIZER),
        "postgres-cleanup finalizer was stripped despite cleanup failure \
         (finalizers now: {finalizers:?})"
    );

    // Clear the fault so the test's drop / parallel teardown doesn't leak
    // a stuck instance into other tests.
    mock_pg().clear_delete_role_failure(&username);

    Ok(())
}
