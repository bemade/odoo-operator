//! `database_host_cluster` decides which PostgreSQL cluster the operator points
//! odoo.conf (and every other child resource) at.
//!
//! The interesting case is a database cluster migration: `spec.database.cluster`
//! names the destination the moment the user edits it, but the data does not
//! move until the migration Job has succeeded.  Anything that connects to the
//! destination in between lands on a database that is about to be dropped —
//! issue #172, where a cron pod's idle session blocked the migration's
//! `DROP DATABASE` and the instance rolled back.

use serde_json::{json, Value};

use odoo_operator::controller::odoo_instance::database_host_cluster;
use odoo_operator::crd::odoo_instance::OdooInstance;

/// Minimal instance with the given status; `spec.database.cluster` is the
/// desired cluster and is passed separately by the caller in reconcile.
fn instance(status: Value) -> OdooInstance {
    serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInstance",
        "metadata": { "name": "inst", "namespace": "default" },
        "spec": {
            "adminPassword": "admin",
            "replicas": 1,
            "ingress": { "hosts": ["inst.example.com"] },
        },
        "status": status,
    }))
    .unwrap()
}

#[test]
fn follows_the_spec_when_nothing_is_migrating() {
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "Running",
        "activeCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "alpha"), "alpha");
}

#[test]
fn follows_the_spec_before_the_database_exists() {
    // No database yet, so there is nothing to migrate: retargeting a
    // not-yet-initialised instance is just a configuration change.
    let inst = instance(json!({
        "dbInitialized": false,
        "phase": "Uninitialized",
        "activeCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "beta"), "beta");
}

#[test]
fn stays_on_the_source_once_the_spec_names_a_new_cluster() {
    // The user has edited spec.database.cluster but the migration has not been
    // triggered yet.  This is the reconcile that used to publish the
    // destination into odoo.conf and roll the cron pod onto it.
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "Running",
        "activeCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "beta"), "alpha");
}

#[test]
fn stays_on_the_source_while_the_migration_job_runs() {
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "MigratingDatabase",
        "activeCluster": "alpha",
        "migrationPreviousCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "beta"), "alpha");
}

#[test]
fn mid_migration_trusts_the_recorded_source_over_active_cluster() {
    // An instance migrated by an older operator can have had activeCluster
    // advanced to the destination before the data moved.  The explicitly
    // recorded source wins, so such an instance still connects to real data.
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "MigratingDatabase",
        "activeCluster": "beta",
        "migrationPreviousCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "beta"), "alpha");
}

#[test]
fn switches_to_the_destination_once_active_cluster_moves() {
    // complete_database_migration has run: the data is on beta, so the
    // ConfigMap should now be rewritten and the pods rolled onto it.
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "FinalizingDatabaseMigration",
        "activeCluster": "beta",
        "migrationPreviousCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "beta"), "beta");
}

#[test]
fn returns_to_the_source_after_a_rollback() {
    // Rollback reverts spec.database.cluster; activeCluster never moved.
    let inst = instance(json!({
        "dbInitialized": true,
        "phase": "Starting",
        "activeCluster": "alpha",
    }));
    assert_eq!(database_host_cluster(&inst, "alpha"), "alpha");
}
