//! End-to-end smoke test for the hot-reloading webhook TLS server.
//!
//! Proves the production serving path actually works: a real TLS handshake
//! completes against `webhook::run`, the cert presented is the one on disk
//! (i.e. the reloading resolver is wired in), and a CREATE `AdmissionReview`
//! posted over that connection comes back `allowed: true`.
//!
//! The per-rotation reload logic itself is unit-tested in `src/tls.rs`; this
//! test covers the warp `run_incoming` + `tokio-rustls` acceptor glue that
//! those unit tests can't reach.

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

/// A client cert verifier that accepts anything but records the leaf cert the
/// server presented, so the test can assert the resolver served the disk cert.
#[derive(Debug)]
struct CapturingVerifier {
    seen_leaf: Mutex<Option<Vec<u8>>>,
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen_leaf.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn write_pair(dir: &std::path::Path, cert_pem: &str, key_pem: &str) -> (String, String) {
    let cert_path = dir.join("tls.crt");
    let key_path = dir.join("tls.key");
    std::fs::File::create(&cert_path)
        .unwrap()
        .write_all(cert_pem.as_bytes())
        .unwrap();
    std::fs::File::create(&key_path)
        .unwrap()
        .write_all(key_pem.as_bytes())
        .unwrap();
    (
        cert_path.to_str().unwrap().to_string(),
        key_path.to_str().unwrap().to_string(),
    )
}

/// A minimal CREATE AdmissionReview (no oldObject) — `validate()` allows it.
fn create_review_body() -> String {
    serde_json::json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "e2e-1",
            "kind": {"group": "bemade.org", "version": "v1alpha1", "kind": "OdooInstance"},
            "resource": {"group": "bemade.org", "version": "v1alpha1", "resource": "odooinstances"},
            "name": "e2e",
            "namespace": "default",
            "operation": "CREATE",
            "userInfo": {"username": "tester"},
            "object": {
                "apiVersion": "bemade.org/v1alpha1",
                "kind": "OdooInstance",
                "metadata": {"name": "e2e", "namespace": "default", "uid": "e2e-uid"},
                "spec": {"adminPassword": "x", "ingress": {"hosts": ["e2e.example.com"]}}
            }
        }
    })
    .to_string()
}

#[tokio::test]
async fn webhook_serves_admission_over_tls_with_disk_cert() {
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["webhook.example.com".to_string()]).unwrap();
    let cert_der = cert.cert.der().to_vec();
    let (cert_path, key_path) = write_pair(
        dir.path(),
        &cert.cert.pem(),
        &cert.signing_key.serialize_pem(),
    );

    // Bind on an ephemeral port and hand the listener to the server so we know
    // the real port without a bind race.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        odoo_operator::webhook::run(listener, &cert_path, &key_path).await;
    });

    // Build a TLS client that trusts nothing but captures the served cert.
    let verifier = Arc::new(CapturingVerifier {
        seen_leaf: Mutex::new(None),
    });
    let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(verifier.clone())
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    // Retry the connect briefly while the server task spins up.
    let body = create_review_body();
    let mut response = String::new();
    let mut connected = false;
    for _ in 0..50 {
        let tcp = match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        };
        let server_name = ServerName::try_from("webhook.example.com").unwrap();
        let mut tls = match connector.connect(server_name, tcp).await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        };
        let req = format!(
            "POST /validate-bemade-org-v1alpha1-odooinstance HTTP/1.1\r\n\
             Host: webhook.example.com\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        tls.write_all(req.as_bytes()).await.unwrap();
        tls.read_to_string(&mut response).await.unwrap();
        connected = true;
        break;
    }

    assert!(
        connected,
        "could not establish a TLS connection to the webhook"
    );
    assert!(
        response.contains("200 OK"),
        "expected HTTP 200, got:\n{response}"
    );
    assert!(
        response.contains("\"allowed\":true"),
        "expected admission allowed, got:\n{response}"
    );

    // The cert the server presented must be exactly the one on disk — proof the
    // reloading resolver (not some other path) is serving the handshake.
    let seen = verifier.seen_leaf.lock().unwrap().clone();
    assert_eq!(
        seen.as_deref(),
        Some(cert_der.as_slice()),
        "server must present the on-disk serving certificate"
    );
}
