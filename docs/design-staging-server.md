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
6. To refresh the staging data later, repeat at least steps 1, 2, 4 and 5 by hand.

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

The filestore side is one question (block-level snapshot vs file copy)
and the database side is `pg_dump | pg_restore` streamed through a
single Job pod.  The two run **in parallel**: the operator spawns a
filestore Job and a database Job concurrently and waits for both to
succeed before the neutralize step runs.  Wall-clock time is
max(filestore, db) rather than their sum.

### Filestore options

**F1 — CSI `VolumeSnapshot` + clone (preferred when supported):**
1. Operator creates a `VolumeSnapshot` of the production instance's
   filestore PVC.
2. Operator creates the staging PVC with `dataSource` pointing at the
   snapshot (CSI will clone the data at the block / copy-on-write level).
3. Staging pods mount the cloned PVC directly — no data transfer through
   the operator.

Requires a `VolumeSnapshotClass` for the filestore's storage class.
Detectable at reconcile time; we auto-fall back to F2 when absent.

**F2 — pod-to-pod rsync / copy (fallback):**
A short-lived Job mounts both PVCs (or streams over a service) and
copies.  Identical to today's filestore handling inside
`OdooRestoreJob` + `restore.sh`.  Slower but works anywhere.

### Database: streamed `pg_dump | pg_restore`

A Job pod runs `pg_dump -h <source-service> -Fc | pg_restore -h
<staging-service> --jobs=<cpus>` end-to-end in a single process.
MVCC snapshot on the source so production writes keep flowing; no
staging artifact; no intermediate storage; `--jobs` parallelizes
index rebuilds on the target.  Works against any Postgres (decoupled
from the cluster manager) and across clusters within the same
Postgres reachability domain.

Then runs the standard neutralize + mail-server verification from
`restore.sh`.

**Alternatives considered and rejected:**

- **CNPG `Backup` / `Recovery`** — filesystem-level physical copy
  is 2-3× faster for 100 GB+ DBs with heavy indexes.  Rejected
  because (a) typical Odoo DBs sit in the 1-50 GB range where
  `pg_restore --jobs` is fast enough, (b) it would pull CNPG into
  the data path we otherwise keep decoupled, (c) it adds backup
  artifact lifecycle management and object-storage credentials for
  bootstrap.  If someone ever hits a DB large enough to care, this
  can be added later as an opt-in method without reshaping the CRD.

- **`CREATE DATABASE ... WITH TEMPLATE`** — fastest possible
  same-cluster copy (server-side file copy in one SQL statement).
  Rejected because Postgres requires *no other connections* to the
  template for the duration — for any live production DB this
  means scaling web + cron to 0 on prod.  Viable for small
  demo/fixture scenarios, not for staging-from-prod.

### Recommendation

Default composition: F1 + D1 (auto-falls-back to F2 when no
`VolumeSnapshotClass`).  Users on CNPG with big DBs can opt into
D2.  The snapshot and `pg_dump`-streaming paths are independent,
so we can ship F1+D1 first and add D2 later without reshaping the
CRD.

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
    filestoreMethod: auto        # auto | snapshot | copy
                                 # "auto" picks snapshot, falls back
                                 # to copy when no VolumeSnapshotClass.
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
  filestoreMethod: auto          # auto | snapshot | copy
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
- Two Jobs run in parallel:
  - **DB Job:** streams `pg_dump | psql` (D1) or orchestrates CNPG
    recovery (D2) into the staging DB
  - **Filestore Job:** creates a CSI clone from a VolumeSnapshot
    (F1) or runs an in-pod rsync between the two PVCs (F2)
- Only after **both** Jobs report Succeeded does the operator run a
  third Job for `odoo neutralize` + mail-server verification (reusing
  `restore.sh`)
- Transition to `Starting` on neutralize success, to `InitFailed` on
  any of the three Jobs failing

Guards/actions follow the existing `Initializing` patterns.  The
parallel-then-barrier shape mirrors how the existing migration phases
already handle multi-step sequences.

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

### Phase 1 — F2 + D1 baseline (2-3 days)
- New `OdooStagingRefreshJob` CRD (and associated RBAC)
- Controller logic that runs a pod-to-pod `pg_dump | psql` stream for
  the DB and a filestore rsync for the filestore
- Reuses `restore.sh`'s neutralize + mail-server verification
  verbatim — no new verification code
- Covers acceptance criteria 1, 3, 4

### Phase 2 — F1 CSI VolumeSnapshot path (2-3 days)
- Detect snapshot-capable storage class via `VolumeSnapshotClass` list
- Implement `VolumeSnapshot` create + wait-for-ready + clone-into-PVC
- Falls back to F2 if preconditions not met
- Covers acceptance criterion 2 — and this is where the real wall-clock
  win sits for typical deployments (DBs are 1-2 GB; filestores are
  40-50 GB and dominate the total transfer time)

### Phase 3 — Staging-aware spec field + auto-init (1 day)
- `OdooInstance.spec.cloneFrom` field + webhook validation
- Auto-create `OdooStagingRefreshJob` on first reconcile when set

Total ~5-7 days.  Phase 1 alone closes the issue's minimum bar with
a path that works against any Postgres.  Phase 2 is the practical
speedup — for the current fleet (2 GB DB / 40-50 GB filestores), the
filestore is the bottleneck and CSI snapshot cloning is where the
order-of-magnitude time difference lives.
