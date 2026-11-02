//! The Odoo master password can come from a Secret instead of the CR (#177).

use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, PostParams};
use serde_json::json;

use super::common::*;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

/// `test_instance_json` with the spec's password fields replaced by `fields`.
fn instance_with(name: &str, ns: &str, fields: serde_json::Value) -> OdooInstance {
    let mut v = test_instance_json(name, ns, 1);
    let spec = v["spec"].as_object_mut().unwrap();
    spec.remove("adminPassword");
    for (k, val) in fields.as_object().unwrap() {
        spec.insert(k.clone(), val.clone());
    }
    serde_json::from_value(v).unwrap()
}

async fn create_secret(c: &kube::Client, ns: &str, name: &str, key: &str, value: &str) {
    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    let s: Secret = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": { "name": name },
        "stringData": { key: value },
    }))
    .unwrap();
    secrets.create(&PostParams::default(), &s).await.unwrap();
}

/// The rendered odoo.conf, or None while the conf Secret does not exist.
async fn rendered_conf(c: &kube::Client, ns: &str, name: &str) -> Option<String> {
    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    let s = secrets
        .get_opt(&format!("{name}-odoo-conf"))
        .await
        .unwrap()?;
    let data = s.data.unwrap_or_default();
    Some(String::from_utf8(data.get("odoo.conf")?.0.clone()).unwrap())
}

async fn wait_for_conf_containing(c: &kube::Client, ns: &str, name: &str, needle: &str) -> bool {
    wait_for(TIMEOUT, POLL, || {
        let (c, ns, name, needle) = (
            c.clone(),
            ns.to_string(),
            name.to_string(),
            needle.to_string(),
        );
        async move {
            rendered_conf(&c, &ns, &name)
                .await
                .is_some_and(|conf| conf.contains(&needle))
        }
    })
    .await
}

/// An instance whose master password lives in a Secret reconciles, and the
/// OdooInstance object — everything a role with only odooinstances
/// get/list/watch can see — carries no trace of the password.
#[tokio::test]
async fn secret_ref_instance_reconciles_with_no_plaintext_in_the_cr() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    create_secret(c, ns, "master", "pw", "ref-master-pw-1").await;
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = instance_with(
        "pw-ref",
        ns,
        json!({ "adminPasswordSecretRef": { "name": "master", "key": "pw" } }),
    );
    api.create(&PostParams::default(), &inst).await.unwrap();

    assert!(
        wait_for_phase(c, ns, "pw-ref", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    assert!(
        wait_for_conf_containing(c, ns, "pw-ref", "admin_passwd = ref-master-pw-1\n").await,
        "odoo.conf never rendered the password from the Secret"
    );
    let raw = serde_json::to_string(&api.get("pw-ref").await.unwrap()).unwrap();
    assert!(!raw.contains("ref-master-pw-1"), "{raw}");
    assert!(!raw.contains("adminPassword\""), "{raw}");

    // A change to the Secret reaches odoo.conf by itself.
    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    secrets
        .patch(
            "master",
            &kube::api::PatchParams::default(),
            &kube::api::Patch::Merge(json!({ "stringData": { "pw": "ref-master-pw-2" } })),
        )
        .await
        .unwrap();
    // No touch: the operator watches referenced Secrets.
    assert!(
        wait_for_conf_containing(c, ns, "pw-ref", "admin_passwd = ref-master-pw-2\n").await,
        "a rotated Secret value never reached odoo.conf"
    );
}

/// A reference to a Secret that does not exist renders nothing — and in
/// particular never falls back to generating a password, which would silently
/// change the master password. Once the Secret appears, the conf follows.
#[tokio::test]
async fn missing_referenced_secret_blocks_rendering_until_it_exists() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = instance_with(
        "pw-late",
        ns,
        json!({ "adminPasswordSecretRef": { "name": "late", "key": "pw" } }),
    );
    api.create(&PostParams::default(), &inst).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(rendered_conf(c, ns, "pw-late").await.is_none());
    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    assert!(secrets
        .get_opt("pw-late-admin-password")
        .await
        .unwrap()
        .is_none());
    let spec_ref = api
        .get("pw-late")
        .await
        .unwrap()
        .spec
        .admin_password_secret_ref;
    assert_eq!(spec_ref.map(|r| r.name).as_deref(), Some("late"));

    create_secret(c, ns, "late", "pw", "late-master-pw").await;
    assert!(
        wait_for_conf_containing(c, ns, "pw-late", "admin_passwd = late-master-pw\n").await,
        "conf not rendered once the referenced Secret existed"
    );
}

/// With no password given, the operator generates one into an owned Secret
/// and points the spec at it, instead of writing the value into the CR (and
/// instead of leaving Odoo on its built-in default master password `admin`).
#[tokio::test]
async fn generated_password_goes_to_an_owned_secret_not_the_spec() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    api.create(
        &PostParams::default(),
        &instance_with("pw-gen", ns, json!({})),
    )
    .await
    .unwrap();

    assert!(
        wait_for(TIMEOUT, POLL, || {
            let api = api.clone();
            async move {
                api.get("pw-gen")
                    .await
                    .unwrap()
                    .spec
                    .admin_password_secret_ref
                    .is_some_and(|r| r.name == "pw-gen-admin-password" && r.key == "password")
            }
        })
        .await,
        "spec.adminPasswordSecretRef never pointed at the generated Secret"
    );

    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    let gen = secrets.get("pw-gen-admin-password").await.unwrap();
    let uid = api.get("pw-gen").await.unwrap().metadata.uid.unwrap();
    assert!(
        gen.metadata
            .owner_references
            .unwrap_or_default()
            .iter()
            .any(|o| o.uid == uid && o.controller == Some(true)),
        "generated Secret must be owned by the instance"
    );
    let pw = String::from_utf8(gen.data.unwrap()["password"].0.clone()).unwrap();
    assert!(pw.len() >= 32, "generated password too short: {}", pw.len());

    assert!(
        wait_for_conf_containing(c, ns, "pw-gen", &format!("admin_passwd = {pw}\n")).await,
        "odoo.conf does not carry the generated password"
    );
    let raw = serde_json::to_string(&api.get("pw-gen").await.unwrap()).unwrap();
    assert!(!raw.contains(&pw), "generated password leaked into the CR");
}

/// Existing instances with the literal field keep reconciling exactly as
/// before: the literal is rendered, no ref is written, nothing is generated.
#[tokio::test]
async fn literal_admin_password_instance_is_unchanged() {
    let ctx = TestContext::new("pw-lit").await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    assert!(
        wait_for_phase(c, ns, "pw-lit", OdooInstancePhase::Uninitialized).await,
        "expected Uninitialized"
    );
    assert!(wait_for_conf_containing(c, ns, "pw-lit", "admin_passwd = admin\n").await);
    touch_instance(c, ns, "pw-lit").await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let spec = api.get("pw-lit").await.unwrap().spec;
    assert_eq!(spec.admin_password, "admin");
    assert!(spec.admin_password_secret_ref.is_none());
    let secrets: Api<Secret> = Api::namespaced(c.clone(), ns);
    assert!(secrets
        .get_opt("pw-lit-admin-password")
        .await
        .unwrap()
        .is_none());
}

/// The API server itself rejects a spec that sets both a literal password and
/// a Secret reference; an empty literal next to a ref (a GitOps manifest that
/// still carries `adminPassword: ""`) is not a conflict.
#[tokio::test]
async fn cel_rejects_literal_and_secret_ref_together() {
    let ctx = TestContext::new_ns().await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);

    let both = instance_with(
        "pw-both",
        ns,
        json!({ "adminPassword": "x", "adminPasswordSecretRef": { "name": "s", "key": "k" } }),
    );
    match api.create(&PostParams::default(), &both).await {
        Err(kube::Error::Api(e)) => {
            assert_eq!(e.code, 422, "{e:?}");
            assert!(e.message.contains("mutually exclusive"), "{}", e.message);
        }
        other => panic!("both fields set must be rejected, got {other:?}"),
    }

    // Raw JSON: serde would skip the empty string on the way out.
    let mut v = serde_json::to_value(instance_with(
        "pw-empty",
        ns,
        json!({ "adminPasswordSecretRef": { "name": "s", "key": "k" } }),
    ))
    .unwrap();
    v["spec"]["adminPassword"] = json!("");
    let raw = serde_json::to_vec(&v).unwrap();
    let req = http::Request::post(format!(
        "/apis/bemade.org/v1alpha1/namespaces/{ns}/odooinstances"
    ))
    .header("Content-Type", "application/json")
    .body(raw)
    .unwrap();
    let created: serde_json::Value = c
        .request(req)
        .await
        .expect("empty literal + ref must be accepted");
    assert_eq!(created["spec"]["adminPassword"], json!(""));
}
