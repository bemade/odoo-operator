//! Integration tests for database cluster migration.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, ListParams, Patch, PatchParams, PostParams};
use serde_json::json;
use std::time::Duration;

use odoo_operator::controller::helpers::FIELD_MANAGER;
use odoo_operator::crd::odoo_instance::{OdooInstance, OdooInstancePhase};

use crate::common::*;

/// Add a second cluster to the postgres-clusters Secret so migration has a target.
async fn ensure_secondary_cluster(client: &kube::Client) {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), "default");
    let yaml = serde_yaml::to_string(&json!({
        "default": {
            "host": "localhost",
            "port": 5432,
            "adminUser": "postgres",
            "adminPassword": "postgres",
            "default": true
        },
        "secondary": {
            "host": "secondary.local",
            "port": 5432,
            "adminUser": "postgres",
            "adminPassword": "postgres",
            "default": false
        }
    }))
    .unwrap();
    let patch = json!({
        "stringData": { "clusters.yaml": yaml }
    });
    secrets
        .patch(
            "postgres-clusters",
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Merge(&patch),
        )
        .await
        .unwrap();
}

/// Wait for a batch/v1 Job to appear with a name matching the given prefix.
async fn wait_for_db_migration_job(client: &kube::Client, ns: &str, prefix: &str) -> String {
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    assert!(
        wait_for(TIMEOUT, POLL, || {
            let jobs = jobs.clone();
            let pfx = prefix.to_string();
            async move {
                if let Ok(list) = jobs.list(&ListParams::default()).await {
                    for job in list.items {
                        if let Some(ref n) = job.metadata.name {
                            if n.starts_with(&pfx) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
        })
        .await,
        "migration job with prefix {prefix} never appeared"
    );
    let list = jobs.list(&ListParams::default()).await.unwrap();
    list.items
        .iter()
        .find_map(|j| {
            j.metadata
                .name
                .as_ref()
                .filter(|n| n.starts_with(prefix))
                .cloned()
        })
        .unwrap()
}

// ─── Test 1: Happy path from Running ────────────────────────────────────────

#[tokio::test]
async fn migrate_database_from_running() {
    let name = "test-dbmig-run";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    ensure_secondary_cluster(c).await;

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    // Verify activeCluster is set after reaching Running.
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get_status(name).await.unwrap();
    let active_cluster = inst
        .status
        .as_ref()
        .and_then(|s| s.active_cluster.as_deref());
    assert_eq!(
        active_cluster,
        Some("default"),
        "activeCluster should be set to 'default'"
    );

    // Change database.cluster to trigger migration.
    patch_instance_spec(c, ns, name, json!({"database": {"cluster": "secondary"}})).await;

    // Should transition to MigratingDatabase.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingDatabase).await,
        "expected MigratingDatabase phase"
    );

    // Fake deployments scaled down.
    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;

    // Fake migration job success.
    let job_name = wait_for_db_migration_job(c, ns, &format!("{name}-migrate-db-")).await;
    fake_job_succeeded(c, ns, &job_name).await;

    // Should transition through FinalizingDatabaseMigration → Starting.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting phase after migration"
    );

    // Verify activeCluster was updated.
    let inst = api.get_status(name).await.unwrap();
    let active_cluster = inst
        .status
        .as_ref()
        .and_then(|s| s.active_cluster.as_deref());
    assert_eq!(
        active_cluster,
        Some("secondary"),
        "activeCluster should be updated to 'secondary'"
    );

    // Verify migration status fields are cleared.
    let status = inst.status.as_ref().unwrap();
    assert!(
        status.db_migration_job_name.is_none(),
        "dbMigrationJobName should be cleared"
    );
    assert!(
        status.migration_previous_cluster.is_none(),
        "migrationPreviousCluster should be cleared"
    );

    // Scale up to reach Running.
    fake_deployment_ready(c, ns, name, 1).await;
    let _handle = keep_deployment_ready(c.clone(), ns.to_string(), name.to_string(), 1);
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Running).await,
        "expected Running phase"
    );
}

// ─── Test 2: Migration from Stopped ─────────────────────────────────────────

#[tokio::test]
async fn migrate_database_from_stopped() {
    let name = "test-dbmig-stop";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    ensure_secondary_cluster(c).await;

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    // Scale to 0 → Stopped.
    patch_instance_spec(c, ns, name, json!({"replicas": 0})).await;
    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Stopped).await,
        "expected Stopped"
    );

    // Trigger migration.
    patch_instance_spec(c, ns, name, json!({"database": {"cluster": "secondary"}})).await;

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingDatabase).await,
        "expected MigratingDatabase"
    );

    // Fake job success.
    let job_name = wait_for_db_migration_job(c, ns, &format!("{name}-migrate-db-")).await;
    fake_job_succeeded(c, ns, &job_name).await;

    // With replicas=0, should end up Stopped.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Stopped).await,
        "expected Stopped after migration with replicas=0"
    );
}

// ─── Test 3: Job failure → rollback ─────────────────────────────────────────

#[tokio::test]
async fn migrate_database_job_failure_rollback() {
    let name = "test-dbmig-fail";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    ensure_secondary_cluster(c).await;

    let ready_handle = fast_track_to_running(&ctx, &format!("{name}-init")).await;
    ready_handle.abort();

    patch_instance_spec(c, ns, name, json!({"database": {"cluster": "secondary"}})).await;

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::MigratingDatabase).await,
        "expected MigratingDatabase"
    );

    fake_deployment_ready(c, ns, name, 0).await;
    fake_deployment_ready(c, ns, &format!("{name}-cron"), 0).await;

    // Fake job failure.
    let job_name = wait_for_db_migration_job(c, ns, &format!("{name}-migrate-db-")).await;
    fake_job_failed(c, ns, &job_name).await;

    // Rollback should revert spec.database.cluster and transition to Starting.
    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Starting).await,
        "expected Starting after rollback"
    );

    // Verify cluster was reverted.
    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get(name).await.unwrap();
    let cluster = inst
        .spec
        .database
        .as_ref()
        .and_then(|d| d.cluster.as_deref());
    assert_eq!(
        cluster,
        Some("default"),
        "database.cluster should be reverted to 'default'"
    );
}

// ─── Test 4: Not triggered during init ──────────────────────────────────────

#[tokio::test]
async fn migrate_database_not_triggered_during_init() {
    let name = "test-dbmig-noinit";
    let ctx = TestContext::new(name).await;
    let (c, ns) = (&ctx.client, ctx.ns.as_str());

    ensure_secondary_cluster(c).await;

    // Create an init job to get into Initializing.
    let init_api: Api<odoo_operator::crd::odoo_init_job::OdooInitJob> =
        Api::namespaced(c.clone(), ns);
    let init_job: odoo_operator::crd::odoo_init_job::OdooInitJob = serde_json::from_value(json!({
        "apiVersion": "bemade.org/v1alpha1",
        "kind": "OdooInitJob",
        "metadata": { "name": format!("{name}-init"), "namespace": ns },
        "spec": { "odooInstanceRef": { "name": name } }
    }))
    .unwrap();
    init_api
        .create(&PostParams::default(), &init_job)
        .await
        .unwrap();

    assert!(
        wait_for_phase(c, ns, name, OdooInstancePhase::Initializing).await,
        "expected Initializing"
    );

    // Change cluster while initializing.
    patch_instance_spec(c, ns, name, json!({"database": {"cluster": "secondary"}})).await;

    // Wait a bit and verify migration was NOT triggered.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let api: Api<OdooInstance> = Api::namespaced(c.clone(), ns);
    let inst = api.get_status(name).await.unwrap();
    let phase = inst.status.as_ref().and_then(|s| s.phase.as_ref());
    assert_ne!(
        phase,
        Some(&OdooInstancePhase::MigratingDatabase),
        "migration should not trigger during Initializing"
    );
}
