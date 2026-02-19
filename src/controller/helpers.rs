//! Shared helpers for controller modules.
//!
//! These functions construct Kubernetes API objects that are reused across
//! multiple controllers (job builders, the instance controller, etc.).
//! Pure utility functions (naming, crypto, config generation) live in
//! `crate::helpers` instead.

use k8s_openapi::api::{
    batch::v1::{Job, JobSpec},
    core::v1::{
        Affinity, ConfigMapKeySelector, Container, EnvVar, EnvVarSource, LocalObjectReference,
        PodSecurityContext, PodSpec, PodTemplateSpec, Volume, VolumeMount,
    },
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::ObjectMeta;
use kube::{Resource, ResourceExt};

use crate::crd::odoo_instance::OdooInstance;

/// Field manager name used for server-side apply patches.
pub const FIELD_MANAGER: &str = "odoo-operator";

/// Build a controller OwnerReference for any kube-rs `Resource`.
///
/// This is generic over `K` — the compiler fills in `api_version()` and
/// `kind()` for whichever CRD type you pass in.  The trait bound
/// `K: Resource<DynamicType = ()>` means "any type whose Kubernetes
/// metadata is known at compile time", which is true for every struct
/// that derives `CustomResource`.
pub fn controller_owner_ref<K: Resource<DynamicType = ()>>(obj: &K) -> OwnerReference {
    OwnerReference {
        api_version: K::api_version(&()).to_string(),
        kind: K::kind(&()).to_string(),
        name: obj.name_any(),
        uid: obj.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Standard Odoo pod security context (uid 100 / gid 101).
///
/// Every Odoo container and job pod in this operator runs with the same
/// non-root identity matching the official Odoo Docker image.
pub fn odoo_security_context() -> PodSecurityContext {
    PodSecurityContext {
        run_as_user: Some(100),
        run_as_group: Some(101),
        fs_group: Some(101),
        ..Default::default()
    }
}

/// Standard volumes shared by the instance Deployment and every job pod:
/// the filestore PVC and the odoo-conf ConfigMap.
pub fn odoo_volumes(instance_name: &str) -> Vec<Volume> {
    vec![
        Volume {
            name: "filestore".to_string(),
            persistent_volume_claim: Some(
                k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: format!("{instance_name}-filestore-pvc"),
                    ..Default::default()
                },
            ),
            ..Default::default()
        },
        Volume {
            name: "odoo-conf".to_string(),
            config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                name: format!("{instance_name}-odoo-conf"),
                ..Default::default()
            }),
            ..Default::default()
        },
    ]
}

/// Standard volume mounts matching [`odoo_volumes`].
pub fn odoo_volume_mounts() -> Vec<VolumeMount> {
    vec![
        VolumeMount {
            name: "filestore".to_string(),
            mount_path: "/var/lib/odoo".to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "odoo-conf".to_string(),
            mount_path: "/etc/odoo".to_string(),
            ..Default::default()
        },
    ]
}

/// Build the `imagePullSecrets` list from an OdooInstance spec.
/// Returns `None` when no pull secret is configured (which omits the
/// field from the serialised JSON, matching the K8s convention).
pub fn image_pull_secrets(instance: &OdooInstance) -> Option<Vec<LocalObjectReference>> {
    instance
        .spec
        .image_pull_secret
        .as_ref()
        .map(|name| vec![LocalObjectReference { name: name.clone() }])
}

/// Shorthand for a plain-value `EnvVar`.
pub fn env(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// Build an `EnvVar` that reads its value from a ConfigMap key.
pub fn cm_env(env_name: &str, cm_name: &str, key: &str) -> EnvVar {
    EnvVar {
        name: env_name.into(),
        value_from: Some(EnvVarSource {
            config_map_key_ref: Some(ConfigMapKeySelector {
                name: cm_name.into(),
                key: key.into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Get the cron deployment name for an odoo instance
pub fn cron_depl_name(instance: &OdooInstance) -> String {
    instance.name_any().to_string() + "-cron"
}

// ── OdooJobBuilder ──────────────────────────────────────────────────────────

/// Builder for batch/v1 `Job` resources used by the operator's job controllers.
///
/// Encapsulates the boilerplate that every job shares (metadata, backoff policy,
/// TTL, security context, standard volumes) and lets callers specify only what
/// differs: containers, init containers, extra volumes, deadlines, and affinity.
///
/// # Example
///
/// ```ignore
/// let job = OdooJobBuilder::new("my-init-", ns, &init_job_cr, &instance)
///     .containers(vec![my_container])
///     .build();
/// ```
pub struct OdooJobBuilder {
    generate_name: String,
    namespace: String,
    owner_ref: OwnerReference,
    pull_secrets: Option<Vec<LocalObjectReference>>,
    volumes: Vec<Volume>,
    containers: Vec<Container>,
    init_containers: Option<Vec<Container>>,
    active_deadline: Option<i64>,
    affinity: Option<Affinity>,
}

impl OdooJobBuilder {
    /// Create a new builder with the standard Odoo job defaults.
    ///
    /// - `generate_name_prefix`: the `metadata.generateName` value (e.g. `"my-init-"`)
    /// - `ns`: namespace for the Job
    /// - `owner`: the CRD resource that owns this Job (sets the controller owner reference)
    /// - `instance`: the OdooInstance (used for pull secrets and standard volumes)
    pub fn new<K: Resource<DynamicType = ()>>(
        generate_name_prefix: &str,
        ns: &str,
        owner: &K,
        instance: &OdooInstance,
    ) -> Self {
        let instance_name = instance.name_any();
        Self {
            generate_name: generate_name_prefix.to_string(),
            namespace: ns.to_string(),
            owner_ref: controller_owner_ref(owner),
            pull_secrets: image_pull_secrets(instance),
            volumes: odoo_volumes(&instance_name),
            containers: vec![],
            init_containers: None,
            active_deadline: None,
            affinity: None,
        }
    }

    /// Set the main containers for the Job pod.
    pub fn containers(mut self, containers: Vec<Container>) -> Self {
        self.containers = containers;
        self
    }

    /// Set init containers for the Job pod.
    pub fn init_containers(mut self, init_containers: Vec<Container>) -> Self {
        self.init_containers = if init_containers.is_empty() {
            None
        } else {
            Some(init_containers)
        };
        self
    }

    /// Append extra volumes beyond the standard filestore + odoo-conf.
    pub fn extra_volumes(mut self, extra: Vec<Volume>) -> Self {
        self.volumes.extend(extra);
        self
    }

    /// Set `spec.activeDeadlineSeconds` on the Job.
    pub fn active_deadline(mut self, seconds: i64) -> Self {
        self.active_deadline = Some(seconds);
        self
    }

    /// Set pod affinity (e.g. co-locate with the instance deployment).
    pub fn affinity(mut self, affinity: Affinity) -> Self {
        self.affinity = Some(affinity);
        self
    }

    /// Consume the builder and produce a `batch/v1 Job`.
    pub fn build(self) -> Job {
        Job {
            metadata: ObjectMeta {
                generate_name: Some(self.generate_name),
                namespace: Some(self.namespace),
                owner_references: Some(vec![self.owner_ref]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                backoff_limit: Some(0),
                ttl_seconds_after_finished: Some(900),
                active_deadline_seconds: self.active_deadline,
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        restart_policy: Some("Never".to_string()),
                        image_pull_secrets: self.pull_secrets,
                        security_context: Some(odoo_security_context()),
                        affinity: self.affinity,
                        volumes: Some(self.volumes),
                        init_containers: self.init_containers,
                        containers: self.containers,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
