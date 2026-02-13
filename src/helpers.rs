use std::collections::BTreeMap;

use chrono::Utc;
use k8s_openapi::api::core::v1::{Affinity, ResourceRequirements, Toleration};
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::crd::odoo_instance::OdooInstance;

/// Return the current UTC time formatted as Odoo expects: `"YYYY-MM-DD HH:MM:SS"`.
/// This matches the Go operator's `time.UTC().Format("2006-01-02 15:04:05")`.
pub fn utc_now_odoo() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ── Operator defaults (injected via CLI flags / env) ──────────────────────────

/// Cluster-specific configuration injected at startup via CLI flags.
/// Written into OdooInstance spec fields on first reconcile.
#[derive(Clone, Debug, Default)]
pub struct OperatorDefaults {
    pub odoo_image: String,
    pub storage_class: String,
    pub storage_size: String,
    pub ingress_class: String,
    pub ingress_issuer: String,
    pub resources: Option<ResourceRequirements>,
    pub affinity: Option<Affinity>,
    pub tolerations: Vec<Toleration>,
}

// ── Naming helpers ────────────────────────────────────────────────────────────

/// Derive the PostgreSQL username from namespace + instance name.
pub fn odoo_username(namespace: &str, name: &str) -> String {
    format!("odoo.{namespace}.{name}")
}

/// Convert a UUID string into a safe database name component by replacing
/// any non-lowercase-alphanumeric characters with underscores.
pub fn sanitise_uid(uid: &str) -> String {
    uid.chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Derive the database name from the instance UID.
pub fn db_name(instance: &OdooInstance) -> String {
    let uid = instance.metadata.uid.as_deref().unwrap_or("unknown");
    format!("odoo_{}", sanitise_uid(uid))
}

/// Generate a cryptographically random 48-hex-char password.
pub fn generate_password() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill(&mut bytes);
    hex::encode(bytes)
}

/// SHA-256 hash of a string, returned as hex.
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── odoo.conf generation ──────────────────────────────────────────────────────

/// Build the content of odoo.conf.
pub fn build_odoo_conf(
    username: &str,
    password: &str,
    admin_password: &str,
    db_host: &str,
    db_port: i32,
    db_name: &str,
    extra: &Option<BTreeMap<String, String>>,
) -> String {
    let mut options = BTreeMap::new();
    options.insert("data_dir", "/var/lib/odoo".to_string());
    options.insert("logfile", String::new());
    options.insert("log_level", "info".to_string());
    options.insert("proxy_mode", "True".to_string());
    options.insert("addons_path", "/mnt/extra-addons".to_string());
    options.insert("db_host", db_host.to_string());
    options.insert("db_port", db_port.to_string());
    options.insert("db_name", db_name.to_string());
    options.insert("db_user", username.to_string());
    options.insert("db_password", password.to_string());
    options.insert("list_db", "False".to_string());
    options.insert("http_interface", "0.0.0.0".to_string());
    options.insert("http_port", "8069".to_string());

    if !admin_password.is_empty() {
        options.insert("admin_passwd", admin_password.to_string());
    }

    if let Some(extra) = extra {
        for (k, v) in extra {
            options.insert(k.as_str(), v.clone());
        }
    }

    // Prepend standard Odoo Docker image addon paths.
    let std_addons = "/opt/odoo/addons,/opt/odoo/odoo/addons";
    let ap = options.get("addons_path").cloned().unwrap_or_default();
    if ap.is_empty() {
        options.insert("addons_path", std_addons.to_string());
    } else {
        options.insert("addons_path", format!("{std_addons},{ap}"));
    }

    // Write standard keys in a stable order, then remaining sorted.
    let standard_keys = [
        "data_dir",
        "logfile",
        "log_level",
        "proxy_mode",
        "addons_path",
        "db_host",
        "db_port",
        "db_name",
        "db_user",
        "db_password",
        "list_db",
        "http_interface",
        "http_port",
        "admin_passwd",
    ];

    let mut out = String::from("[options]\n");
    let mut written = std::collections::HashSet::new();

    for &key in &standard_keys {
        if let Some(val) = options.get(key) {
            out.push_str(&format!("{key} = {val}\n"));
            written.insert(key);
        }
    }

    // BTreeMap is already sorted, so remaining keys come out in order.
    for (key, val) in &options {
        if !written.contains(&key[..]) {
            out.push_str(&format!("{key} = {val}\n"));
        }
    }

    out
}
