//! Hot-reloading TLS for the validating webhook server.
//!
//! Kubernetes (cert-manager) rotates the webhook serving certificate
//! periodically and writes the new cert/key into the mounted Secret on disk.
//! A process that loads the certificate only once at startup keeps presenting
//! the *old* cert after a rotation. The API server — which by then trusts the
//! *new* CA via the `caBundle` injected into the `ValidatingWebhookConfiguration`
//! by cert-manager-cainjector — rejects the stale cert with
//! `x509: certificate signed by unknown authority`, and the webhook stays
//! broken until the pod is restarted.
//!
//! This module closes that gap with a [`ReloadingCertResolver`]: the current
//! certificate lives in an [`ArcSwap`] consulted on every TLS handshake, and a
//! background poller reloads it from disk whenever the bytes change. Rotations
//! are picked up live, with no restart and no dropped connections.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

/// How often the background task re-reads the cert files looking for a rotation.
/// Rotations happen on the order of months, so a slow poll is plenty; the cost
/// of a poll that finds no change is two small file reads and a hash.
const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// A parsed serving certificate plus a fingerprint of the source bytes. The
/// fingerprint lets the poller skip the swap (and the log line) when the files
/// are unchanged, which is the overwhelmingly common case.
struct LoadedCert {
    certified_key: Arc<CertifiedKey>,
    fingerprint: [u8; 32],
}

/// Read the PEM cert chain + private key from disk and build a [`CertifiedKey`],
/// returning it alongside a SHA-256 fingerprint of the raw file bytes.
fn load_from_disk(cert_path: &str, key_path: &str) -> Result<LoadedCert> {
    let cert_pem =
        std::fs::read(cert_path).with_context(|| format!("reading webhook cert {cert_path}"))?;
    let key_pem =
        std::fs::read(key_path).with_context(|| format!("reading webhook key {key_path}"))?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<std::result::Result<Vec<CertificateDer>, _>>()
        .context("parsing webhook certificate chain")?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {cert_path}");
    }

    let key: PrivateKeyDer = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("parsing webhook private key")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let signing_key = any_supported_type(&key).context("unsupported webhook private key type")?;
    let certified_key = Arc::new(CertifiedKey::new(certs, signing_key));

    let mut hasher = Sha256::new();
    hasher.update(&cert_pem);
    hasher.update(&key_pem);
    let fingerprint = hasher.finalize().into();

    Ok(LoadedCert {
        certified_key,
        fingerprint,
    })
}

/// A rustls server-cert resolver whose certificate can be swapped at runtime.
///
/// Every handshake reads the current cert via a lock-free [`ArcSwap`] load; the
/// background poller replaces it in place when a rotation is detected.
pub struct ReloadingCertResolver {
    current: ArcSwap<CertifiedKey>,
}

impl ReloadingCertResolver {
    fn new(initial: Arc<CertifiedKey>) -> Self {
        Self {
            current: ArcSwap::new(initial),
        }
    }

    fn store(&self, certified_key: Arc<CertifiedKey>) {
        self.current.store(certified_key);
    }
}

// rustls requires `Debug` on `ResolvesServerCert`; a `CertifiedKey` has no
// secret-safe Debug we want to leak, so keep it opaque.
impl std::fmt::Debug for ReloadingCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadingCertResolver")
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for ReloadingCertResolver {
    fn resolve(&self, _client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

/// Load the webhook cert from disk and spawn a background task that reloads it
/// every [`RELOAD_INTERVAL`], returning a resolver wired to the live cert.
///
/// The *initial* load is fatal on failure — the caller should propagate the
/// error so the pod crash-loops rather than serve with no certificate. Later
/// reload failures are non-fatal: they are logged and the last-good cert keeps
/// serving, so a transient read during an atomic Secret swap can't take the
/// webhook down.
pub fn spawn_reloading_resolver(
    cert_path: &str,
    key_path: &str,
) -> Result<Arc<ReloadingCertResolver>> {
    let initial =
        load_from_disk(cert_path, key_path).context("initial webhook certificate load")?;
    let resolver = Arc::new(ReloadingCertResolver::new(initial.certified_key));

    let task_resolver = resolver.clone();
    let cert_path = cert_path.to_string();
    let key_path = key_path.to_string();
    tokio::spawn(async move {
        let mut current_fp = initial.fingerprint;
        let mut interval = tokio::time::interval(RELOAD_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately; skip it since we just loaded.
        interval.tick().await;
        loop {
            interval.tick().await;
            match load_from_disk(&cert_path, &key_path) {
                Ok(loaded) if loaded.fingerprint != current_fp => {
                    task_resolver.store(loaded.certified_key);
                    current_fp = loaded.fingerprint;
                    info!("webhook serving certificate reloaded after rotation");
                }
                Ok(_) => { /* unchanged — the common case */ }
                Err(e) => {
                    warn!(error = %e, "failed to reload webhook certificate; keeping current")
                }
            }
        }
    });

    Ok(resolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Generate a fresh self-signed cert/key PEM pair for `host`.
    fn gen_cert_pem(host: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
        (cert.cert.pem(), cert.signing_key.serialize_pem())
    }

    /// Write cert+key PEM into a temp dir, returning the dir and the two paths.
    fn write_pair(cert_pem: &str, key_pem: &str) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("tls.crt");
        let key_path = dir.path().join("tls.key");
        std::fs::File::create(&cert_path)
            .unwrap()
            .write_all(cert_pem.as_bytes())
            .unwrap();
        std::fs::File::create(&key_path)
            .unwrap()
            .write_all(key_pem.as_bytes())
            .unwrap();
        (
            dir,
            cert_path.to_str().unwrap().to_string(),
            key_path.to_str().unwrap().to_string(),
        )
    }

    #[test]
    fn load_from_disk_parses_valid_cert() {
        let (cert_pem, key_pem) = gen_cert_pem("webhook.example.com");
        let (_dir, cert_path, key_path) = write_pair(&cert_pem, &key_pem);

        let loaded = load_from_disk(&cert_path, &key_path).expect("should parse");
        assert!(!loaded.certified_key.cert.is_empty(), "chain present");
    }

    #[test]
    fn fingerprint_is_stable_for_unchanged_files() {
        let (cert_pem, key_pem) = gen_cert_pem("webhook.example.com");
        let (_dir, cert_path, key_path) = write_pair(&cert_pem, &key_pem);

        let a = load_from_disk(&cert_path, &key_path).unwrap();
        let b = load_from_disk(&cert_path, &key_path).unwrap();
        assert_eq!(
            a.fingerprint, b.fingerprint,
            "same bytes must hash identically so the poller skips the swap"
        );
    }

    #[test]
    fn fingerprint_changes_after_rotation() {
        // Simulate cert-manager rotating the serving cert in place.
        let (cert1, key1) = gen_cert_pem("webhook.example.com");
        let (dir, cert_path, key_path) = write_pair(&cert1, &key1);
        let before = load_from_disk(&cert_path, &key_path).unwrap().fingerprint;

        let (cert2, key2) = gen_cert_pem("webhook.example.com");
        std::fs::write(dir.path().join("tls.crt"), cert2).unwrap();
        std::fs::write(dir.path().join("tls.key"), key2).unwrap();
        let after = load_from_disk(&cert_path, &key_path).unwrap().fingerprint;

        assert_ne!(
            before, after,
            "a rotated cert must produce a different fingerprint so the poller reloads"
        );
    }

    #[test]
    fn resolver_serves_swapped_cert() {
        // The resolver must hand out whatever cert was last stored, proving a
        // rotation is reflected in handshakes without rebuilding the resolver.
        let (cert1, key1) = gen_cert_pem("webhook.example.com");
        let (dir, cert_path, key_path) = write_pair(&cert1, &key1);
        let first = load_from_disk(&cert_path, &key_path).unwrap();
        let resolver = ReloadingCertResolver::new(first.certified_key.clone());

        let initial = resolver.current.load_full();
        assert!(Arc::ptr_eq(&initial, &first.certified_key));

        let (cert2, key2) = gen_cert_pem("webhook.example.com");
        std::fs::write(dir.path().join("tls.crt"), cert2).unwrap();
        std::fs::write(dir.path().join("tls.key"), key2).unwrap();
        let second = load_from_disk(&cert_path, &key_path).unwrap();
        resolver.store(second.certified_key.clone());

        let after = resolver.current.load_full();
        assert!(
            Arc::ptr_eq(&after, &second.certified_key),
            "resolver must serve the cert stored after rotation"
        );
        assert!(!Arc::ptr_eq(&after, &first.certified_key));
    }

    #[test]
    fn load_from_disk_errors_on_missing_file() {
        let err = load_from_disk("/nonexistent/tls.crt", "/nonexistent/tls.key");
        assert!(err.is_err(), "missing cert file must be an error");
    }

    #[test]
    fn load_from_disk_errors_on_empty_cert() {
        let (_cert_pem, key_pem) = gen_cert_pem("webhook.example.com");
        let (_dir, cert_path, key_path) = write_pair("", &key_pem);
        let err = load_from_disk(&cert_path, &key_path);
        assert!(err.is_err(), "empty cert chain must be an error");
    }
}
