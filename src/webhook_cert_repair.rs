//! Self-healing for a webhook serving certificate that no longer chains to the
//! CA the API server trusts.
//!
//! The webhook's serving certificate is issued by a cert-manager CA whose
//! public half is injected into the `ValidatingWebhookConfiguration`'s
//! `caBundle`. Those two are separate cert-manager `Certificate` resources, and
//! nothing guarantees they stay in step: if the CA rotates at the same moment
//! the leaf is re-issued, the leaf can end up signed by the *outgoing* CA while
//! cainjector publishes the *incoming* one. Every admission request then fails
//! with:
//!
//! ```text
//! x509: certificate signed by unknown authority
//!   (candidate authority certificate "<release>-webhook-ca")
//! ```
//!
//! which blocks all OdooInstance writes cluster-wide. This is not
//! self-correcting: cert-manager considers the leaf perfectly healthy — it is
//! unexpired and matches its spec — so it will not re-issue it until its own
//! renewal falls due, potentially months later. Recovery has meant a human
//! noticing and deleting the leaf secret by hand.
//!
//! The chart gives the CA a much longer life than the leaf so the two cannot
//! renew together, which removes the common trigger. This module removes the
//! need for the human: it periodically compares the CA's Subject Key
//! Identifier with the leaf's Authority Key Identifier and, when they disagree,
//! deletes the leaf secret so cert-manager re-issues it against the current CA.
//! `tls.rs` then picks the new certificate up without a restart.
//!
//! Comparing key identifiers rather than subjects is essential: both CAs carry
//! the same `CN=<release>-webhook-ca`, so subject/issuer comparison sees a
//! matching pair in precisely the broken case.
//!
//! The repair works even while admission is failing, because the validating
//! webhook gates `OdooInstance` resources, not `Secrets`.

use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::{
    api::{Api, DeleteParams},
    Client,
};
use tracing::{debug, info, warn};
use x509_parser::prelude::*;

/// How often to compare the CA and the serving certificate.
///
/// The condition this repairs arises only at certificate renewal, so a slow
/// poll is ample. It is deliberately not tied to the reconcile loop: this is a
/// property of the operator's own webhook plumbing, not of any one instance.
const CHECK_INTERVAL: Duration = Duration::from_secs(300);

/// Subject Key Identifier of a CA certificate, if it carries one.
fn subject_key_id(pem_bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, pem) = parse_x509_pem(pem_bytes).ok()?;
    let (_, cert) = X509Certificate::from_der(&pem.contents).ok()?;
    cert.extensions()
        .iter()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(ski) => Some(ski.0.to_vec()),
            _ => None,
        })
}

/// Authority Key Identifier of a leaf certificate, if it carries one.
fn authority_key_id(pem_bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, pem) = parse_x509_pem(pem_bytes).ok()?;
    let (_, cert) = X509Certificate::from_der(&pem.contents).ok()?;
    cert.extensions()
        .iter()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|k| k.0.to_vec())
            }
            _ => None,
        })
}

/// Whether `leaf_pem` was signed by something other than `ca_pem`.
///
/// Returns `false` whenever the answer is not clearly yes — an unparseable
/// certificate or a missing key identifier yields "assume fine". Deleting a
/// secret is destructive and triggers a re-issue, so the bias is deliberately
/// toward leaving a certificate alone rather than churning one that might be
/// healthy.
pub fn leaf_is_orphaned(ca_pem: &[u8], leaf_pem: &[u8]) -> bool {
    match (subject_key_id(ca_pem), authority_key_id(leaf_pem)) {
        (Some(ski), Some(aki)) => ski != aki,
        _ => false,
    }
}

async fn check_once(
    client: &Client,
    namespace: &str,
    ca_secret: &str,
    cert_secret: &str,
) -> anyhow::Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);

    let ca = secrets.get(ca_secret).await?;
    let leaf = secrets.get(cert_secret).await?;
    let ca_pem = ca
        .data
        .as_ref()
        .and_then(|d| d.get("tls.crt"))
        .map(|b| b.0.clone());
    let leaf_pem = leaf
        .data
        .as_ref()
        .and_then(|d| d.get("tls.crt"))
        .map(|b| b.0.clone());

    let (Some(ca_pem), Some(leaf_pem)) = (ca_pem, leaf_pem) else {
        debug!(%ca_secret, %cert_secret, "webhook secrets missing tls.crt; skipping check");
        return Ok(());
    };

    if !leaf_is_orphaned(&ca_pem, &leaf_pem) {
        debug!("webhook serving certificate chains to the current CA");
        return Ok(());
    }

    warn!(
        %namespace, %cert_secret,
        "webhook serving certificate was signed by a CA that is no longer published in the \
         caBundle; deleting the secret so cert-manager re-issues it"
    );
    secrets
        .delete(cert_secret, &DeleteParams::default())
        .await?;
    info!(
        %cert_secret,
        "deleted stale webhook serving certificate; cert-manager will re-issue and the TLS \
         resolver will pick it up without a restart"
    );
    Ok(())
}

/// Run the repair loop forever.
pub async fn run(client: Client, namespace: String, ca_secret: String, cert_secret: String) {
    info!(
        %namespace, %ca_secret, %cert_secret,
        "starting webhook certificate consistency checker"
    );
    loop {
        if let Err(e) = check_once(&client, &namespace, &ca_secret, &cert_secret).await {
            // Non-fatal: a transient API error must never take down the
            // operator, and the next tick retries.
            warn!(%e, "webhook certificate consistency check failed");
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // caA/leaf are a matching pair; caB shares caA's subject
    // (CN=odoo-operator-webhook-ca) but has a different key — exactly the
    // shape produced when the CA and leaf renew in the same instant.
    const CA_A: &[u8] = include_bytes!("../tests/fixtures/webhook-certs/caA.crt");
    const CA_B: &[u8] = include_bytes!("../tests/fixtures/webhook-certs/caB.crt");
    const LEAF: &[u8] = include_bytes!("../tests/fixtures/webhook-certs/leaf.crt");

    #[test]
    fn matching_pair_is_not_orphaned() {
        assert!(!leaf_is_orphaned(CA_A, LEAF));
    }

    #[test]
    fn leaf_signed_by_a_different_ca_is_orphaned() {
        assert!(leaf_is_orphaned(CA_B, LEAF));
    }

    #[test]
    fn identical_subjects_do_not_mask_the_mismatch() {
        // The whole reason for comparing key identifiers: a subject/issuer
        // comparison would see these as a matching pair.
        let (_, a) = parse_x509_pem(CA_A).unwrap();
        let (_, a) = X509Certificate::from_der(&a.contents).unwrap();
        let (_, b) = parse_x509_pem(CA_B).unwrap();
        let (_, b) = X509Certificate::from_der(&b.contents).unwrap();
        assert_eq!(a.subject().to_string(), b.subject().to_string());
        assert!(leaf_is_orphaned(CA_B, LEAF));
    }

    #[test]
    fn unparseable_input_is_treated_as_healthy() {
        // Bias toward leaving certificates alone: deleting a secret is
        // destructive, so garbage in must not trigger a re-issue.
        assert!(!leaf_is_orphaned(b"not a certificate", LEAF));
        assert!(!leaf_is_orphaned(CA_A, b"not a certificate"));
    }
}
