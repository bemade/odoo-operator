//! Child resource helpers — ensure_* functions for OdooInstance infrastructure.
//!
//! These create/update the phase-independent Kubernetes resources that every
//! OdooInstance needs: Secret, PG role, PVC, ConfigMap, Service, Ingress,
//! Deployment.  They run every reconcile tick before the state machine.

use std::collections::BTreeMap;

use k8s_openapi::api::{
    apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
    core::v1::{
        ConfigMap, Container, ContainerPort, HTTPGetAction, PersistentVolumeClaim,
        PersistentVolumeClaimSpec, PodSpec, PodTemplateSpec, Probe, Secret, Service, ServicePort,
        ServiceSpec, VolumeResourceRequirements,
    },
    networking::v1::{
        HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
        IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, ServiceBackendPort,
    },
};
use k8s_openapi::apimachinery::pkg::{
    api::resource::Quantity,
    apis::meta::v1::{LabelSelector, OwnerReference},
    util::intstr::IntOrString,
};
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams, ResourceExt};
use kube::Client;
use serde_json::json;

use crate::crd::odoo_instance::{DeploymentStrategyType, OdooInstance};
use crate::error::Result;
use crate::helpers::{build_odoo_conf, db_name, generate_password, odoo_username, sha256_hex};
use crate::postgres::PostgresClusterConfig;

use super::helpers::{
    cron_depl_name, image_pull_secrets, odoo_security_context, odoo_volume_mounts, odoo_volumes,
    FIELD_MANAGER,
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

    // Filestore defaults.
    let fs = instance.spec.filestore.as_ref();
    let mut fs_patch = serde_json::Map::new();
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

pub async fn ensure_postgres_role(
    ctx: &Context,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
) -> Result<()> {
    let ns = instance.namespace().unwrap_or_default();
    let name = instance.name_any();
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
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

    ctx.postgres.ensure_role(pg, &username, &password).await
}

pub async fn ensure_filestore_pvc(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    ctx: &Context,
    oref: &OwnerReference,
) -> Result<()> {
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

    // Only create if it doesn't exist (PVCs are immutable after creation).
    if pvcs.get(&pvc_name).await.is_ok() {
        return Ok(());
    }

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteMany".to_string()]),
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

pub async fn ensure_config_map(
    client: &Client,
    ns: &str,
    name: &str,
    instance: &OdooInstance,
    pg: &PostgresClusterConfig,
    oref: &OwnerReference,
) -> Result<()> {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm_name = format!("{name}-odoo-conf");
    let username = odoo_username(ns, name);
    let db = db_name(instance);

    // Read password from the odoo-user secret.
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret = secrets.get(&format!("{name}-odoo-user")).await?;
    let data = secret.data.unwrap_or_default();
    let password = String::from_utf8_lossy(
        data.get("password")
            .map(|v| v.0.as_slice())
            .unwrap_or_default(),
    )
    .to_string();

    let conf = build_odoo_conf(
        &username,
        &password,
        &instance.spec.admin_password,
        &pg.host,
        pg.port,
        &db,
        &instance.spec.config_options,
    );

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name.clone()),
            namespace: Some(ns.to_string()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        data: Some(BTreeMap::from([
            ("odoo.conf".to_string(), conf),
            ("db_host".to_string(), pg.host.clone()),
            ("db_port".to_string(), pg.port.to_string()),
            ("db_name".to_string(), db),
            ("db_user".to_string(), username),
            ("db_password".to_string(), password),
        ])),
        ..Default::default()
    };

    cms.patch(
        &cm_name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&cm),
    )
    .await?;
    Ok(())
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
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm = cms.get(&format!("{name}-odoo-conf")).await?;
    let conf_content = cm
        .data
        .as_ref()
        .and_then(|d| d.get("odoo.conf"))
        .map(|s| s.as_str())
        .unwrap_or("");
    let conf_hash = sha256_hex(conf_content);

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

    let dep = Deployment {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(replicas),
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
                    labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
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
                    volumes: Some(odoo_volumes(name)),
                    containers: vec![Container {
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
                        volume_mounts: Some(odoo_volume_mounts()),
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
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&dep),
        )
        .await?;
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

    // Hash odoo.conf for rollout trigger.
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm = cms.get(&format!("{name}-odoo-conf")).await?;
    let conf_content = cm
        .data
        .as_ref()
        .and_then(|d| d.get("odoo.conf"))
        .map(|s| s.as_str())
        .unwrap_or("");
    let conf_hash = sha256_hex(conf_content);

    let dep = Deployment {
        metadata: ObjectMeta {
            name: Some(depl_name.clone()),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([("app".to_string(), name.to_string())])),
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
                    labels: Some(BTreeMap::from([("app".to_string(), depl_name.to_string())])),
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
                    volumes: Some(odoo_volumes(name)),
                    containers: vec![Container {
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
                        volume_mounts: Some(odoo_volume_mounts()),
                        resources: instance.spec.cron.resources.clone(),
                        ..Default::default()
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
