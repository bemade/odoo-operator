//! Unit tests for webhook quantity parsing and comparison logic.
//! The webhook module's parse_quantity and compare_quantities are private,
//! so we test them indirectly via the public module-level tests in webhook.rs.
//! This file tests the CRD schema generation.

use kube::CustomResourceExt;

use odoo_operator::crd::odoo_backup_job::OdooBackupJob;
use odoo_operator::crd::odoo_init_job::OdooInitJob;
use odoo_operator::crd::odoo_instance::OdooInstance;
use odoo_operator::crd::odoo_restore_job::OdooRestoreJob;
use odoo_operator::crd::odoo_upgrade_job::OdooUpgradeJob;

#[test]
fn test_odoo_instance_crd_generates_valid_schema() {
    let crd = OdooInstance::crd();
    assert_eq!(
        crd.metadata.name.as_deref(),
        Some("odooinstances.bemade.org")
    );
    let spec = &crd.spec;
    assert_eq!(spec.group, "bemade.org");
    assert_eq!(spec.names.kind, "OdooInstance");
    assert_eq!(spec.names.plural, "odooinstances");
    assert_eq!(spec.names.short_names.as_ref().unwrap(), &["odoo"]);
    assert_eq!(spec.scope, "Namespaced");
}

#[test]
fn test_odoo_init_job_crd_generates_valid_schema() {
    let crd = OdooInitJob::crd();
    assert_eq!(
        crd.metadata.name.as_deref(),
        Some("odooinitjobs.bemade.org")
    );
    assert_eq!(crd.spec.names.kind, "OdooInitJob");
}

#[test]
fn test_odoo_backup_job_crd_generates_valid_schema() {
    let crd = OdooBackupJob::crd();
    assert_eq!(
        crd.metadata.name.as_deref(),
        Some("odoobackupjobs.bemade.org")
    );
    assert_eq!(crd.spec.names.kind, "OdooBackupJob");
}

#[test]
fn test_odoo_restore_job_crd_generates_valid_schema() {
    let crd = OdooRestoreJob::crd();
    assert_eq!(
        crd.metadata.name.as_deref(),
        Some("odoorestorejobs.bemade.org")
    );
    assert_eq!(crd.spec.names.kind, "OdooRestoreJob");
}

#[test]
fn test_odoo_upgrade_job_crd_generates_valid_schema() {
    let crd = OdooUpgradeJob::crd();
    assert_eq!(
        crd.metadata.name.as_deref(),
        Some("odooupgradejobs.bemade.org")
    );
    assert_eq!(crd.spec.names.kind, "OdooUpgradeJob");
}

#[test]
fn test_all_crds_have_v1alpha1_version() {
    for crd in [
        OdooInstance::crd(),
        OdooInitJob::crd(),
        OdooBackupJob::crd(),
        OdooRestoreJob::crd(),
        OdooUpgradeJob::crd(),
    ] {
        let versions: Vec<&str> = crd.spec.versions.iter().map(|v| v.name.as_str()).collect();
        assert!(
            versions.contains(&"v1alpha1"),
            "CRD {} missing v1alpha1 version",
            crd.spec.names.kind
        );
    }
}

#[test]
fn test_odoo_instance_crd_has_status_subresource() {
    let crd = OdooInstance::crd();
    let version = &crd.spec.versions[0];
    assert!(
        version.subresources.is_some(),
        "OdooInstance CRD should have subresources"
    );
    let subresources = version.subresources.as_ref().unwrap();
    assert!(
        subresources.status.is_some(),
        "OdooInstance CRD should have status subresource"
    );
}

#[test]
fn test_crd_yaml_serialization() {
    // Verify all CRDs can be serialized to YAML without panicking.
    let yaml = serde_yaml::to_string(&OdooInstance::crd()).unwrap();
    assert!(yaml.contains("OdooInstance"));
    assert!(yaml.contains("bemade.org"));

    let yaml = serde_yaml::to_string(&OdooInitJob::crd()).unwrap();
    assert!(yaml.contains("OdooInitJob"));
}
