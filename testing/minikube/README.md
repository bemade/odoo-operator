# Local minikube test cluster

Scripts to stand up a disposable single-node cluster that runs the operator
end to end: a CloudNativePG Postgres, cert-manager for the webhook, Mailpit as
the staging SMTP sink, the CSI hostpath driver for snapshot-capable volumes,
and a demo pair of Odoo instances (`demo/prod`, Production, and
`demo/staging`, Staging). On top of that you get a smoke test of the
staging-refresh path, a released-to-local upgrade flow, and a few watchers
for observing an upgrade.

The envtest integration tests (`cargo test --test integration`) have no
kubelet, so no pod ever runs. This cluster is where Jobs, PVCs, snapshots,
`odoo neutralize` and real rollouts get exercised.

## Safety: the scripts only touch the minikube profile

A developer's default kubeconfig (`~/.kube/config`) may well point at a
production cluster. Also, `minikube start` writes its context into whatever
`$KUBECONFIG` it sees **and makes that context the current one**. To keep the
two apart:

- every script exports `KUBECONFIG` to a file used only by this profile,
  `${KUBECONFIG_ODOO_OP:-$HOME/.kube/minikube-odoo-op.yaml}`, and refuses to
  run if that resolves to `~/.kube/config` or a list of files;
- before touching the cluster, every script checks that the current context is
  the profile's (`odoo-op` by default) **and** that its API server is the
  profile's node, as reported by `minikube -p <profile> ip`. It refuses
  otherwise;
- every `kubectl` and `helm` call is also pinned with `--context` /
  `--kube-context <profile>`.

The scripts use no `sudo` and install no packages.

| Variable | Default | Purpose |
|---|---|---|
| `ODOO_OP_PROFILE` | `odoo-op` | minikube profile, and so the kube context name |
| `KUBECONFIG_ODOO_OP` | `~/.kube/minikube-odoo-op.yaml` | the profile's dedicated kubeconfig |
| `ODOO_OP_WORKDIR` | `${XDG_CACHE_HOME:-~/.cache}/odoo-operator-minikube` | scratch space for exported charts and sources |

To use `kubectl` yourself against the cluster, export the same file:
`export KUBECONFIG=~/.kube/minikube-odoo-op.yaml`.

## Prerequisites

- `minikube`, `docker` (the default driver), `helm` 3, `kubectl`, `git`, `jq`.
- Network access to the Helm repositories of cert-manager and CloudNativePG,
  and to the container registries (Docker Hub, `ghcr.io`).
- Resources: by default the profile gets **8 CPUs, 16 GiB of memory and a
  60 GB disk**, which is what these scripts were exercised with. Override with
  `MINIKUBE_CPUS`, `MINIKUBE_MEMORY`, `MINIKUBE_DISK` (and
  `MINIKUBE_K8S_VERSION`, `MINIKUBE_DRIVER`). Smaller sizes were not tried.
  A cold build of the operator image (`upgrade-local.sh`) also needs several
  GB of Docker build cache.
- A clone with release tags: `git fetch --tags`.

## Quick start

```sh
cd testing/minikube
./setup.sh all     # cluster + dependencies + latest released operator + demo instances
./smoke.sh         # end-to-end smoke; exit status 0 only if every check passed
```

`make minikube-setup` and `make test-minikube` run the same two commands from
the repository root.

## Setup steps

`./setup.sh all` runs the steps below in order. Any step can be re-run on its
own, e.g. `./setup.sh mailpit demo`; they are idempotent.

| Step | What it does |
|---|---|
| `cluster` | `minikube start -p <profile>`, then enables the `volumesnapshots` and `csi-hostpath-driver` addons. |
| `snapclass` | Marks `csi-hostpath-snapclass` as the default VolumeSnapshotClass (see Caveats). |
| `certmanager` | Installs cert-manager (`CERT_MANAGER_VERSION`). The chart's webhook certificates are cert-manager Issuers/Certificates, so it is required. |
| `cnpg` | Installs the CloudNativePG operator (`CNPG_CHART_VERSION`). |
| `pgcluster` | Creates the single-instance cluster `pg/pg-main` ([manifests/pg-cluster.yaml](manifests/pg-cluster.yaml)), and the operator's `odoo-operator/postgres-clusters` Secret, built from the CNPG superuser secret. |
| `mailpit` | Deploys Mailpit in namespace `mailpit` (SMTP 1025, UI/API 8025). |
| `operator` | Installs a **released** operator: the chart comes from `git archive <tag> charts/odoo-operator` and the image is `ghcr.io/bemade/odoo-operator:<tag>`. It installs the newest local `vX.Y.Z` tag, or the one given by `--version vX.Y.Z` (or `ODOO_OPERATOR_VERSION`). Values: [values.yaml](values.yaml). |
| `demo` | Creates `demo/prod` and `demo/staging` ([manifests/demo-instances.yaml](manifests/demo-instances.yaml)) and waits until both are Running. |
| `smoke` | Runs `./smoke.sh` (not part of `all`). |

Two details of the `operator` step. GHCR tags carry the `v` (`v2.8.5`, not
`2.8.5`). The chart's default `operator.image` has no tag, so the script always
sets `operator.image` explicitly.

## Smoke test

`./smoke.sh` with no arguments runs:

```
seed  check prod  refresh copy  mail  reap  refresh snapshot  mail
```

| Action | Checks |
|---|---|
| `seed` | Creates `smoke-*` attachments on `demo/prod`, so its filestore holds files that only exist there, and a "real" outgoing mail server, `smtp.example.com`, that must not survive into staging. |
| `check <instance>` | Every stored `ir_attachment` row has its file; the file checksums match; the `smoke-*` attachments are present. A **Production** instance must not be neutralized, and its seeded mail server must be active. A **Staging** instance must be neutralized, the seeded server inactive, the operator-injected Mailpit server the only active one, and it must have as many `smoke-*` attachments as the source. |
| `refresh <copy\|snapshot\|auto>` | Creates an `OdooStagingRefreshJob` from prod to staging and waits for it (`SMOKE_REFRESH_TIMEOUT`, default 1800 s). It must reach `Completed` by the expected path (`status.sourceSnapshot` set for `snapshot`, unset for `copy`), and the temporary `<db>_refresh_*` database must be gone. Then it runs `check staging`. |
| `mail` | Sends a mail with a unique subject from `demo/staging` and expects it in Mailpit within 30 s. |
| `reap` | Deletes finished (Succeeded/Failed) pods in `demo`. It refuses while a refresh is running (see Caveats). |

Actions run in the order given, e.g. `./smoke.sh check staging` or
`./smoke.sh refresh snapshot mail`. Each check prints `PASS`/`FAIL` as it
runs, then a summary is printed. The exit status is non-zero if any check
failed or the run aborted.

The Odoo-side checks are plain `odoo shell` scripts in
[odoo-shell/](odoo-shell/), piped through `kubectl exec`.

## Testing an upgrade from a release to a local revision

1. **Baseline on a release:** `./setup.sh all`, or `./setup.sh --version
   v2.8.5 all` for a specific one. Run `./smoke.sh` to confirm the baseline.
2. **Start observers** in other terminals, if you want evidence of the
   transition (see [observe/](#observe)):
   ```sh
   observe/watch-operator-pods.sh 900      # old/new operator pod overlap
   observe/probe-http.sh 900               # instance downtime
   observe/collect-operator-logs.sh 900 /tmp/op-logs
   observe/capture.sh > /tmp/before.txt
   ```
3. **Upgrade to a local revision:** `./upgrade-local.sh <rev>` (default `HEAD`;
   `make install REV=<rev>`). It:
   - exports `<rev>` with `git archive` into `$ODOO_OP_WORKDIR/src-<sha>`, and
     `docker build`s it from there as `localhost/odoo-operator:local-<sha>`;
   - loads that image into the minikube node (`minikube image load`, no
     registry needed);
   - runs `helm upgrade` with **that revision's chart**, and so its CRDs, using
     the same `values.yaml` as the baseline, plus `operator.imagePullPolicy=Never`.

   Only the committed tree is built: uncommitted changes are left out, and the
   script warns when it sees them. Commit first, even as a throwaway WIP
   commit. If you changed CRD types, run `make helm-crds` and commit the chart
   before building. This also keeps the clone's multi-GB `target/` out of the
   Docker build context (the repository has no `.dockerignore`).
   `--build-only` (`make docker-load`) stops after the image load, and
   `--deploy-only` re-runs only the Helm step.
4. **Verify:** `./smoke.sh` again, `observe/capture.sh > /tmp/after.txt`, and
   `observe/grep-pg-errors.sh 15m`.
5. **Roll back** to the release with `./setup.sh --version vX.Y.Z operator`.

## Exercising the neutralize-failure path

[inject-neutralize-failure.sh](inject-neutralize-failure.sh) makes the next
refresh fail in its neutralize step, every time. It installs a `BEFORE UPDATE`
trigger ([sql/neutralize-failure-inject.sql](sql/neutralize-failure-inject.sql))
that keeps the seeded `smtp.example.com` mail server active. `pg_dump` carries
the trigger into the refresh's temporary database, so the post-neutralize check
for active outgoing mail servers fails there.

The script only ever targets the demo's **source** database: the `db_name` in
`demo/prod`'s own `odoo.conf`. It refuses unless `demo/prod` is
`environment: Production` and that database is neither `demo/staging`'s nor a
refresh temporary DB. The SQL itself also aborts unless it is connected to the
database the script resolved. The source instance keeps working; nothing there
changes unless someone archives that mail server.

```sh
./smoke.sh seed                          # the trigger acts on the seeded server
./inject-neutralize-failure.sh apply
./smoke.sh refresh copy                  # expected: FAIL refresh-copy (phase Failed)
./inject-neutralize-failure.sh undo
./inject-neutralize-failure.sh status
./smoke.sh refresh copy                  # a clean refresh afterwards
```

Things worth checking while it is applied:
- the refresh ends `Failed`. This can take several minutes, because the
  neutralize Job retries up to its backoff limit;
- the live staging database is left untouched (write a marker into it
  beforehand);
- the temporary database is kept;
- the state the target instance is left in. In testing, staging was left
  `InitFailed`, scaled to zero, although its live database was intact; the
  next successful refresh brought it back.

## Reaching Odoo and Mailpit

There is no ingress controller, so the `*.odoo-op.local` hosts in the demo
manifests, and the URLs in the instance status, do not resolve. Use
port-forwards, with the profile's kubeconfig:

```sh
export KUBECONFIG=~/.kube/minikube-odoo-op.yaml
kubectl -n demo port-forward svc/prod 8069:8069         # http://localhost:8069
kubectl -n demo port-forward svc/staging 8070:8069      # http://localhost:8070
kubectl -n mailpit port-forward svc/mailpit 8025:8025   # http://localhost:8025
```

The databases are fresh `-i base` installs, so the Odoo login is Odoo's
default `admin` / `admin`. The instances' `adminPassword` (`admin` as well) is
the database-manager master password.

## Observe

Read-only watchers for upgrades and refreshes. Each takes a duration in
seconds and logs only on change.

| Script | Shows |
|---|---|
| `observe/watch-operator-pods.sh [s]` | operator pods: phase, readiness, image tag, deletionTimestamp (1 s poll) |
| `observe/probe-http.sh [s]` | `/web/login` UP/DOWN for prod and staging, probed from inside the cluster |
| `observe/watch-demo.sh [s]` | `*-odoo-conf` ConfigMaps/Secrets and pods in `demo` |
| `observe/watch-dbs.sh [s]` | the `odoo*` databases, e.g. a refresh's temporary DB coming and going |
| `observe/collect-operator-logs.sh <s> <dir>` | full operator pod logs, kept after the pods go away |
| `observe/grep-pg-errors.sh [since]` | counts of connection-exhaustion and role-reset lines in the operator and Postgres logs |
| `observe/capture.sh` | point-in-time state of `demo`, for before/after diffs; prints no secret values |

## Teardown

```sh
minikube delete -p odoo-op
rm -f ~/.kube/minikube-odoo-op.yaml          # optional
rm -rf ~/.cache/odoo-operator-minikube       # exported charts and sources
docker image ls 'localhost/odoo-operator'    # locally built images, if you want them gone
```

## Caveats

- **Back-to-back refreshes can stall on the snapshot path.** A snapshot-path
  refresh that follows another refresh can sit with the target filestore PVC
  "still terminating". The cause is the previous refresh's finished Job pods:
  they still mount the PVC, so `kubernetes.io/pvc-protection` holds it.
  Deleting the finished Job pods in the namespace (`./smoke.sh reap`) lets the
  refresh proceed. The default smoke sequence reaps before its snapshot
  refresh for this reason. Run `refresh snapshot` without it to reproduce the
  stall.
- **The hostpath CSI driver populates volumes synchronously.** A PVC restored
  from a snapshot is complete as soon as it binds. Drivers that populate
  asynchronously, such as JuiceFS, behave differently, so timing-sensitive
  snapshot behaviour cannot be reproduced here.
- **The VolumeSnapshotClass must be the default.** The operator creates its
  VolumeSnapshots without a `volumeSnapshotClassName`. Without a default class
  (`setup.sh snapclass`), the snapshot path fails.
- **The snapshot path needs explicit, equal storage classes.** It is only
  possible when both instances' `spec.filestore.storageClass` are set and
  equal; a class inherited from the chart default does not count. The demo
  manifests set it on both.
- **Old and new operator pods can overlap during an upgrade.** The chart leaves
  the operator Deployment on the default `RollingUpdate` strategy, and there is
  no leader election. During `helm upgrade`, the old and new operator pods can
  therefore run side by side for up to the old pod's termination grace period,
  both reconciling the same instances. Use `observe/watch-operator-pods.sh` and
  `observe/grep-pg-errors.sh` to see whether that happened.
- **Rolling back reinstalls the older CRDs.** `setup.sh --version <older>
  operator` applies the older chart's CRD schemas, so the API server prunes
  fields that only the newer version knows. Objects the newer operator created
  (Secrets, for example) are left behind.
- **Only committed code is built.** `upgrade-local.sh` builds committed trees
  only (see above).
