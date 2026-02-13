//! Generate CRD YAML from Rust types.
//!
//! Usage:
//!   cargo run --bin crdgen              # all CRDs to stdout
//!   cargo run --bin crdgen -- --out-dir ./crds   # one file per CRD

use kube::CustomResourceExt;
use odoo_operator::crd::{
    odoo_backup_job::OdooBackupJob, odoo_init_job::OdooInitJob, odoo_instance::OdooInstance,
    odoo_restore_job::OdooRestoreJob, odoo_upgrade_job::OdooUpgradeJob,
};
use std::path::PathBuf;

fn main() {
    let out_dir: Option<PathBuf> = std::env::args()
        .skip_while(|a| a != "--out-dir")
        .nth(1)
        .map(PathBuf::from);

    let crds = vec![
        (
            "odoo-crd.yaml",
            serde_yaml::to_string(&OdooInstance::crd()).unwrap(),
        ),
        (
            "initjob-crd.yaml",
            serde_yaml::to_string(&OdooInitJob::crd()).unwrap(),
        ),
        (
            "backupjob-crd.yaml",
            serde_yaml::to_string(&OdooBackupJob::crd()).unwrap(),
        ),
        (
            "restorejob-crd.yaml",
            serde_yaml::to_string(&OdooRestoreJob::crd()).unwrap(),
        ),
        (
            "upgradejob-crd.yaml",
            serde_yaml::to_string(&OdooUpgradeJob::crd()).unwrap(),
        ),
    ];

    match out_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir).expect("failed to create output directory");
            for (name, yaml) in &crds {
                let path = dir.join(name);
                std::fs::write(&path, format!("---\n{yaml}"))
                    .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
                eprintln!("wrote {}", path.display());
            }
        }
        None => {
            for (_name, yaml) in &crds {
                println!("---\n{yaml}");
            }
        }
    }
}
