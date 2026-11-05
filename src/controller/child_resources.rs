//! Child resource helpers — ensure_* functions for OdooInstance infrastructure.
//!
//! These create/update the phase-independent Kubernetes resources that every
//! OdooInstance needs: Secrets (incl. the rendered odoo.conf), PG role, PVC, Service, Ingress,
//! Deployment.  They run every reconcile tick before the state machine.

use std::collections::BTreeMap;

use k8s_openapi::api::{
    apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
    batch::v1::Job,
    core::v1::{
        ConfigMap, Container, ContainerPort, EnvVar, ExecAction, HTTPGetAction,
        PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec, PodTemplateSpec, Probe,
        Secret, Service, ServicePort, ServiceSpec, TypedObjectReference,
        VolumeResourceRequirements,
    },
    networking::v1::{
        HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
        IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, ServiceBackendPort,
    },
    storage::v1::StorageClass,
};
use k8s_openapi::apimachinery::pkg::{
    api::resource::Quantity,
    apis::meta::v1::{LabelSelector, OwnerReference},
    util::intstr::IntOrString,
};
use k8s_openapi::ByteString;
use kube::api::{
    Api, DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams, Preconditions,
    ResourceExt,
};
use kube::Client;
use serde_json::json;

use gateway_api::apis::standard::httproutes::{
    HTTPRoute, HTTPRouteParentRefs, HTTPRouteRules, HTTPRouteRulesBackendRefs,
    HTTPRouteRulesMatches, HTTPRouteRulesMatchesPath, HTTPRouteRulesMatchesPathType, HTTPRouteSpec,
};

use crate::crd::odoo_instance::{DeploymentStrategyType, Environment, GatewayRef, OdooInstance};
use crate::error::{Error, Result};
use crate::helpers::{
    build_odoo_conf, db_name, generate_password, odoo_ro_username, odoo_username, parse_quantity,
    sha256_hex,
};
use crate::postgres::PostgresClusterConfig;

use super::helpers::{
    apply_extra_env, cron_depl_name, env, image_pull_secrets, is_ephemeral, job_is_finished,
    odoo_deployment_volume_mounts, odoo_security_context, odoo_volumes,
    pod_spec_references_configmap, secret_env, FIELD_MANAGER,
};
use super::odoo_instance::Context;

/// Write operator-level defaults into unset spec fields and persist via patch.
/// Returns true if the spec was changed (caller should requeue).
pub async fn apply_defaults(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
) -> Result<bool> {
    let mut patch = serde_json::Map::new();

    if instance.spec.image.is_none() {
        let img = if ctx.defaults.odoo_image.is_empty() {
            "odoo:18.0".to_string()
        } else {
            ctx.defaults.odoo_image.clone()
        };
        patch.insert("image".into(), json!(img));
    }

    // Filestore defaults. An ephemeral filestore gets no size/class injected
    // (the webhook denies setting them together with emptyDir), and values
    // left over from before a flip are deliberately left in place: they are
    // inert while ephemeral (`ensure_filestore_pvc` returns early and the
    // storage-class mismatch check is forced off), and they are what makes a
    // flip back to persistent storage reuse the retained PVC as-is. Stripping
    // them here and re-injecting the operator defaults on the way back would
    // silently change the storage class, and start a filestore migration, as
    // a side effect of toggling a boolean twice.
    let fs = instance.spec.filestore.as_ref();
    let mut fs_patch = serde_json::Map::new();
    if !is_ephemeral(instance) {
        if fs.and_then(|f| f.storage_class.as_ref()).is_none() {
            let sc = if ctx.defaults.storage_class.is_empty() {
                "standard".to_string()
            } else {
                ctx.defaults.storage_class.clone()
            };
            fs_patch.insert("storageClass".into(), json!(sc));
        }
        if fs.and_then(|f| f.storage_size.as_ref()).is_none() {
            let sz = if ctx.defaults.storage_size.is_empty() {
                "2Gi".to_string()
            } else {
                ctx.defaults.storage_size.clone()
            };
            fs_patch.insert("storageSize".into(), json!(sz));
        }
    }
    if !fs_patch.is_empty() {
        patch.insert("filestore".into(), json!(fs_patch));
    }

    // Ingress defaults.
    let mut ing_patch = serde_json::Map::new();
    if instance.spec.ingress.issuer.is_none() && !ctx.defaults.ingress_issuer.is_empty() {
        ing_patch.insert("issuer".into(), json!(ctx.defaults.ingress_issuer));
    }
    if instance.spec.ingress.class.is_none() && !ctx.defaults.ingress_class.is_empty() {
        ing_patch.insert("class".into(), json!(ctx.defaults.ingress_class));
    }
    if instance.spec.ingress.gateway_ref.is_none()
        && !ctx.defaults.gateway_ref_name.is_empty()
        && !ctx.defaults.gateway_ref_namespace.is_empty()
    {
        ing_patch.insert(
            "gatewayRef".into(),
            json!({
                "name": ctx.defaults.gateway_ref_name,
                "namespace": ctx.defaults.gateway_ref_namespace,
            }),
        );
    }
    if !ing_patch.is_empty() {
        patch.insert("ingress".into(), json!(ing_patch));
    }

    // Resources, affinity, tolerations defaults.
    if instance.spec.resources.is_none() && ctx.defaults.resources.is_some() {
        patch.insert("resources".into(), json!(ctx.defaults.resources));
    }
    if instance.spec.affinity.is_none() && ctx.defaults.affinity.is_some() {
        patch.insert("affinity".into(), json!(ctx.defaults.affinity));
    }
    if instance.spec.tolerations.is_empty() && !ctx.defaults.tolerations.is_empty() {
        patch.insert("tolerations".into(), json!(ctx.defaults.tolerations));
    }

    // Cron resources
    let mut cron_patch = serde_json::Map::new();
    if instance.spec.cron.resources.is_none() && ctx.defaults.resources.is_some() {
        cron_patch.insert("resources".into(), json!(ctx.defaults.resources));
    }
    if !cron_patch.is_empty() {
        patch.insert("cron".into(), json!(cron_patch));
    }

    if patch.is_empty() {
        return Ok(false);
    }

    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let spec_patch = json!({"spec": patch});
    api.patch(
        name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&spec_patch),
    )
    .await?;
    Ok(true)
}

/// Copy the image pull secret from the operator namespace into the instance
/// namespace so that Deployments and Jobs can pull from private registries.
/// No-op if the instance has no `imagePullSecret` configured.
pub async fn ensure_image_pull_secret(
    client: &Client,
    ns: &str,
    instance: &OdooInstance,
    operator_namespace: &str,
) -> Result<()> {
    let secret_name = match &instance.spec.image_pull_secret {
        Some(name) if !name.is_empty() => name.clone(),
        _ => return Ok(()),
    };

    let target_secrets: Api<Secret> = Api::namespaced(client.clone(), ns);

    // Already exists in target namespace — nothing to do.
    if target_secrets.get(&secret_name).await.is_ok() {
        return Ok(());
    }

    // Read from operator namespace and mirror into instance namespace.
    let source_secrets: Api<Secret> = Api::namespaced(client.clone(), operator_namespace);
    let source = source_secrets.get(&secret_name).await?;

    let mirrored = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name),
            namespace: Some(ns.to_string()),
            // No owner reference — the secret should survive instance deletion
            // so other instances in the same namespace can share it.
            ..Default::default()
        },
        data: source.data,
        type_: source.type_,
        ..Default::default()
    };
    target_secrets
        .create(&PostParams::default(), &mirrored)
        .await?;
    Ok(())
}

pub async fn ensure_odoo_user_secret(
    client: &Client,
    ns: &str,
    name: &str,
    oref: &OwnerReference,
) -> Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret_name = format!("{name}-odoo-user");

    // Only create if it doesn't exist (credentials are generated once).
    match secrets.get(&secret_name).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            let username = odoo_username(ns, name);
            let password = generate_password();
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(secret_name.clone()),
                    namespace: Some(ns.to_string()),
                    owner_references: Some(vec![oref.clone()]),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([
                    ("username".to_string(), username),
                    ("password".to_string(), password),
                ])),
                ..Default::default()
            };
            secrets.create(&PostParams::default(), &secret).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Read the Odoo user credentials (username + password) from the instance's
/// `-odoo-user` Secret.
pub async fn read_odoo_credentials(
    client: &Client,
    ns: &str,
    name: &str,
) -> Result<(String, String)> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets.get(&format!("{name}-odoo-user")).await?;

    let data = secret.data.unwrap_or_default();
    let username = String::from_utf8_lossy(
        data.get("username")
            .map(|v| v.0.as_slice())
            .unwrap_or_default(),
    )
    .to_string();
    let password = String::from_utf8_lossy(
        data.get("password")
            .map(|v| v.0.as_slice())
            .unwrap_or_default(),
    )
    .to_string();

    Ok((username, password))
}

pub async fn ensure_postgres_role(
    ctx: &Context,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let (username, password) = read_odoo_credentials(&ctx.client, &ns, &name).await?;
    ctx.postgres.ensure_role(pg, &username, &password).await
}

/// Create the filestore PVC for an OdooInstance, expanding it in place if
/// the spec requests a larger size than what's already provisioned.
///
/// `explicit_data_source` is an override used by the staging refresh's
/// `CloningFromSource` state to inject a `VolumeSnapshot` reference (the
/// universal path that works for both CephFS and JuiceFS CSI drivers).
/// When `None`, the function falls back to the legacy auto-detection in
/// `get_pvc_source` (PVC→PVC clone, only works on CephFS-class drivers).
/// The always-on reconcile path always passes `None`; only the refresh
/// state handler passes `Some`.
pub async fn ensure_filestore_pvc(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
    explicit_data_source: Option<TypedObjectReference>,
) -> Result<()> {
    // Ephemeral filestore: no PVC is desired. Returning before any create or
    // reconcile is also the RETENTION mechanism — an existing PVC from before
    // a flip to emptyDir is simply left alone (owner-ref still ties its
    // lifetime to the OdooInstance), so the flip orphans data recoverably
    // instead of destroying it.
    if is_ephemeral(instance) {
        return Ok(());
    }

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let pvc_name = format!("{name}-filestore-pvc");

    let storage_size = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_size.as_deref())
        .unwrap_or(&ctx.defaults.storage_size);
    let storage_class = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|f| f.storage_class.as_deref())
        .unwrap_or(&ctx.defaults.storage_class);

    // If the PVC already exists, reconcile its storage request: expand if the
    // spec asks for more than what's currently requested. Storage class changes
    // and shrinks are out of scope here (handled by the migration phases /
    // rejected by the webhook).
    if let Ok(existing) = pvcs.get(&pvc_name).await {
        let current_size = existing
            .spec
            .as_ref()
            .and_then(|s| s.resources.as_ref())
            .and_then(|r| r.requests.as_ref())
            .and_then(|m| m.get("storage"))
            .map(|q| q.0.clone())
            .unwrap_or_default();

        let desired_bytes = parse_quantity(storage_size).unwrap_or(0);
        let current_bytes = parse_quantity(&current_size).unwrap_or(0);

        if desired_bytes > current_bytes {
            tracing::info!(
                pvc = %pvc_name,
                from = %current_size,
                to = %storage_size,
                "expanding filestore PVC",
            );
            let patch = json!({
                "spec": {
                    "resources": {
                        "requests": { "storage": storage_size }
                    }
                }
            });
            pvcs.patch(&pvc_name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        } else if desired_bytes < current_bytes && desired_bytes > 0 {
            tracing::warn!(
                pvc = %pvc_name,
                current = %current_size,
                desired = %storage_size,
                "spec.filestore.storageSize is smaller than the existing PVC; \
                 PVCs cannot shrink — ignoring",
            );
        }
        return Ok(());
    }

    // No existing PVC, so we fully construct it, possibly with a data source.
    // Explicit caller-provided source wins (used by CloningFromSource's
    // VolumeSnapshot path).  Otherwise fall back to the legacy
    // PVC-to-PVC clone auto-detection.
    let source = match explicit_data_source {
        Some(s) => Some(s),
        None => get_pvc_source(client, ns, instance).await,
    };
    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
            data_source_ref: source,
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity(storage_size.to_string()),
                )])),
                ..Default::default()
            }),
            storage_class_name: Some(storage_class.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    pvcs.create(&PostParams::default(), &pvc).await?;
    Ok(())
}

/// CSI provisioners that accept only a `VolumeSnapshot` as a PVC data source
/// and reject a `PersistentVolumeClaim` one ("only VolumeSnapshot data source
/// is supported"), which would leave a cloned PVC Pending forever.
const SNAPSHOT_ONLY_PROVISIONERS: &[&str] = &["csi.juicefs.com"];

/// Whether `class` is provisioned by a driver in `SNAPSHOT_ONLY_PROVISIONERS`.
/// A class that cannot be read is assumed clone-capable, which keeps the
/// behaviour from before the check existed.
async fn is_snapshot_only_class(client: &Client, class: &str) -> bool {
    let classes: Api<StorageClass> = Api::all(client.clone());
    match classes.get_opt(class).await {
        Ok(Some(sc)) => SNAPSHOT_ONLY_PROVISIONERS.contains(&sc.provisioner.as_str()),
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                storage_class = %class,
                error = %e,
                "could not read StorageClass; assuming it can clone PVCs"
            );
            false
        }
    }
}

/// Make a snapshot from the production instance PVC if possible,
/// returns a reference to be used in the PVC spec as a source_ref.
async fn get_pvc_source(
    client: &Client,
    ns: &str,
    instance: &OdooInstance,
) -> Option<TypedObjectReference> {
    let inst_name = instance.name_any();
    let Environment::Staging = instance.spec.environment else {
        tracing::debug!(name = %inst_name, "get_pvc_source: not staging");
        return None;
    };
    let Some(production_ref) = instance.spec.production_instance_ref.as_ref() else {
        tracing::debug!(name = %inst_name, "get_pvc_source: no production_instance_ref");
        return None;
    };
    let prod_ns = production_ref.namespace.as_deref().unwrap_or(ns);
    let production_name = production_ref.name.as_str();
    let src_pvc_name = format!("{production_name}-filestore-pvc");
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), prod_ns);
    let prod_pvc = match pvcs.get(src_pvc_name.as_str()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                name = %inst_name,
                error = %e,
                pvc = %src_pvc_name,
                ns = %prod_ns,
                "get_pvc_source: prod PVC lookup failed"
            );
            return None;
        }
    };
    let sc = instance
        .spec
        .filestore
        .as_ref()
        .and_then(|spec| spec.storage_class.as_deref());
    let prod_sc = prod_pvc
        .spec
        .as_ref()
        .and_then(|spec| spec.storage_class_name.as_deref());
    if sc != prod_sc {
        tracing::info!(
            name = %inst_name,
            target_sc = ?sc,
            prod_sc = ?prod_sc,
            "get_pvc_source: storage class mismatch — falling back to copy"
        );
        return None;
    }
    if let Some(class) = prod_sc {
        if is_snapshot_only_class(client, class).await {
            tracing::info!(
                name = %inst_name,
                storage_class = %class,
                "get_pvc_source: driver restores only from VolumeSnapshots — \
                 creating the PVC empty; a staging refresh fills it"
            );
            return None;
        }
    }
    tracing::info!(
        name = %inst_name,
        src_pvc = %src_pvc_name,
        ns = %prod_ns,
        "get_pvc_source: returning dataSourceRef for snapshot/clone"
    );
    // Only set `namespace` when the source PVC is actually in a different
    // namespace.  Setting it for same-namespace clones triggers K8s'
    // cross-namespace data-source path, which requires the alpha
    // `CrossNamespaceVolumeDataSource` feature gate plus a ReferenceGrant
    // — without those, the API server silently drops the entire
    // `dataSourceRef` field and the PVC binds empty.
    let namespace = if prod_ns == ns {
        None
    } else {
        Some(prod_ns.to_string())
    };
    Some(TypedObjectReference {
        api_group: None,
        kind: "PersistentVolumeClaim".to_string(),
        name: src_pvc_name,
        namespace,
    })
}

/// Name of the Secret the operator generates the master password into when
/// the spec gives none.
pub fn generated_admin_password_secret_name(name: &str) -> String {
    format!("{name}-admin-password")
}

/// Generate-if-empty for the Odoo master password (#177).
///
/// When the spec sets neither `adminPassword` nor `adminPasswordSecretRef`,
/// create (if absent) the owned Secret `<name>-admin-password` holding a
/// random password, then point `spec.adminPasswordSecretRef` at it. The value
/// never enters the spec. Create-if-absent before the patch makes this
/// idempotent: if the patch fails, the next reconcile reuses the Secret
/// instead of minting a second password. Returns true when the spec was
/// patched (the caller requeues).
pub async fn ensure_admin_password_ref(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    oref: &OwnerReference,
) -> Result<bool> {
    if instance.spec.admin_password_secret_ref.is_some() || !instance.spec.admin_password.is_empty()
    {
        return Ok(false);
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret_name = generated_admin_password_secret_name(name);
    if secrets.get_opt(&secret_name).await?.is_none() {
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(secret_name.clone()),
                namespace: Some(ns.to_string()),
                owner_references: Some(vec![oref.clone()]),
                ..Default::default()
            },
            type_: Some("Opaque".to_string()),
            string_data: Some(BTreeMap::from([(
                ADMIN_PASSWORD_KEY.to_string(),
                generate_password(),
            )])),
            ..Default::default()
        };
        match secrets.create(&PostParams::default(), &secret).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(e) => return Err(e.into()),
        }
    }
    let api: Api<OdooInstance> = Api::namespaced(client.clone(), ns);
    let patch = json!({"spec": {"adminPasswordSecretRef": {
        "name": secret_name,
        "key": ADMIN_PASSWORD_KEY,
    }}});
    api.patch(
        name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(true)
}

/// Key of the generated master-password Secret.
const ADMIN_PASSWORD_KEY: &str = "password";

/// The master password to render as `admin_passwd`: read from
/// `spec.adminPasswordSecretRef` when set, else the deprecated literal
/// (rendered exactly as before; empty means none).
///
/// A missing Secret or key is an error, never a fallback: falling back to the
/// literal, to generation or to no password would silently change the master
/// password. See [`master_password_line`] for what a usable value is (a
/// trailing newline, as `kubectl create secret --from-file` leaves, is fine).
async fn resolve_admin_password(
    client: &Client,
    ns: &str,
    instance: &OdooInstance,
) -> Result<String> {
    let Some(r) = instance.spec.admin_password_secret_ref.as_ref() else {
        return Ok(instance.spec.admin_password.clone());
    };
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets.get_opt(&r.name).await?.ok_or_else(|| {
        Error::config(format!(
            "spec.adminPasswordSecretRef: Secret {ns}/{} not found",
            r.name
        ))
    })?;
    let raw = secret
        .data
        .and_then(|mut d| d.remove(&r.key))
        .ok_or_else(|| {
            Error::config(format!(
                "spec.adminPasswordSecretRef: Secret {ns}/{} has no key {:?}",
                r.name, r.key
            ))
        })?;
    master_password_line(&raw.0).ok_or_else(|| {
        Error::config(format!(
            "spec.adminPasswordSecretRef: {ns}/{}[{}] must be a non-empty single line of UTF-8",
            r.name, r.key
        ))
    })
}

/// A Secret value usable as `admin_passwd`: UTF-8, one trailing newline
/// dropped, non-empty, and no other line break (which would inject lines into
/// odoo.conf).
fn master_password_line(raw: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(raw).ok()?;
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value);
    (!value.is_empty() && !value.contains(['\n', '\r'])).then(|| value.to_string())
}

/// Render odoo.conf into the `<name>-odoo-conf` Secret.
///
/// A Secret rather than a ConfigMap: the file carries `db_password` and the
/// `admin_passwd` master password, and Odoo reads the latter only from the
/// config file, so the file itself has to be secret (#131). Besides
/// `odoo.conf`, the Secret carries the connection details as individual keys
/// (`db_host`, `db_port`, `db_name`, `db_user`, `db_password`) for the Job
/// builders, which inject them with `secretKeyRef`.
pub async fn ensure_odoo_conf_secret(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
    oref: &OwnerReference,
) -> Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let conf_name = format!("{name}-odoo-conf");
    let username = odoo_username(ns, name);
    let db = db_name(instance);

    // Read password from the odoo-user secret.
    let secret = secrets.get(&format!("{name}-odoo-user")).await?;
    let data = secret.data.unwrap_or_default();
    let password = String::from_utf8_lossy(
        data.get("password")
            .map(|v| v.0.as_slice())
            .unwrap_or_default(),
    )
    .to_string();

    let admin_password = resolve_admin_password(client, ns, instance).await?;
    let conf = build_odoo_conf(
        &username,
        &password,
        &admin_password,
        &pg.host,
        pg.port,
        &db,
        &instance.spec.config_options,
    );

    let entries = [
        ("odoo.conf", conf),
        ("db_host", pg.host.clone()),
        ("db_port", pg.port.to_string()),
        ("db_name", db),
        ("db_user", username),
        ("db_password", password),
    ];
    let conf_secret = Secret {
        metadata: ObjectMeta {
            name: Some(conf_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), ByteString(v.into_bytes())))
                .collect(),
        ),
        ..Default::default()
    };

    secrets
        .patch(
            &conf_name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&conf_secret),
        )
        .await?;
    Ok(())
}

/// SHA-256 of the rendered odoo.conf, stamped on the pod templates so a
/// config change rolls the Deployments.
async fn odoo_conf_hash(client: &Client, ns: &str, name: &str) -> Result<String> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let conf = secrets.get(&format!("{name}-odoo-conf")).await?;
    let content = conf
        .data
        .as_ref()
        .and_then(|d| d.get("odoo.conf"))
        .map(|b| String::from_utf8_lossy(&b.0).into_owned())
        .unwrap_or_default();
    Ok(sha256_hex(&content))
}

/// Delete the `<name>-odoo-conf` ConfigMap that operator versions before the
/// move to a Secret (#131) rendered odoo.conf into, once nothing needs it.
///
/// It holds `db_password` and `admin_passwd` in plaintext, so it must not
/// linger; but a pod that has not started yet and mounts it, or reads a key
/// from it, would hang on its absence. So it is deleted only when no
/// Deployment template, no unfinished Job template and no live Pod in the
/// namespace references it. Until then every reconcile re-checks; nothing
/// writes to it any more. A ConfigMap of that name the instance does not own
/// is somebody else's and is never touched.
pub async fn delete_legacy_conf_configmap(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
) -> Result<()> {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm_name = format!("{name}-odoo-conf");
    let Some(cm) = cms.get_opt(&cm_name).await? else {
        return Ok(());
    };
    let owned = instance.metadata.uid.as_deref().is_some_and(|uid| {
        cm.owner_references()
            .iter()
            .any(|o| o.uid == uid && o.kind == "OdooInstance")
    });
    if !owned {
        return Ok(());
    }

    let lp = ListParams::default();
    let deps: Api<Deployment> = Api::namespaced(client.clone(), ns);
    if let Some(d) = deps.list(&lp).await?.into_iter().find(|d| {
        d.spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .is_some_and(|p| pod_spec_references_configmap(p, &cm_name))
    }) {
        tracing::debug!(%name, deployment = %d.name_any(), "legacy odoo-conf ConfigMap still referenced");
        return Ok(());
    }
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    if let Some(j) = jobs.list(&lp).await?.into_iter().find(|j| {
        !job_is_finished(j)
            && j.spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .is_some_and(|p| pod_spec_references_configmap(p, &cm_name))
    }) {
        tracing::info!(%name, job = %j.name_any(), "legacy odoo-conf ConfigMap kept until this Job finishes");
        return Ok(());
    }
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    if let Some(p) = pods.list(&lp).await?.into_iter().find(|p| {
        let done = p
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .is_some_and(|ph| ph == "Succeeded" || ph == "Failed");
        !done
            && p.spec
                .as_ref()
                .is_some_and(|s| pod_spec_references_configmap(s, &cm_name))
    }) {
        tracing::debug!(%name, pod = %p.name_any(), "legacy odoo-conf ConfigMap still mounted");
        return Ok(());
    }

    // Preconditions: never delete a ConfigMap recreated or changed since we
    // looked at it.
    let dp = DeleteParams {
        preconditions: Some(Preconditions {
            uid: cm.metadata.uid.clone(),
            resource_version: cm.metadata.resource_version.clone(),
        }),
        ..Default::default()
    };
    match cms.delete(&cm_name, &dp).await {
        Ok(_) => {
            tracing::info!(%name, "deleted legacy odoo-conf ConfigMap (odoo.conf now lives in a Secret)");
            Ok(())
        }
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn ensure_service(
    client: &Client,
    ns: &str,
    name: &str,
    oref: &OwnerReference,
) -> Result<()> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), ns);
    let svc = Service {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            type_: Some("ClusterIP".to_string()),
            ports: Some(vec![
                ServicePort {
                    name: Some("http".to_string()),
                    port: 8069,
                    target_port: Some(IntOrString::Int(8069)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                },
                ServicePort {
                    name: Some("websocket".to_string()),
                    port: 8072,
                    target_port: Some(IntOrString::Int(8072)),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };

    svcs.patch(
        name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&svc),
    )
    .await?;
    Ok(())
}

pub async fn ensure_ingress(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    oref: &OwnerReference,
) -> Result<()> {
    let ingresses: Api<Ingress> = Api::namespaced(client.clone(), ns);

    let mut annotations = BTreeMap::new();
    if let Some(ref issuer) = instance.spec.ingress.issuer {
        annotations.insert("cert-manager.io/cluster-issuer".to_string(), issuer.clone());
    }

    let path_type = "Prefix".to_string();
    let rules: Vec<IngressRule> = instance
        .spec
        .ingress
        .hosts
        .iter()
        .map(|host| IngressRule {
            host: Some(host.clone()),
            http: Some(HTTPIngressRuleValue {
                paths: vec![
                    HTTPIngressPath {
                        path: Some("/websocket".to_string()),
                        path_type: path_type.clone(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: name.to_string(),
                                port: Some(ServiceBackendPort {
                                    number: Some(8072),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    },
                    HTTPIngressPath {
                        path: Some("/".to_string()),
                        path_type: path_type.clone(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: name.to_string(),
                                port: Some(ServiceBackendPort {
                                    number: Some(8069),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    },
                ],
            }),
        })
        .collect();

    let ing = Ingress {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            annotations: Some(annotations),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(K8sIngressSpec {
            ingress_class_name: instance.spec.ingress.class.clone(),
            rules: Some(rules),
            tls: Some(vec![IngressTLS {
                hosts: Some(instance.spec.ingress.hosts.clone()),
                secret_name: Some(format!("{name}-tls")),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    ingresses
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&ing),
        )
        .await?;
    Ok(())
}

pub async fn ensure_deployment(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
) -> Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);

    // Replicas are managed by the state machine via scale_deployment().
    // Here we only ensure the Deployment spec (image, probes, volumes, etc.)
    // exists.  We read the current replica count so we don't clobber it.
    let current_replicas = match deployments.get(name).await {
        Ok(dep) => dep.spec.and_then(|s| s.replicas).unwrap_or(0),
        Err(_) => 0,
    };
    let replicas = current_replicas;
    let image = instance
        .spec
        .image
        .as_deref()
        .unwrap_or(&ctx.defaults.odoo_image);

    let strategy_type = instance
        .spec
        .strategy
        .as_ref()
        .map(|s| &s.strategy_type)
        .unwrap_or(&DeploymentStrategyType::Recreate);
    let k8s_strategy = match strategy_type {
        DeploymentStrategyType::Recreate => "Recreate",
        DeploymentStrategyType::RollingUpdate => "RollingUpdate",
    };

    let probe_startup = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.startup_path.as_str())
        .unwrap_or("/web/health");
    let probe_liveness = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.liveness_path.as_str())
        .unwrap_or("/web/health");
    let probe_readiness = instance
        .spec
        .probes
        .as_ref()
        .map(|p| p.readiness_path.as_str())
        .unwrap_or("/web/health");

    // Hash odoo.conf for rollout trigger.
    let conf_hash = odoo_conf_hash(client, ns, name).await?;

    // Override PGDATABASE so the Odoo config layer (which reads env vars
    // with higher priority than the config file) uses the correct database.
    let db = db_name(instance);
    let mut pg_env = vec![env("PGDATABASE", &db)];
    pg_env.extend(readonly_sql_env(instance));

    let make_http_probe = |path: &str| -> Probe {
        Probe {
            http_get: Some(HTTPGetAction {
                path: Some(path.to_string()),
                port: IntOrString::Int(8069),
                ..Default::default()
            }),
            ..Default::default()
        }
    };

    let mut depl_labels = BTreeMap::from([("app".to_string(), name.to_string())]);
    depl_labels.extend(super::helpers::instance_labels(instance));
    let dep = Deployment {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(depl_labels.clone()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            // NOTE: selector.matchLabels stays on `app` only — selectors are
            // immutable on existing Deployments, so we can't add env labels
            // here without breaking in-place upgrades.  The env labels are
            // added to pod template labels below instead (which Calico keys on).
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some(k8s_strategy.to_string()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(depl_labels.clone()),
                    annotations: Some(BTreeMap::from([(
                        "bemade.org/odoo-conf-hash".to_string(),
                        conf_hash,
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    image_pull_secrets: image_pull_secrets(instance),
                    affinity: instance.spec.affinity.clone(),
                    tolerations: if instance.spec.tolerations.is_empty() {
                        None
                    } else {
                        Some(instance.spec.tolerations.clone())
                    },
                    security_context: Some(odoo_security_context()),
                    volumes: Some(odoo_volumes(name, is_ephemeral(instance))),
                    containers: vec![apply_extra_env(
                        Container {
                            name: format!("odoo-{name}"),
                            image: Some(image.to_string()),
                            image_pull_policy: Some("IfNotPresent".to_string()),
                            command: Some(vec![
                                "/entrypoint.sh".to_string(),
                                "odoo".to_string(),
                                "--max-cron-threads".to_string(),
                                "0".to_string(),
                            ]),
                            ports: Some(vec![
                                ContainerPort {
                                    name: Some("http".to_string()),
                                    container_port: 8069,
                                    ..Default::default()
                                },
                                ContainerPort {
                                    name: Some("websocket".to_string()),
                                    container_port: 8072,
                                    ..Default::default()
                                },
                            ]),
                            env: Some(pg_env),
                            volume_mounts: Some(odoo_deployment_volume_mounts()),
                            resources: instance.spec.resources.clone(),
                            startup_probe: Some(Probe {
                                initial_delay_seconds: Some(5),
                                period_seconds: Some(10),
                                timeout_seconds: Some(5),
                                failure_threshold: Some(30),
                                ..make_http_probe(probe_startup)
                            }),
                            liveness_probe: Some(Probe {
                                period_seconds: Some(15),
                                timeout_seconds: Some(5),
                                failure_threshold: Some(3),
                                ..make_http_probe(probe_liveness)
                            }),
                            readiness_probe: Some(Probe {
                                period_seconds: Some(10),
                                timeout_seconds: Some(5),
                                failure_threshold: Some(3),
                                ..make_http_probe(probe_readiness)
                            }),
                            ..Default::default()
                        },
                        instance,
                    )],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deployments
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&dep),
        )
        .await?;
    Ok(())
}

/// Create or update an HTTPRoute for Gateway API mode.
pub async fn ensure_http_route(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    gateway_ref: &GatewayRef,
    oref: &OwnerReference,
) -> Result<()> {
    let routes: Api<HTTPRoute> = Api::namespaced(client.clone(), ns);

    let route = HTTPRoute {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: HTTPRouteSpec {
            parent_refs: Some(vec![HTTPRouteParentRefs {
                group: Some("gateway.networking.k8s.io".to_string()),
                kind: Some("Gateway".to_string()),
                namespace: Some(gateway_ref.namespace.clone()),
                name: gateway_ref.name.clone(),
                section_name: None,
                port: None,
            }]),
            hostnames: Some(instance.spec.ingress.hosts.clone()),
            rules: Some(vec![
                HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/websocket".to_string()),
                        }),
                        headers: None,
                        query_params: None,
                        method: None,
                    }]),
                    filters: None,
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: name.to_string(),
                        port: Some(8072),
                        filters: None,
                        group: None,
                        kind: Some("Service".to_string()),
                        namespace: None,
                        weight: None,
                    }]),
                    timeouts: None,
                },
                HTTPRouteRules {
                    matches: Some(vec![HTTPRouteRulesMatches {
                        path: Some(HTTPRouteRulesMatchesPath {
                            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                            value: Some("/".to_string()),
                        }),
                        headers: None,
                        query_params: None,
                        method: None,
                    }]),
                    filters: None,
                    backend_refs: Some(vec![HTTPRouteRulesBackendRefs {
                        name: name.to_string(),
                        port: Some(8069),
                        filters: None,
                        group: None,
                        kind: Some("Service".to_string()),
                        namespace: None,
                        weight: None,
                    }]),
                    timeouts: None,
                },
            ]),
        },
        status: None,
    };

    routes
        .patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&route),
        )
        .await?;
    Ok(())
}

/// Ensure the correct routing resource (Ingress or HTTPRoute) exists and
/// clean up the stale one when switching modes.
pub async fn ensure_routing(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    oref: &OwnerReference,
) -> Result<()> {
    if let Some(ref gw) = instance.spec.ingress.gateway_ref {
        // Gateway API mode — create HTTPRoute and delete stale Ingress.
        ensure_http_route(client, ns, name, instance, gw, oref).await?;
        let ingresses: Api<Ingress> = Api::namespaced(client.clone(), ns);
        if ingresses.get(name).await.is_ok() {
            ingresses
                .delete(name, &Default::default())
                .await
                .map(|_| ())?;
        }
    } else {
        // Ingress mode — create Ingress and delete stale HTTPRoute.
        ensure_ingress(client, ns, name, instance, oref).await?;
        let routes: Api<HTTPRoute> = Api::namespaced(client.clone(), ns);
        if routes.get(name).await.is_ok() {
            routes.delete(name, &Default::default()).await.map(|_| ())?;
        }
    }
    Ok(())
}

pub async fn ensure_cron_deployment(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
) -> Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);

    // Replicas are managed by the state machine via scale_deployment().
    // Here we only ensure the Deployment spec (image, probes, volumes, etc.)
    // exists.  We read the current replica count so we don't clobber it.
    let depl_name = cron_depl_name(instance);
    let current_replicas = match deployments.get(depl_name.as_str()).await {
        Ok(dep) => dep.spec.and_then(|s| s.replicas).unwrap_or(0),
        Err(_) => 0,
    };
    let replicas = current_replicas;
    let image = instance
        .spec
        .image
        .as_deref()
        .unwrap_or(&ctx.defaults.odoo_image);

    let strategy_type = instance
        .spec
        .strategy
        .as_ref()
        .map(|s| &s.strategy_type)
        .unwrap_or(&DeploymentStrategyType::Recreate);
    let k8s_strategy = match strategy_type {
        DeploymentStrategyType::Recreate => "Recreate",
        DeploymentStrategyType::RollingUpdate => "RollingUpdate",
    };

    // Override PGDATABASE (see ensure_deployment for rationale).
    let db = db_name(instance);
    let pg_env = vec![env("PGDATABASE", &db)];

    // Hash odoo.conf for rollout trigger.
    let conf_hash = odoo_conf_hash(client, ns, name).await?;

    let mut depl_labels = BTreeMap::from([("app".to_string(), depl_name.to_string())]);
    depl_labels.extend(super::helpers::instance_labels(instance));
    // Cron pods carry the same `app=<cron-depl>` for service/selector matching,
    // plus the env labels for Calico (identical to web).
    let dep = Deployment {
        metadata: ObjectMeta {
            name: Some(depl_name.clone()),
            namespace: Some(ns.to_string()),
            labels: Some(depl_labels.clone()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([("app".to_string(), depl_name.to_string())])),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                type_: Some(k8s_strategy.to_string()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(depl_labels.clone()),
                    annotations: Some(BTreeMap::from([(
                        "bemade.org/odoo-conf-hash".to_string(),
                        conf_hash,
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    image_pull_secrets: image_pull_secrets(instance),
                    affinity: instance.spec.affinity.clone(),
                    tolerations: if instance.spec.tolerations.is_empty() {
                        None
                    } else {
                        Some(instance.spec.tolerations.clone())
                    },
                    security_context: Some(odoo_security_context()),
                    volumes: Some(odoo_volumes(name, is_ephemeral(instance))),
                    containers: vec![{
                        // Cron pods run --no-http so there is no HTTP endpoint
                        // for probes.  Instead we query PostgreSQL directly to
                        // detect a stuck cron system.
                        // See scripts/cron_{startup,liveness}_probe.py.
                        let startup_cmd = vec![
                            "python3".to_string(),
                            "-c".to_string(),
                            include_str!("../../scripts/cron_startup_probe.py").to_string(),
                        ];
                        let liveness_cmd = vec![
                            "python3".to_string(),
                            "-c".to_string(),
                            include_str!("../../scripts/cron_liveness_probe.py").to_string(),
                        ];

                        apply_extra_env(
                            Container {
                                name: format!("odoo-cron-{name}"),
                                image: Some(image.to_string()),
                                image_pull_policy: Some("IfNotPresent".to_string()),
                                command: Some(vec![
                                    "/entrypoint.sh".to_string(),
                                    "odoo".to_string(),
                                    "--workers".to_string(),
                                    "0".to_string(),
                                    "--no-http".to_string(),
                                ]),
                                env: Some(pg_env),
                                volume_mounts: Some(odoo_deployment_volume_mounts()),
                                resources: instance.spec.cron.resources.clone(),
                                startup_probe: Some(Probe {
                                    initial_delay_seconds: Some(5),
                                    period_seconds: Some(10),
                                    timeout_seconds: Some(5),
                                    failure_threshold: Some(30),
                                    exec: Some(ExecAction {
                                        command: Some(startup_cmd),
                                    }),
                                    ..Default::default()
                                }),
                                liveness_probe: Some(Probe {
                                    initial_delay_seconds: Some(300),
                                    period_seconds: Some(30),
                                    timeout_seconds: Some(5),
                                    failure_threshold: Some(3),
                                    exec: Some(ExecAction {
                                        command: Some(liveness_cmd),
                                    }),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            instance,
                        )
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    deployments
        .patch(
            depl_name.as_str(),
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&dep),
        )
        .await?;
    Ok(())
}

// ── Read-only SQL access ──────────────────────────────────────────────────────

/// Name of the Secret that holds the read-only role password.
pub fn ro_secret_name(instance_name: &str) -> String {
    format!("{instance_name}-db-ro-password")
}

/// Build the env vars that expose the read-only DB credentials to the web pod.
///
/// Returns two `EnvVar`s sourced from the `<instance>-db-ro-password` Secret
/// when `spec.readOnlySqlAccess.enabled` is true; otherwise returns an empty
/// Vec.  Called from `ensure_deployment` to extend the container env.
pub fn readonly_sql_env(instance: &OdooInstance) -> Vec<EnvVar> {
    if instance
        .spec
        .read_only_sql_access
        .as_ref()
        .is_some_and(|s| s.enabled)
    {
        let secret = ro_secret_name(&instance.name_any());
        vec![
            secret_env("ODOO_RO_DB_USER", &secret, "username"),
            secret_env("ODOO_RO_DB_PASSWORD", &secret, "password"),
        ]
    } else {
        vec![]
    }
}

/// Ensure the k8s Secret for the read-only role exists.
/// The Secret is created once with a random password; subsequent reconciles
/// are no-ops so the password is stable (rotate by deleting the Secret).
///
/// Returns `(ro_username, ro_password)`.
pub async fn ensure_readonly_secret(
    client: &Client,
    ns: &str,
    name: &str,
    oref: &OwnerReference,
) -> Result<(String, String)> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret_name = ro_secret_name(name);
    let ro_username = odoo_ro_username(ns, name);

    match secrets.get(&secret_name).await {
        Ok(existing) => {
            let data = existing.data.unwrap_or_default();
            let password = String::from_utf8_lossy(
                data.get("password")
                    .map(|v| v.0.as_slice())
                    .unwrap_or_default(),
            )
            .to_string();
            Ok((ro_username, password))
        }
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            let password = generate_password();
            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(secret_name.clone()),
                    namespace: Some(ns.to_string()),
                    owner_references: Some(vec![oref.clone()]),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([
                    ("username".to_string(), ro_username.clone()),
                    ("password".to_string(), password.clone()),
                ])),
                ..Default::default()
            };
            secrets.create(&PostParams::default(), &secret).await?;
            Ok((ro_username, password))
        }
        Err(e) => Err(e.into()),
    }
}

/// Ensure the read-only Postgres role and its per-DB grants are in place.
///
/// Reads/creates the k8s Secret for the RO password, then delegates to
/// `PostgresManager::ensure_readonly_role` for the SQL side.  Idempotent —
/// safe to call on every reconcile tick.
///
/// No-op when `spec.readOnlySqlAccess` is absent or `enabled: false`.
pub async fn ensure_readonly_role(
    ctx: &Context,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
    oref: &OwnerReference,
) -> Result<()> {
    let spec = match instance
        .spec
        .read_only_sql_access
        .as_ref()
        .filter(|s| s.enabled)
    {
        Some(s) => s,
        None => return Ok(()),
    };

    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();

    let (ro_username, ro_password) = ensure_readonly_secret(&ctx.client, &ns, &name, oref).await?;
    let (owner_username, owner_password) = read_odoo_credentials(&ctx.client, &ns, &name).await?;
    let db = db_name(instance);

    ctx.postgres
        .ensure_readonly_role(
            pg,
            crate::postgres::ReadonlyRoleParams {
                ro_username: &ro_username,
                ro_password: &ro_password,
                owner_username: &owner_username,
                owner_password: &owner_password,
                db_name: &db,
                connection_limit: spec.connection_limit,
            },
        )
        .await
}

/// Delete the read-only Postgres role and its k8s Secret.
///
/// Called when `spec.readOnlySqlAccess.enabled` transitions to false or when
/// the instance is being deleted.  The k8s Secret is owned by the instance and
/// will be garbage-collected by kube on instance deletion; this function
/// handles both disable-while-alive and the explicit teardown path.
pub async fn delete_readonly_role(
    ctx: &Context,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let ro_username = odoo_ro_username(&ns, &name);
    let db = db_name(instance);

    ctx.postgres
        .delete_readonly_role(pg, &ro_username, &db)
        .await?;

    // Also delete the k8s Secret so a re-enable generates fresh credentials.
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    let secret_name = ro_secret_name(&name);
    match secrets
        .delete(&secret_name, &kube::api::DeleteParams::default())
        .await
    {
        Ok(_) | Err(kube::Error::Api(kube::core::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::master_password_line;

    #[test]
    fn master_password_from_secret_must_be_one_line() {
        assert_eq!(master_password_line(b"s3cret").as_deref(), Some("s3cret"));
        // `kubectl create secret --from-file` keeps the file's newline.
        assert_eq!(master_password_line(b"s3cret\n").as_deref(), Some("s3cret"));
        assert_eq!(
            master_password_line(b"s3cret\r\n").as_deref(),
            Some("s3cret")
        );
        // A line break would inject options into odoo.conf.
        assert_eq!(master_password_line(b"x\nlist_db = True"), None);
        assert_eq!(master_password_line(b""), None);
        assert_eq!(master_password_line(b"\n"), None);
        assert_eq!(master_password_line(&[0xff, 0xfe]), None);
    }
}
