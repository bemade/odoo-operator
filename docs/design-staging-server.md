# Design proposal — staging server from production snapshot

Proposal for #75.  **Not implementation** — this document is here to drive
the design conversation before any CRD/operator code lands.

## Problem

Spinning up a staging Odoo instance populated with current production data
is today a multi-step, error-prone workflow:

1. Trigger an ad-hoc `OdooBackupJob` on the production instance (or use the
   most recent scheduled backup).
2. Upload or locate the resulting zip on S3.
3. Create a fresh `OdooInstance` CR for the staging environment.
4. Submit an `OdooRestoreJob` pointing at that zip with `neutralize: true`.
5. Wait for it to complete; hope neutralization left no active mail servers.
6. To refresh the staging data later, repeat steps 1-5 by hand.

This is the same pattern Odoo.sh exposes as a one-click "create staging from
production" button.  We can offer it as a first-class CRD-driven flow,
building on top of the hardened restore pipeline from #76.

## Goals (from the issue)

1. An init flow that copies production DB + filestore **efficiently**.
2. Use a **storage snapshot** for the filestore where the storage class
   supports it, rather than round-tripping through S3.
3. Allow refresh from production via existing `OdooRestoreJob` (with new
   fields) or a new CRD — whichever is cleaner.
4. Neutralization + mail-server verification is mandatory on every
   creation and every refresh.  The restore hardening PR #76 already
   enforces (4); this feature inherits it for free.

## Non-goals (for v1)

- Logical replication / CDC against production.
- Continuous or scheduled auto-refresh (can be layered on later via
  `OdooBackupJob` scheduling + a new `OdooStagingRefreshJob`).
- Cross-cluster staging (production and staging live in the same K8s
  cluster; cross-cluster is a v2 concern).
- Module-selective staging (staging gets the full DB, full filestore).

## Two possible approaches

### Option A — snapshot-and-clone (preferred)

Leverage Kubernetes CSI `VolumeSnapshot` + `VolumeSnapshotContent` and
CloudNativePG's `Backup` / `Recovery` primitives.

**Filestore:**
1. Operator creates a `VolumeSnapshot` of the production instance's
   filestore PVC.
2. Operator creates the staging PVC with `dataSource` pointing at the
   snapshot (CSI will clone the data at the block / copy-on-write level).
3. Staging pods mount the cloned PVC directly — no data transfer through
   the operator.

**Database:**
1. Operator creates a CNPG `Backup` on the production cluster (or reuses
   the latest scheduled one).
2. Operator creates a fresh CNPG `Cluster` for staging with
   `bootstrap.recovery.backup.name` pointing at that `Backup`.
3. CNPG restores the cluster-level backup directly into the staging
   cluster — no intermediate pg_dump/restore round trip.
4. Once the staging DB is recovered, the operator runs `odoo neutralize`
   and the mail-server verification from the restore.sh pipeline.

**Pros:**
- Minutes instead of hours for large databases; constant time at the
  filestore level regardless of filestore size.
- Zero S3 egress.
- Uses the same neutralize/mail-verify path as `OdooRestoreJob`, so the
  safety guarantees from #76 apply unchanged.

**Cons:**
- Only works when both production and staging use a snapshot-capable
  storage class *and* the same CNPG cluster topology.  We'd fall back
  to Option B when those preconditions aren't met.
- Adds a runtime dependency on `snapshot.storage.k8s.io` CRDs.

### Option B — pg_dump + filestore sidecar rsync (fallback)

Reuse today's `OdooBackupJob` → zip → `OdooRestoreJob` pipeline but have
the operator orchestrate it end-to-end from a single staging-creation
intent.

**Pros:**
- Works anywhere — no CSI snapshot requirement.
- Reuses existing, hardened code paths from #76.

**Cons:**
- Still slow for large datasets.
- Still goes through S3 (or a mounted shared volume) as an intermediate.

### Recommendation

Implement A as the primary path with automatic fallback to B when
preconditions (VolumeSnapshotClass exists for the storage class, CNPG
is the database cluster) aren't met.  B is essentially free given #76 —
it's today's flow wrapped in a single CR.

## Proposed surface

### New `OdooInstance` spec field

```yaml
apiVersion: bemade.org/v1alpha1
kind: OdooInstance
metadata:
  name: staging-for-prod
  namespace: staging
spec:
  cloneFrom:                     # NEW
    instanceName: prod
    instanceNamespace: production  # optional; defaults to .metadata.namespace
    initialRefresh: true         # if true, operator snapshots and clones
                                 # immediately on creation (= init flow).
                                 # If false, staging starts empty until
                                 # an OdooStagingRefreshJob is submitted.
    method: auto                 # auto | snapshot | backup (A auto-falls-back
                                 # to B when snapshot preconditions fail).
  # ... other spec fields unchanged ...
```

When `cloneFrom` is set AND `status.dbInitialized` is false, the
`Uninitialized` state auto-creates an `OdooStagingRefreshJob` instead of
the normal `OdooInitJob` (which assumes a fresh empty DB).

### New `OdooStagingRefreshJob` CRD

```yaml
apiVersion: bemade.org/v1alpha1
kind: OdooStagingRefreshJob
metadata:
  name: staging-for-prod-refresh
  namespace: staging
spec:
  odooInstanceRef:
    name: staging-for-prod
  method: auto                   # override auto-selected method
  skipFilestore: false           # rare: DB-only refresh
  webhook:                       # reuses the pattern from existing job CRDs
    url: ...
status:
  phase: Pending | Snapshotting | Cloning | Restoring | Neutralizing | Completed | Failed
  snapshotName: ...              # VolumeSnapshot reference
  sourceBackupName: ...          # CNPG Backup reference
  jobName: ...                   # underlying batch/v1 Job for neutralize
  startTime: ...
  completionTime: ...
  message: ...
```

### State-machine additions

New phase: `CloningFromSource`.  Lifecycle while in it:
- Web + cron scaled to 0 (same as `Initializing`)
- Operator drives the snapshot → clone → recover → neutralize sequence
- Transition to `Starting` on success, to `InitFailed` on failure

Guards/actions follow the existing `Initializing` patterns.

### Webhook validation additions

`cloneFrom` is mutable only before `status.dbInitialized=true` (can't
retroactively turn a regular instance into a clone of something else).
Attempting to set a different `cloneFrom.instanceName` after creation is
rejected by the validating webhook.

Refuse creation when the referenced production instance doesn't exist or
isn't in `Running` phase at the moment of snapshot — catches typos early.

## Interaction with #76 (restore hardening)

This feature rides on top of #76's guarantees for free:
- The post-clone neutralize step goes through `restore.sh`'s pipeline
  (reinit params → `odoo neutralize` → verify neutralization flag →
  verify no active mail servers)
- A neutralize/mail-check failure drops the staging DB + sends the
  instance to `Uninitialized` (so staging never comes up with active
  production mail servers)

## Open questions for Marc

1. **Same-cluster only for v1?**  Snapshot-based cloning realistically
   requires both instances in the same cluster because `VolumeSnapshot`
   is namespaced + CSI-bound.  OK as a v1 constraint?
2. **New CRD vs. extending `OdooRestoreJob`?**  I lean toward a new CRD
   (`OdooStagingRefreshJob`) because the semantics differ meaningfully:
   the source is a *live instance* (not an artifact), and the lifecycle
   includes a snapshot step that the restore path doesn't have.  Conflating
   them would complicate the existing `OdooRestoreJob` which is now clean.
   Counter-argument: fewer CRDs is good.
3. **Filestore snapshot reclaim policy.**  After a successful clone, do
   we keep the `VolumeSnapshot` around (useful for a fast re-clone if the
   staging DB gets mangled during testing) or delete it immediately to
   save storage?  Proposing: keep for 24h then GC unless user pins it.
4. **Refresh semantics.**  When a refresh runs against an already-populated
   staging instance: do we drop-and-reclone (fast, data loss) or attempt
   a diff-and-apply (impossible in practice for Odoo)?  Proposing:
   always drop-and-reclone; warn loudly in the CR description.
5. **CNPG dependency for the DB path.**  Should we make the snapshot-based
   DB path CNPG-only for v1 and fall back to pg_dump for non-CNPG
   clusters?  Same Option A/B split, just for the DB.

## Implementation phases (if approved)

### Phase 1 — Option B wrapper (1-2 days)
- New `OdooStagingRefreshJob` CRD (and associated RBAC)
- Controller logic that creates an `OdooBackupJob` on source,
  waits for completion, creates an `OdooRestoreJob` on target, waits
  for completion, marks staging ready
- Covers acceptance criterion 1 + inherits 4 from #76
- No snapshot code yet — slow but correct

### Phase 2 — Option A filestore snapshot (2-3 days)
- Detect snapshot-capable storage class via `VolumeSnapshotClass` list
- Implement `VolumeSnapshot` create + wait-for-ready + clone-into-PVC
- Fallback to B if preconditions not met
- Covers acceptance criterion 2

### Phase 3 — Option A DB backup/recovery (2-3 days)
- Integrate with CNPG `Backup` / bootstrap-recovery
- Fallback to B if not CNPG
- Completes the optimized path

### Phase 4 — Staging-aware spec field + auto-init (1 day)
- `OdooInstance.spec.cloneFrom` field + webhook validation
- Auto-create `OdooStagingRefreshJob` on first reconcile when set
- Covers acceptance criterion 3

Total ~7-10 days of focused work.  Phase 1 alone already closes the
issue's minimum bar.
