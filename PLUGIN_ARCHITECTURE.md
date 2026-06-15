# OdooInstance Plugin Architecture — Design Proposal

**Status:** Draft / Proposal (not yet implemented)
**Audience:** odoo-operator maintainers and plugin authors

> This document captures a proposed extension model that lets *separate*
> controllers provision and configure per-instance dependencies (mail capture,
> databases, object-store filestores, notification servers, …) without baking
> any of them into the operator core. It is a design record for discussion — no
> code changes are implied by merging it.

---

## 1. Motivation

The operator keeps accreting the same shape of work: *"when an `OdooInstance`
exists, ensure some dependency exists, capture its connection details, configure
the instance to use it, and tear it down (or release it) on delete."* Concrete
cases already on the table:

- **Per-tenant mail capture (Mailpit).** Today a single cluster-wide Mailpit is
  shared by every staging instance via the global `--default-staging-smtp-host`
  flag; all tenants' mail lands in one inbox behind one password. We want
  per-tenant isolation (e.g. one Mailpit per client namespace).
- **Per-instance / per-namespace Postgres (CNPG).** The operator currently
  treats Postgres as an external network endpoint (a `clusters.yaml` secret +
  direct-SQL role management). We may want to provision a CloudNativePG
  `Cluster` per instance or per namespace — while keeping the existing
  cluster-wide-connection mode working unchanged.
- **S3 / object-store filestores.** Today the filestore is a PVC only; an S3
  backend would mean provisioning a bucket and wiring its credentials in.
- **Other** per-instance services as they come up (e.g. an ntfy notification
  server).

Solving each of these one-off, with bespoke `if` branches inside
`reconcile_instance()`, is how an operator rots. We want **one extension seam**
so each dependency is an independent, optional unit — ideally maintained and
released on its own cadence, including by external contributors (the operator
already has contributors running it in multiple countries).

### Precedent: CloudNativePG's CNPG-I

CloudNativePG hit exactly this wall and is mid-migration through it. They
**deprecated the in-tree `barmanObjectStore` backup config** (CNPG 1.26,
removal targeted ~1.28) and moved Barman Cloud to an **external plugin** built
on **CNPG-I**, their gRPC plugin interface. Stated rationale: a *backup-agnostic
core*, leaner operand images, and *independent maintenance of the plugin while
keeping the core focused on PostgreSQL orchestration*.

CNPG-I is a strong precedent for "core shouldn't carry every integration," and
this proposal borrows several of its patterns (a `spec.plugins` list, capability
declaration, plugin-owned CRDs referenced by name, graceful degradation when a
plugin is absent). It deliberately **does not** adopt CNPG-I's gRPC transport —
see §4.

---

## 2. Goals / Non-Goals

**Goals**
- An out-of-process extension model: dependencies are provisioned/configured by
  **separate controllers**, not compiled into the operator binary.
- The operator core stays dependency-agnostic; adding a dependency must not
  require core code changes for the common cases.
- **No regression:** existing modes (cluster-wide Postgres connection, global
  staging SMTP) keep working with zero change for instances that don't opt in.
- Safe concurrency: multiple writers (operator, plugins, humans) co-authoring
  one `OdooInstance` must never silently clobber each other.
- Legibility: an instance's effective configuration is visible on the
  `OdooInstance` itself (`kubectl get odooinstance -o yaml`), not smeared across
  side Secrets and out-of-repo scripts.

**Non-Goals**
- A general-purpose, dynamically-loaded (gRPC/WASM) plugin runtime. (Considered
  and rejected for current scale — §4.)
- Replacing the existing state machine; this slots into it.
- Solving multi-cluster fan-out.

---

## 3. Design Overview

Plugins are **independent Kubernetes controllers**. The coordination "protocol"
is the Kubernetes API itself — the `OdooInstance` CRD plus a status convention —
not a custom RPC.

For a given `OdooInstance`, a plugin:

1. **Watches** OdooInstances and selects the ones it applies to.
2. **Provisions** its dependency (a Mailpit, a CNPG `Cluster`, an S3 bucket, …),
   owning whatever child resources/CRDs that entails.
3. **Writes the resulting configuration into the instance's own (structured)
   spec fields** — e.g. `spec.mail.outgoing`, `spec.database` — using
   server-side apply under its own field manager.
4. **Signals readiness** by writing `status.plugins[<name>].ready = true`.

The operator:

1. Knows, per instance, the set of applicable plugins (from registration — §5.2).
2. **Cedes defaulting** on fields a plugin has claimed (uses built-in defaults
   otherwise — this is what keeps the existing modes working).
3. **Gates** the lifecycle in an `AwaitingPlugins` stage until every applicable
   plugin reports ready, then reconciles using the now-complete spec.

The config a plugin injects is, from the operator's point of view, just ordinary
CRD configuration it already knows how to consume.

---

## 4. Why not gRPC (CNPG-I-style)?

CNPG-I uses synchronous gRPC because backup/WAL operations are **fine-grained
and ordered** ("archive *this* WAL segment *now*"). That justifies a call
interface with typed responses, mTLS, discovery, and protocol versioning.

odoo-operator's coordination is **coarse and state-based**: "does a database
exist and is it reachable?", "what is the SMTP host?", "is the bucket
provisioned?". Those are not calls — they are *facts that converge over time*,
which is precisely what the Kubernetes status/condition model is built for. A
declarative contract is therefore cheaper, language-agnostic (a plugin is just a
controller in any framework), debuggable with `kubectl`, and avoids a bespoke
proto + mTLS-for-RPC + version-negotiation surface.

We keep an escape hatch in mind: if a future plugin genuinely needs **synchronous
validation** or **arbitrary pod mutation** the operator can't anticipate, that's
the point to consider a narrow webhook (or, last resort, an RPC). Start
declarative; escalate per concrete need.

Alternatives considered and rejected:

- **In-binary provider trait** (a Rust `DependencyProvider` trait, à la the
  existing `PostgresManager`). Clean, but still compiled into the operator and
  on its release cadence — fails the "out-of-process / independent maintenance"
  goal, and has no good home for shared-resource GC.
- **Full gRPC plugin interface** (CNPG-I clone). Right for CNPG's scale and
  needs; over-engineered here (CNPG-I is still *Experimental* and lacks protocol
  version negotiation; we'd be reinventing it in Rust/tonic).
- **Env passthrough as the primary mechanism** (operator dumps plugin-produced
  env/Secret refs into the pod). Rejected as opaque: config ends up scattered
  across the operator, a Secret, and a neutralize script in a *different repo*.
  Retained only as a long-tail escape hatch (§6.4).

---

## 5. Core Mechanisms

### 5.1 Plugins as independent controllers

Each plugin is its own Deployment/controller with its own RBAC, CRDs, and
finalizers. This is what cleanly solves the **shared-resource** case: a
per-namespace Mailpit shared by several staging instances needs reference
counting and "last-instance-out" teardown — logic the operator core has no
facility for. As an independent controller, the Mailpit plugin owns that
lifecycle entirely, out of core.

Per-instance (1:1) dependencies (a CNPG `Cluster`, an S3 bucket) are simpler:
the plugin can `ownerReference` them to the `OdooInstance` so Kubernetes
garbage-collects them on delete.

### 5.2 Registration CR + claims + validating webhook

A plugin ships a **registration CR** (working name `OdooOperatorPlugin`) that
declares:

- the plugin's **name**,
- an **instance selector** (e.g. `environment=Staging` for Mailpit),
- the **spec field(s) it claims** (the structured fields it will author),
- its readiness contract.

The operator **watches** these registrations, so for any new instance it knows
the applicable plugin set *before* it starts reconciling (this avoids a
bootstrap race where the operator races ahead of a plugin's first watch event).

Claims are validated at **install time** by the operator's existing **validating
webhook**: registering a plugin whose claim conflicts with an already-registered
plugin is **rejected before the plugin can run**. This converts conflict from a
silent *runtime* problem into an explicit *admission* one.

"What is up for grabs" has three categories:

| Category | Examples | Claimable? |
|---|---|---|
| **Claimable / dependency fields** | `spec.database`, `spec.mail.outgoing`, `spec.filestore`, future `spec.notifications` | Yes — at most one plugin each |
| **Operator-structural fields** | `replicas`, `image`, `adminPassword`, ingress hosts | No — claim rejected |
| **Granular escape hatch** | `config_options` (map) | No claim needed — safely shared per-key (§6.4) |

Claiming a claimable field is an **authority handoff, not a conflict**: the
operator defaults `spec.database` *until* a `cnpg` plugin claims it, at which
point the operator cedes its default and waits for the plugin. This is what
preserves *"Postgres is just a provider, and the old built-in mode still works."*

Webhook logic: *claimable + unclaimed* → grant (operator yields its default);
*claimable + already-claimed* → reject; *not claimable* → reject. Plus two cheap
guards: the claimed path must **exist in the current `OdooInstance` schema**, and
**prefix overlap** is a conflict (`spec.database` vs `spec.database.host`).

**Human writes are governed by the same claims — at two layers.** Managed
*child* resources (Deployments, Services, the Mailpit/CNPG resources a plugin
spawns) are never admission-guarded; they're protected by **reconvergence** —
the owning controller rebuilds them from spec each cycle. The shared
`OdooInstance` CR is different: it's protected by **SSA + the validating
webhook**. SSA conflict detection already rejects a non-owner *apply* of a
claimed field for free; the webhook covers the *edit/patch* path SSA ownership
doesn't block. On `UPDATE` the webhook receives both `oldObject` and `object`
plus `request.userInfo`, so it diffs the claimed paths and **denies if a claimed
field changed and the requester isn't its registered owner** (simplest v1:
claimed fields are editable only by registered controller ServiceAccounts;
humans are directed to the plugin's own CR). This extends a choice the operator
already made — see the Decision Log entry *"Validating webhook over
reconcile-loop revert"* in `ROADMAP.md` — to plugin-claimed fields.

### 5.3 Field ownership via Server-Side Apply (SSA)

Registration is the *policy* layer (who may own what); **SSA is the runtime
enforcement** layer. Each writer applies under a distinct **field manager**; the
API server records per-field ownership in `metadata.managedFields` and rejects
conflicting writes. The operator already applies all child resources via SSA
under `FIELD_MANAGER = "odoo-operator"`, so this is an extension of the existing
apply discipline, not a new mechanism.

This works cleanly only if list/map merge semantics are declared correctly:

- **Maps** (e.g. `config_options`) are *granular* by default — ownership tracked
  per key. Plugins can safely co-own different keys. (No registration needed.)
- **Lists** default to *atomic* — owned as a single blob, so any writer replaces
  the whole list and silently drops others' entries. Structured list fields
  (e.g. `spec.mail.outgoing`) **must** be declared `x-kubernetes-list-type: map`
  with `x-kubernetes-list-map-keys` (the element's unique key) so each entry is
  owned independently.

> **Prerequisite:** `spec.extraEnv` (added in #127) is currently an atomic list.
> Making it merge-keyed (`x-kubernetes-list-type: map`, key `name`) is tracked
> as **issue #144** — a self-contained, backward-compatible CRD change that
> hardens the SSA foundation independently of this design. (`spec.extraEnvFrom`
> has no natural unique scalar key and stays atomic by design — co-writers
> reference their own fully-owned Secret/ConfigMap instead of sharing it.)

New structured fields (`spec.mail.outgoing`, etc.) should be declared
merge-keyed **from day one** so they never inherit the atomic problem.

### 5.4 The `AwaitingPlugins` lifecycle stage

The operator already has a stubbed `provisioning.rs` state (with a TODO to
absorb the dependency steps). That is the home for an `AwaitingPlugins` /
`Provisioning` stage placed **before** `Initializing`:

- It blocks until every *applicable* plugin reports `status.plugins[<name>].ready`.
- It surfaces a condition listing who is still pending.
- It **times out** to a `Degraded`/`Blocked` state naming the missing plugin, so
  a not-installed or crashed plugin is diagnosable rather than a silent hang.

The gate is on **readiness**, not on a field being populated — a plugin may
legitimately report ready having deliberately left a claimed field empty (§5.5).

Then it transitions to `Initializing` with the spec the plugins completed. This
also enforces ordering (DB-before-init, mail-host-before-neutralize) via a
readiness gate rather than an RPC.

### 5.5 Conditional and partial fill

Claiming a field is about *ownership*, not an *obligation* to fill it. A plugin
may, for a given instance, fill a claimed field, leave it deliberately empty, or
fill only some of several claimed fields. Three rules keep this well-defined:

1. **Gate on `ready`, interpret content after.** The `AwaitingPlugins` gate
   waits on `status.plugins[<name>].ready`, never on a field being non-empty —
   otherwise a deliberately-empty field would hang forever. `ready` means "I have
   finished deciding," whether or not anything was written.

2. **SSA ownership expresses the outcome — no extra contract.** After a plugin is
   ready, each claimed field is in one of three states, distinguished purely by
   SSA field ownership:

   | Plugin action | Ownership | Operator interpretation |
   |---|---|---|
   | wrote a value | owns it, non-empty | use the plugin's value |
   | wrote an explicit empty/disabled value (e.g. `outgoing: []`) | owns it, empty | feature explicitly **off** — no fallback |
   | didn't touch the field | **unowned** | operator applies its built-in default; if none, unset/off |

   Because a manager only owns fields it includes in its apply, an untouched
   field is unowned — so the operator can default it *without a conflict*. The
   timing rule: only treat "still unowned" as "deliberately not filled" **after
   the plugin is ready**, else the operator races a plugin about to fill it.

3. **Prefer the selector for *static* non-applicability.** If a plugin
   structurally never configures a class of instances (e.g. mail only for
   staging), express that in the registration **selector** so the field is never
   *claimed* there — the operator keeps its default and doesn't gate on the
   plugin. Reserve the runtime ownership-trichotomy for *data-dependent*
   emptiness a selector can't predict. (A concrete argument for selector-scoped
   claims — §9.)

Finally, make it **observable**: surface a condition when a claimed field is left
to default or explicitly disabled by a plugin, so under-configuration is visible
rather than a later head-scratch.

---

## 6. Output Handoff

### 6.1 Primary path: structured spec fields

Plugins write into well-defined CRD fields the operator already consumes
(`spec.database`, `spec.mail.outgoing`, `spec.filestore`, …). This keeps the
effective config legible on the instance and reuses the operator's existing
wiring (the `<instance>-odoo-conf` ConfigMap, the neutralize job env, etc.).

### 6.2 Example: mail (the multi-tenant Mailpit goal)

SMTP currently comes from the global `--default-staging-smtp-host` flag, injected
into the neutralize job, which SQL-writes Odoo's `ir_mail_server` table. This
design moves SMTP to a **per-instance `spec.mail.outgoing`** field (a merge-keyed
list). The Mailpit plugin provisions the per-namespace Mailpit, writes
`spec.mail.outgoing` pointing at it, and signals ready; the operator gates the
neutralize step on readiness and uses the field value. This is what makes
per-tenant mail isolation expressible at all.

### 6.3 Example: database (per-instance/namespace CNPG)

No `database`-owning plugin registered → operator uses its built-in
cluster-wide-connection default. A `cnpg` plugin registered → operator defers
`spec.database`, the plugin provisions a `Cluster` (ownerRef'd for 1:1, or
namespace-scoped + refcounted for shared) and writes the connection (creds in a
Secret it owns), then signals ready.

### 6.4 Escape hatch: `config_options` (and `extraEnvFrom`)

For the long tail that only needs a few `odoo.conf` keys, plugins write into
`config_options` — a granular map, so multiple plugins safely co-own keys with
no registration. When a plugin needs real container env from a resource it owns,
it references its own Secret/ConfigMap via `spec.extraEnvFrom` (single-writer by
design; not a shared list).

The tradeoff (accepted): **the CRD schema is the contract.** A genuinely new
dependency *category* needs a new CRD field (a versioned, reviewed core change),
not a free-form drop-in. `config_options` softens this for simple conf-key cases.

---

## 7. Lifecycle Edge Cases

- **Deregistration / uninstall.** Removing a plugin releases its claims (fields
  revert to "up for grabs" / operator default). Instances still carrying that
  plugin's authored values (e.g. `mail.outgoing` pointing at a now-deleted
  Mailpit) are left stale — so plugin removal should either be **gated** while
  instances still reference it, or trigger the operator to **revert** those
  fields to defaults. To be decided.
- **Eager vs lazy repoint.** Because the SMTP host is baked into the DB at
  neutralize time, an already-neutralized instance won't pick up a new host
  until its next refresh ("lazy"). An "eager" path would run a standalone
  `UPDATE ir_mail_server` against running instances. To be decided.
- **CRD schema evolution.** New structured fields are additive CRD changes,
  rolled out in normal operator version bumps.

---

## 8. Proposed Phasing

1. **Phase 0 — SSA prerequisite (independent):** make `spec.extraEnv`
   merge-keyed (#144). Backward-compatible, ships on its own.
2. **Phase 1 — structured mail field:** add a merge-keyed `spec.mail.outgoing`;
   teach the operator to read it; keep the global SMTP flag as the default when
   no mail plugin is present.
3. **Phase 2 — extension plumbing:** the `OdooOperatorPlugin` registration CR,
   webhook claim validation (incl. human-edit governance), and the
   `AwaitingPlugins` stage.
4. **Phase 3 — pilot plugin: Mailpit.** Chosen as the first plugin because it is
   the live need, exercises the shared-namespace GC that only the
   separate-process model handles, and is low-blast-radius (staging mail capture
   failing is benign). Postgres provisioning stays built-in (cluster-wide
   params) until the interface is battle-tested.
5. **Later:** CNPG and S3 plugins.

---

## 9. Open Questions

- **Global vs selector-scoped claims.** Start **global** (a field has exactly
  one owner cluster-wide); add selector-scoping (e.g. different DB plugins for
  Staging vs Production) only if a concrete need appears. §5.5 (conditional fill)
  is one such need — selector-scoped claims turn many "applicable-but-empty"
  cases into clean "not applicable."
- **Eager vs lazy repoint** of running instances (§7).
- **Uninstall semantics** — gate vs revert (§7).
- **Spec-vs-status authorship.** This design has plugins author *spec* (for
  legibility). The purist alternative — plugins write *status*, operator merges
  into an effective config — avoids controller-authored spec but loses the
  "the CR shows what it uses" transparency. We chose spec authorship + SSA
  field ownership; noted here as the considered alternative.

---

## 10. References

- CloudNativePG 1.26 release (Barman deprecation rationale):
  https://cloudnative-pg.io/releases/cloudnative-pg-1-26.0-released/
- CNPG-I (plugin interface): https://github.com/cloudnative-pg/cnpg-i
- Barman Cloud Plugin: https://github.com/cloudnative-pg/plugin-barman-cloud
- Kubernetes Server-Side Apply:
  https://kubernetes.io/docs/reference/using-api/server-side-apply/
- `x-kubernetes-list-type` / `-list-map-keys`:
  https://kubernetes.io/docs/reference/using-api/server-side-apply/#merge-strategy
- odoo-operator #127 — `spec.extraEnv` / `spec.extraEnvFrom` injection
- odoo-operator #144 — make `spec.extraEnv` a merge-keyed list (SSA prerequisite)
