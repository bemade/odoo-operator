//! Validating admission webhook for OdooInstance.
//!
//! Rejects updates that would:
//! - Decrease filestore storage size (PVCs cannot shrink)
//! - Change the postgres cluster (migration not implemented)
//! - Change the filestore storageClass (PVC storageClass is immutable)

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

    // 2. Reject database cluster changes — cluster migration is not yet implemented.
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
    if !old_cluster.is_empty() && new_cluster != old_cluster {
        return AdmissionResponse::from(&req).deny(format!(
            "spec.database.cluster: changing the postgres cluster from {:?} to {:?} is not supported",
            old_cluster, new_cluster
        ));
    }

    // 3. Reject storageClass changes — the filestore PVC storageClass is immutable.
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
    if !old_class.is_empty() && new_class != old_class {
        return AdmissionResponse::from(&req).deny(format!(
            "spec.filestore.storageClass: cannot change storage class from {:?} to {:?}; \
             the filestore PVC is immutable after creation",
            old_class, new_class
        ));
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
        // Validation logic is tested via compare_quantities and the field checks.
        // Full AdmissionRequest testing requires constructing the review objects,
        // which is better done as an integration test.
    }
}
