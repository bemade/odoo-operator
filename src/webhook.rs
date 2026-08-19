//! Validating admission webhook for OdooInstance.
//!
//! Rejects updates that would:
//! - Decrease filestore storage size (PVCs cannot shrink)
//!
//! StorageClass and database cluster changes are allowed — the operator
//! handles migration automatically.  Changes are rejected during unsafe
//! phases (Restoring, Upgrading, BackingUp, migrating phases, Uninitialized).

use std::sync::Arc;

use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};
use warp::Filter;

use crate::crd::odoo_instance::OdooInstance;
use crate::helpers::parse_quantity;
use crate::tls::spawn_reloading_resolver;

/// Start the validating webhook server on an already-bound TCP listener.
/// Returns a future that runs the HTTPS server forever.
///
/// Taking a pre-bound listener (rather than a `SocketAddr`) lets callers — and
/// tests — observe the actual bound port via [`tokio::net::TcpListener::local_addr`].
///
/// TLS termination is handled here (rather than via warp's built-in
/// `.tls().cert_path()`) so the serving certificate can be **hot-reloaded**
/// when cert-manager rotates it — see [`crate::tls`]. Decrypted connections are
/// fed to the warp filter via [`warp::Server::run_incoming`].
pub async fn run(listener: tokio::net::TcpListener, tls_cert: &str, tls_key: &str) {
    let route = warp::post()
        .and(warp::path("validate-bemade-org-v1alpha1-odooinstance"))
        .and(warp::body::json())
        .map(|review: AdmissionReview<OdooInstance>| {
            let req: AdmissionRequest<OdooInstance> = match review.try_into() {
                Ok(req) => req,
                Err(e) => {
                    warn!(%e, "invalid admission request");
                    let resp = AdmissionResponse::invalid(format!("invalid request: {e}"));
                    return warp::reply::json(&resp.into_review());
                }
            };

            let resp = validate(req);
            warp::reply::json(&resp.into_review())
        });

    // A resolver backed by the cert on disk, kept fresh by a background poller.
    // Failing the initial load is fatal: returning here ends the webhook future
    // in main's `select!`, which exits the process so the pod restarts.
    let resolver = match spawn_reloading_resolver(tls_cert, tls_key) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to load webhook TLS certificate; webhook not started");
            return;
        }
    };

    // Build with the ring provider explicitly so we don't depend on a
    // process-wide default provider being installed elsewhere.
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring provider supports the default protocol versions")
            .with_no_client_auth()
            .with_cert_resolver(resolver);
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let local_addr = listener.local_addr().ok();
    info!(
        ?local_addr,
        "starting validating webhook server (hot-reloading TLS)"
    );

    // Accept TCP connections, perform the TLS handshake off the accept path so a
    // slow/stalled handshake can't block other clients, and stream the decrypted
    // connections into warp. The channel decouples accept from warp's consumer.
    let (tx, rx) = tokio::sync::mpsc::channel(128);
    tokio::spawn(async move {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "tcp accept failed");
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                match acceptor.accept(tcp).await {
                    Ok(stream) => {
                        // A send error just means warp has shut down; drop the conn.
                        let _ = tx.send(Ok::<_, std::io::Error>(stream)).await;
                    }
                    Err(e) => warn!(%peer, error = %e, "tls handshake failed"),
                }
            });
        }
    });

    let incoming = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|conn| (conn, rx))
    });
    warp::serve(route).run_incoming(incoming).await;
}

/// Validate an OdooInstance admission request.
fn validate(req: AdmissionRequest<OdooInstance>) -> AdmissionResponse {
    let mut warnings: Vec<String> = Vec::new();

    // 0. Ephemeral-filestore coherence — the one rule that also applies to
    //    CREATE. `storageSize`/`storageClass` are meaningless on an emptyDir
    //    filestore: deny a request that sets or changes them together with
    //    `emptyDir: true`; tolerate values that were already present before a
    //    flip (typically injected by the operator's own defaulting pass — the
    //    same pass strips them afterwards), but say so. A transition INTO
    //    emptyDir additionally gets a warning spelling out the contract.
    if let Some(new) = req.object.as_ref() {
        if let Some(new_fs) = new.spec.filestore.as_ref() {
            if new_fs.empty_dir {
                let old_fs = req
                    .old_object
                    .as_ref()
                    .and_then(|o| o.spec.filestore.as_ref());
                let new_size = new_fs.storage_size.as_deref();
                let old_size = old_fs.and_then(|f| f.storage_size.as_deref());
                let new_class = new_fs.storage_class.as_deref();
                let old_class = old_fs.and_then(|f| f.storage_class.as_deref());
                if (new_size.is_some() && new_size != old_size)
                    || (new_class.is_some() && new_class != old_class)
                {
                    return AdmissionResponse::from(&req).deny(
                        "spec.filestore: storageSize/storageClass cannot be set together with emptyDir: true",
                    );
                }
                if new_size.is_some() || new_class.is_some() {
                    warnings.push(
                        "spec.filestore: storageSize/storageClass are ignored while emptyDir is true and will be removed by the operator".to_string(),
                    );
                }
                if !old_fs.is_some_and(|f| f.empty_dir) {
                    warnings.push(
                        "spec.filestore.emptyDir: the filestore is now per-pod and ephemeral; an existing filestore PVC is retained but no longer mounted. This declares that attachments live in external storage and sessions in a non-filesystem store — backups of this instance are database-only".to_string(),
                    );
                }
            }
        }
    }

    // CREATE and DELETE are always allowed (beyond rule 0).
    if req.old_object.is_none() {
        return with_warnings(AdmissionResponse::from(&req), warnings);
    }

    let old = match req.old_object {
        Some(ref obj) => obj,
        None => return AdmissionResponse::from(&req),
    };
    let new = match req.object {
        Some(ref obj) => obj,
        None => return AdmissionResponse::from(&req),
    };

    // 1. Reject storage size decreases — PVCs cannot shrink.
    if let (Some(old_fs), Some(new_fs)) = (&old.spec.filestore, &new.spec.filestore) {
        if let (Some(old_size), Some(new_size)) = (&old_fs.storage_size, &new_fs.storage_size) {
            if !old_size.is_empty() && !new_size.is_empty() {
                if let Err(msg) = compare_quantities(old_size, new_size) {
                    return AdmissionResponse::from(&req).deny(msg);
                }
            }
        }
    }

    // 2. Reject database cluster changes during unsafe phases.
    //    Allow rollback: changing back to the previous cluster stored in status.
    let old_cluster = old
        .spec
        .database
        .as_ref()
        .and_then(|d| d.cluster.as_deref())
        .unwrap_or("");
    let new_cluster = new
        .spec
        .database
        .as_ref()
        .and_then(|d| d.cluster.as_deref())
        .unwrap_or("");
    if !old_cluster.is_empty() && !new_cluster.is_empty() && new_cluster != old_cluster {
        use crate::crd::odoo_instance::OdooInstancePhase::*;
        let phase = old.status.as_ref().and_then(|s| s.phase.as_ref());
        let prev_cluster = old
            .status
            .as_ref()
            .and_then(|s| s.migration_previous_cluster.as_deref());
        let is_rollback = prev_cluster.is_some_and(|c| c == new_cluster);
        let blocked = !is_rollback
            && matches!(
                phase,
                Some(
                    Restoring
                        | Upgrading
                        | BackingUp
                        | MigratingFilestore
                        | FinalizingFilestoreMigration
                        | MigratingDatabase
                        | FinalizingDatabaseMigration
                        | Uninitialized,
                )
            );
        if blocked {
            return AdmissionResponse::from(&req).deny(format!(
                "spec.database.cluster: cannot change cluster while instance is in {} phase",
                phase.unwrap()
            ));
        }
    }

    // 3. Reject storageClass changes when the instance is in an unsafe phase.
    //    Allow rollback: changing back to the previous SC stored in status.
    let old_class = old
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.as_deref())
        .unwrap_or("");
    let new_class = new
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.as_deref())
        .unwrap_or("");
    if !old_class.is_empty() && !new_class.is_empty() && old_class != new_class {
        use crate::crd::odoo_instance::OdooInstancePhase::*;
        let phase = old.status.as_ref().and_then(|s| s.phase.as_ref());
        let prev_sc = old
            .status
            .as_ref()
            .and_then(|s| s.migration_previous_storage_class.as_deref());
        let is_rollback = prev_sc.is_some_and(|sc| sc == new_class);
        let blocked = !is_rollback
            && matches!(
                phase,
                Some(
                    Restoring
                        | Upgrading
                        | BackingUp
                        | MigratingFilestore
                        | FinalizingFilestoreMigration
                        | Uninitialized,
                )
            );
        if blocked {
            return AdmissionResponse::from(&req).deny(format!(
                "spec.filestore.storageClass: cannot change storage class while instance is in {} phase",
                phase.unwrap()
            ));
        }
    }

    with_warnings(AdmissionResponse::from(&req), warnings)
}

/// Attach collected admission warnings to an allow response.
fn with_warnings(mut resp: AdmissionResponse, warnings: Vec<String>) -> AdmissionResponse {
    if !warnings.is_empty() {
        resp.warnings = Some(warnings);
    }
    resp
}

/// Compare two Kubernetes quantity strings and reject if new < old.
/// Uses a simplified parser that handles common suffixes (Ki, Mi, Gi, Ti).
fn compare_quantities(old: &str, new: &str) -> Result<(), String> {
    let old_bytes =
        parse_quantity(old).map_err(|e| format!("invalid old quantity {old:?}: {e}"))?;
    let new_bytes =
        parse_quantity(new).map_err(|e| format!("invalid new quantity {new:?}: {e}"))?;

    if new_bytes < old_bytes {
        return Err(format!(
            "spec.filestore.storageSize: cannot decrease storage size from {old} to {new}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::admission::AdmissionRequest;

    fn make_instance_json_full(
        db_name: Option<&str>,
        cluster: Option<&str>,
        storage_class: Option<&str>,
    ) -> serde_json::Value {
        let mut db = serde_json::Map::new();
        if let Some(n) = db_name {
            db.insert("name".into(), serde_json::json!(n));
        }
        if let Some(c) = cluster {
            db.insert("cluster".into(), serde_json::json!(c));
        }
        let mut spec = serde_json::json!({
            "adminPassword": "admin",
            "ingress": { "hosts": ["test.example.com"] }
        });
        if !db.is_empty() {
            spec["database"] = serde_json::Value::Object(db);
        }
        if let Some(sc) = storage_class {
            spec["filestore"] = serde_json::json!({ "storageClass": sc });
        }
        serde_json::json!({
            "apiVersion": "bemade.org/v1alpha1",
            "kind": "OdooInstance",
            "metadata": { "name": "test", "namespace": "default", "uid": "test-uid" },
            "spec": spec
        })
    }

    fn make_instance_json(db_name: Option<&str>, cluster: Option<&str>) -> serde_json::Value {
        make_instance_json_full(db_name, cluster, None)
    }

    fn make_update_request(
        old_db_name: Option<&str>,
        old_cluster: Option<&str>,
        new_db_name: Option<&str>,
        new_cluster: Option<&str>,
    ) -> AdmissionRequest<OdooInstance> {
        let review: serde_json::Value = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "req-1",
                "kind": { "group": "bemade.org", "version": "v1alpha1", "kind": "OdooInstance" },
                "resource": { "group": "bemade.org", "version": "v1alpha1", "resource": "odooinstances" },
                "name": "test",
                "namespace": "default",
                "operation": "UPDATE",
                "userInfo": { "username": "test" },
                "object": make_instance_json(new_db_name, new_cluster),
                "oldObject": make_instance_json(old_db_name, old_cluster),
                "dryRun": false,
            }
        });
        let ar: kube::core::admission::AdmissionReview<OdooInstance> =
            serde_json::from_value(review).expect("valid AdmissionReview");
        ar.try_into().expect("valid AdmissionRequest")
    }

    fn make_sc_change_request(
        old_class: &str,
        new_class: &str,
        old_phase: Option<&str>,
    ) -> AdmissionRequest<OdooInstance> {
        make_sc_change_request_with_prev(old_class, new_class, old_phase, None)
    }

    fn make_sc_change_request_with_prev(
        old_class: &str,
        new_class: &str,
        old_phase: Option<&str>,
        prev_sc: Option<&str>,
    ) -> AdmissionRequest<OdooInstance> {
        let mut old_obj = make_instance_json_full(None, None, Some(old_class));
        if let Some(phase) = old_phase {
            let mut status = serde_json::json!({"phase": phase});
            if let Some(sc) = prev_sc {
                status["migrationPreviousStorageClass"] = serde_json::json!(sc);
            }
            old_obj["status"] = status;
        }
        let review: serde_json::Value = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "req-sc",
                "kind": { "group": "bemade.org", "version": "v1alpha1", "kind": "OdooInstance" },
                "resource": { "group": "bemade.org", "version": "v1alpha1", "resource": "odooinstances" },
                "name": "test",
                "namespace": "default",
                "operation": "UPDATE",
                "userInfo": { "username": "test" },
                "object": make_instance_json_full(None, None, Some(new_class)),
                "oldObject": old_obj,
                "dryRun": false,
            }
        });
        let ar: kube::core::admission::AdmissionReview<OdooInstance> =
            serde_json::from_value(review).expect("valid AdmissionReview");
        ar.try_into().expect("valid AdmissionRequest")
    }

    fn make_cluster_change_request(
        old_cluster: &str,
        new_cluster: &str,
        old_phase: Option<&str>,
    ) -> AdmissionRequest<OdooInstance> {
        let mut old_obj = make_instance_json(None, Some(old_cluster));
        if let Some(phase) = old_phase {
            old_obj["status"] = serde_json::json!({"phase": phase});
        }
        let review: serde_json::Value = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "req-cluster",
                "kind": { "group": "bemade.org", "version": "v1alpha1", "kind": "OdooInstance" },
                "resource": { "group": "bemade.org", "version": "v1alpha1", "resource": "odooinstances" },
                "name": "test",
                "namespace": "default",
                "operation": "UPDATE",
                "userInfo": { "username": "test" },
                "object": make_instance_json(None, Some(new_cluster)),
                "oldObject": old_obj,
                "dryRun": false,
            }
        });
        let ar: kube::core::admission::AdmissionReview<OdooInstance> =
            serde_json::from_value(review).expect("valid AdmissionReview");
        ar.try_into().expect("valid AdmissionRequest")
    }

    #[test]
    fn test_parse_quantity() {
        assert_eq!(parse_quantity("2Gi").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_quantity("10Gi").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_quantity("500Mi").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_quantity("1Ti").unwrap(), 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_quantity("100").unwrap(), 100);
    }

    #[test]
    fn test_compare_quantities_allows_increase() {
        assert!(compare_quantities("2Gi", "10Gi").is_ok());
        assert!(compare_quantities("2Gi", "2Gi").is_ok());
    }

    #[test]
    fn test_compare_quantities_rejects_decrease() {
        assert!(compare_quantities("10Gi", "2Gi").is_err());
        assert!(compare_quantities("1Gi", "500Mi").is_err());
    }

    #[test]
    fn test_validate_allows_normal_update() {
        let req = make_update_request(Some("mydb"), None, Some("mydb"), None);
        let resp = validate(req);
        assert!(resp.allowed);
    }

    #[test]
    fn test_validate_allows_cluster_change_when_running() {
        let req = make_update_request(None, Some("pg-cluster-a"), None, Some("pg-cluster-b"));
        let resp = validate(req);
        assert!(
            resp.allowed,
            "cluster change should be allowed (no phase = safe state)"
        );
    }

    #[test]
    fn test_validate_rejects_cluster_change_when_restoring() {
        let req = make_cluster_change_request("pg-a", "pg-b", Some("Restoring"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "cluster change should be rejected when Restoring"
        );
    }

    #[test]
    fn test_validate_rejects_cluster_change_when_migrating_db() {
        let req = make_cluster_change_request("pg-a", "pg-b", Some("MigratingDatabase"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "cluster change should be rejected when already migrating"
        );
    }

    #[test]
    fn test_validate_allows_storage_class_change_when_running() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("Running"));
        let resp = validate(req);
        assert!(
            resp.allowed,
            "storageClass change should be allowed when Running"
        );
    }

    #[test]
    fn test_validate_allows_storage_class_change_when_stopped() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("Stopped"));
        let resp = validate(req);
        assert!(
            resp.allowed,
            "storageClass change should be allowed when Stopped"
        );
    }

    #[test]
    fn test_validate_rejects_storage_class_change_when_restoring() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("Restoring"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "storageClass change should be rejected when Restoring"
        );
    }

    #[test]
    fn test_validate_rejects_storage_class_change_when_upgrading() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("Upgrading"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "storageClass change should be rejected when Upgrading"
        );
    }

    #[test]
    fn test_validate_rejects_storage_class_change_when_backing_up() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("BackingUp"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "storageClass change should be rejected when BackingUp"
        );
    }

    #[test]
    fn test_validate_rejects_storage_class_change_when_migrating() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("MigratingFilestore"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "storageClass change should be rejected when already migrating"
        );
    }

    #[test]
    fn test_validate_rejects_storage_class_change_when_uninitialized() {
        let req = make_sc_change_request("cephfs", "juicefs", Some("Uninitialized"));
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "storageClass change should be rejected when Uninitialized"
        );
    }

    #[test]
    fn test_validate_allows_rollback_during_migration() {
        // Reverting to the previous SC (stored in status) should be allowed
        // even during MigratingFilestore.
        let req = make_sc_change_request_with_prev(
            "juicefs",
            "cephfs",
            Some("MigratingFilestore"),
            Some("cephfs"),
        );
        let resp = validate(req);
        assert!(
            resp.allowed,
            "rollback to previous storageClass should be allowed during migration"
        );
    }

    #[test]
    fn test_validate_rejects_non_rollback_during_migration() {
        // Changing to a DIFFERENT SC (not the previous one) during migration
        // should still be rejected.
        let req = make_sc_change_request_with_prev(
            "juicefs",
            "longhorn",
            Some("MigratingFilestore"),
            Some("cephfs"),
        );
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "changing to a third storageClass during migration should be rejected"
        );
    }

    // ── emptyDir filestore coherence ────────────────────────────────────

    fn make_instance_json_fs(filestore: Option<serde_json::Value>) -> serde_json::Value {
        let mut obj = make_instance_json_full(None, None, None);
        if let Some(fs) = filestore {
            obj["spec"]["filestore"] = fs;
        }
        obj
    }

    fn make_fs_request(
        old_fs: Option<serde_json::Value>,
        new_fs: Option<serde_json::Value>,
        create: bool,
    ) -> AdmissionRequest<OdooInstance> {
        let mut request = serde_json::json!({
            "uid": "req-fs",
            "kind": { "group": "bemade.org", "version": "v1alpha1", "kind": "OdooInstance" },
            "resource": { "group": "bemade.org", "version": "v1alpha1", "resource": "odooinstances" },
            "name": "test",
            "namespace": "default",
            "operation": if create { "CREATE" } else { "UPDATE" },
            "userInfo": { "username": "test" },
            "object": make_instance_json_fs(new_fs),
            "dryRun": false,
        });
        if !create {
            request["oldObject"] = make_instance_json_fs(old_fs);
        }
        let review: serde_json::Value = serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": request,
        });
        let ar: kube::core::admission::AdmissionReview<OdooInstance> =
            serde_json::from_value(review).expect("valid AdmissionReview");
        ar.try_into().expect("valid AdmissionRequest")
    }

    #[test]
    fn test_validate_rejects_empty_dir_with_storage_on_create() {
        let req = make_fs_request(
            None,
            Some(serde_json::json!({ "emptyDir": true, "storageSize": "2Gi" })),
            true,
        );
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "creating with emptyDir + storageSize should be rejected"
        );
    }

    #[test]
    fn test_validate_allows_empty_dir_create_with_contract_warning() {
        let req = make_fs_request(None, Some(serde_json::json!({ "emptyDir": true })), true);
        let resp = validate(req);
        assert!(resp.allowed, "clean emptyDir create should be allowed");
        assert!(
            resp.warnings.as_ref().is_some_and(|w| w.len() == 1),
            "entering emptyDir should carry exactly the contract warning"
        );
    }

    #[test]
    fn test_validate_allows_flip_with_leftover_defaults_and_warns() {
        // Old spec carries operator-injected size/class; the user flips
        // emptyDir on without touching them. Must be allowed (the defaults
        // pass strips them) with both warnings attached.
        let old = serde_json::json!({ "storageSize": "2Gi", "storageClass": "standard" });
        let new = serde_json::json!({
            "emptyDir": true, "storageSize": "2Gi", "storageClass": "standard"
        });
        let req = make_fs_request(Some(old), Some(new), false);
        let resp = validate(req);
        assert!(
            resp.allowed,
            "flip to emptyDir with unchanged leftover size/class should be allowed"
        );
        assert!(
            resp.warnings.as_ref().is_some_and(|w| w.len() == 2),
            "leftover-values warning + contract warning expected"
        );
    }

    #[test]
    fn test_validate_rejects_storage_change_while_ephemeral() {
        let old = serde_json::json!({ "emptyDir": true });
        let new = serde_json::json!({ "emptyDir": true, "storageClass": "standard" });
        let req = make_fs_request(Some(old), Some(new), false);
        let resp = validate(req);
        assert!(
            !resp.allowed,
            "introducing storageClass while emptyDir should be rejected"
        );
    }

    #[test]
    fn test_validate_steady_ephemeral_update_is_quiet() {
        let old = serde_json::json!({ "emptyDir": true });
        let new = serde_json::json!({ "emptyDir": true });
        let req = make_fs_request(Some(old), Some(new), false);
        let resp = validate(req);
        assert!(resp.allowed);
        assert!(
            resp.warnings.is_none(),
            "an already-ephemeral instance should not warn on every update"
        );
    }
}
