# OdooInstance State Machine

Auto-generated from the `TRANSITIONS` table in `controller/state_machine.rs`.

```mermaid
stateDiagram-v2
    [*] --> Provisioning

    Provisioning --> Uninitialized : [!db_initialized]
    Provisioning --> Starting : [db_initialized]

    Uninitialized --> Initializing : [init_job present]
    Uninitialized --> Restoring : [restore_job present]
    Uninitialized --> CloningFromSource : [refresh_job present]

    Initializing --> Starting : [init_job succeeded] / CompleteInitJob, MarkDbInitialized
    Initializing --> InitFailed : [init_job failed] / FailInitJob
    Initializing --> Uninitialized : [init_job absent]

    InitFailed --> Initializing : [init_job present]
    InitFailed --> Restoring : [restore_job present]
    InitFailed --> CloningFromSource : [refresh_job present]

    Starting --> Stopped : [replicas == 0]
    Starting --> Restoring : [restore_job present]
    Starting --> CloningFromSource : [refresh_job present]
    Starting --> Upgrading : [upgrade_job ready]
    Starting --> BackingUp : [backup_job present]
    Starting --> Running : [ready >= desired]

    Running --> MigratingFilestore : [storage_class_mismatch] / BeginFilestoreMigration
    Running --> MigratingDatabase : [cluster_mismatch] / BeginDatabaseMigration
    Running --> Stopped : [replicas == 0]
    Running --> Restoring : [restore_job present]
    Running --> CloningFromSource : [refresh_job present]
    Running --> Upgrading : [upgrade_job ready]
    Running --> BackingUp : [backup_job present]
    Running --> Degraded : [ready < desired && ready > 0]
    Running --> Starting : [ready == 0]

    Degraded --> MigratingFilestore : [storage_class_mismatch] / BeginFilestoreMigration
    Degraded --> MigratingDatabase : [cluster_mismatch] / BeginDatabaseMigration
    Degraded --> Stopped : [replicas == 0]
    Degraded --> Restoring : [restore_job present]
    Degraded --> CloningFromSource : [refresh_job present]
    Degraded --> Upgrading : [upgrade_job ready]
    Degraded --> BackingUp : [backup_job present]
    Degraded --> Running : [ready >= desired]
    Degraded --> Starting : [ready == 0]

    BackingUp --> BackingUp : [backup succeeded && another pending] / CompleteBackupJob
    BackingUp --> BackingUp : [backup failed && another pending] / FailBackupJob
    BackingUp --> Stopped : [backup succeeded && replicas == 0] / CompleteBackupJob
    BackingUp --> Stopped : [backup failed && replicas == 0] / FailBackupJob
    BackingUp --> Running : [backup succeeded && ready >= desired] / CompleteBackupJob
    BackingUp --> Running : [backup failed && ready >= desired] / FailBackupJob
    BackingUp --> Degraded : [backup succeeded && 0 < ready < desired] / CompleteBackupJob
    BackingUp --> Degraded : [backup failed && 0 < ready < desired] / FailBackupJob
    BackingUp --> Starting : [backup succeeded && ready == 0] / CompleteBackupJob
    BackingUp --> Starting : [backup failed && ready == 0] / FailBackupJob
    BackingUp --> Running : [backup absent && ready >= desired]
    BackingUp --> Degraded : [backup absent && 0 < ready < desired]
    BackingUp --> Starting : [backup absent && ready == 0]
    BackingUp --> Stopped : [backup absent && replicas == 0]

    CloningFromSource --> Starting : [refresh_job succeeded] / CompleteRefreshJob, MarkDbInitialized
    CloningFromSource --> InitFailed : [refresh_job failed] / FailRefreshJob
    CloningFromSource --> Uninitialized : [refresh_job absent]

    Upgrading --> Starting : [upgrade_job succeeded] / CompleteUpgradeJob
    Upgrading --> Starting : [upgrade_job failed] / FailUpgradeJob
    Upgrading --> Starting : [upgrade_job absent]

    Restoring --> Starting : [restore_job succeeded] / CompleteRestoreJob, MarkDbInitialized
    Restoring --> Uninitialized : [restore_job failed] / FailRestoreJob, MarkDbUninitialized
    Restoring --> Starting : [restore_job absent]

    Stopped --> MigratingFilestore : [storage_class_mismatch] / BeginFilestoreMigration
    Stopped --> MigratingDatabase : [cluster_mismatch] / BeginDatabaseMigration
    Stopped --> Restoring : [restore_job present]
    Stopped --> CloningFromSource : [refresh_job present]
    Stopped --> Upgrading : [upgrade_job ready]
    Stopped --> Starting : [replicas > 0]

    MigratingFilestore --> FinalizingFilestoreMigration : [migration_job succeeded] / CompleteFilestoreMigration
    MigratingFilestore --> Starting : [migration_job failed/absent && replicas > 0] / RollbackFilestoreMigration
    MigratingFilestore --> Stopped : [migration_job failed/absent && replicas == 0] / RollbackFilestoreMigration

    FinalizingFilestoreMigration --> Starting : [pvc rebound && replicas > 0] / ClearFilestoreMigrationStatus
    FinalizingFilestoreMigration --> Stopped : [pvc rebound && replicas == 0] / ClearFilestoreMigrationStatus

    MigratingDatabase --> FinalizingDatabaseMigration : [db_migration_job succeeded] / CompleteDatabaseMigration
    MigratingDatabase --> Starting : [db_migration_job failed/absent && replicas > 0] / RollbackDatabaseMigration
    MigratingDatabase --> Stopped : [db_migration_job failed/absent && replicas == 0] / RollbackDatabaseMigration

    FinalizingDatabaseMigration --> Starting : [cluster switched && replicas > 0] / ClearDatabaseMigrationStatus
    FinalizingDatabaseMigration --> Stopped : [cluster switched && replicas == 0] / ClearDatabaseMigrationStatus

    Error --> Starting : [db_initialized]
    Error --> Uninitialized : [!db_initialized]
```
