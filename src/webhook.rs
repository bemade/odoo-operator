//! Validating admission webhook for OdooInstance.
//!
//! Rejects updates that would:
//! - Decrease filestore storage size (PVCs cannot shrink)
//!
//! StorageClass and database cluster changes are allowed — the operator
//! handles migration automatically.  Changes are rejected during unsafe
//! phases (Restoring, Upgrading, BackingUp, migrating phases, Uninitialized).

use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use tracing::{info, warn};
use warp::Filter;

use crate::crd::odoo_instance::OdooInstance;

/// Start the validating webhook server on the given address.
/// Returns a future that runs the HTTPS server forever.
pub async fn run(addr: std::net::SocketAddr, tls_cert: &str, tls_key: &str) {
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

    info!(%addr, "starting validating webhook server");
    warp::serve(route)
        .tls()
        .cert_path(tls_cert)
        .key_path(tls_key)
        .run(addr)
        .await;
}

/// Validate an OdooInstance admission request.
fn validate(req: AdmissionRequest<OdooInstance>) -> AdmissionResponse {
    // CREATE and DELETE are always allowed.
    if req.old_object.is_none() {
        return AdmissionResponse::from(&req);
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

    AdmissionResponse::from(&req)
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

/// Parse a Kubernetes resource quantity string into bytes.
/// Supports: Ki, Mi, Gi, Ti (binary) and k, M, G, T (decimal).
fn parse_quantity(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty quantity".to_string());
    }

    // Find where the numeric part ends.
    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, suffix) = s.split_at(num_end);

    let num: f64 = num_str
        .parse()
        .map_err(|e| format!("invalid number {num_str:?}: {e}"))?;

    let multiplier: u64 = match suffix {
        "" => 1,
        "Ki" => 1024,
        "Mi" => 1024 * 1024,
        "Gi" => 1024 * 1024 * 1024,
        "Ti" => 1024 * 1024 * 1024 * 1024,
        "k" => 1000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        other => return Err(format!("unknown suffix {other:?}")),
    };

    Ok((num * multiplier as f64) as u64)
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
}
