# Feature: Plugin Management


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Register Builtin Plugin Catalog](#register-builtin-plugin-catalog)
  - [Create Custom Plugin Definition](#create-custom-plugin-definition)
  - [List and Read Plugin Definitions](#list-and-read-plugin-definitions)
  - [Retrieve Custom Plugin Source](#retrieve-custom-plugin-source)
  - [Delete Plugin Definition](#delete-plugin-definition)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Builtin Catalog and Registry Construction](#builtin-catalog-and-registry-construction)
  - [Plugin Identifier Classification](#plugin-identifier-classification)
  - [Plugin Definition Validation](#plugin-definition-validation)
  - [Plugin Binding Validation](#plugin-binding-validation)
  - [Plugin-in-Use Conflict Detection](#plugin-in-use-conflict-detection)
  - [Plugin List Query Translation](#plugin-list-query-translation)
  - [Plugin Store Write and Invariant Enforcement](#plugin-store-write-and-invariant-enforcement)
- [4. States (CDSL)](#4-states-cdsl)
  - [Plugin Definition State Machine](#plugin-definition-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Plugin Type Catalog and GTS Identifier Patterns](#plugin-type-catalog-and-gts-identifier-patterns)
  - [Builtin Plugin Registries](#builtin-plugin-registries)
  - [Plugin Definition Model](#plugin-definition-model)
  - [Plugin Management Endpoints](#plugin-management-endpoints)
  - [Plugin Immutability](#plugin-immutability)
  - [Plugin Source Endpoint](#plugin-source-endpoint)
  - [Plugin-in-Use Conflict Detection](#plugin-in-use-conflict-detection-1)
  - [Plugin Binding Validation](#plugin-binding-validation-1)
  - [Tenant Scoping of Plugin Definitions](#tenant-scoping-of-plugin-definitions)
  - [In-Memory Plugin Store and Logical Invariants](#in-memory-plugin-store-and-logical-invariants)
  - [Test Layering](#test-layering)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-plugin-management-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-management`

<!--
=============================================================================
FEATURE SPECIFICATION
=============================================================================
PURPOSE: Define detailed implementation behavior — flows, algorithms, states,
and implementation requirements that bridge PRD and DESIGN to code.

SCOPE:
  ✓ Actor flows (user-facing interactions, step by step)
  ✓ Processes / Business Logic (incl. internal logic, validation, async jobs, etc)
  ✓ State machines (entity lifecycle)
  ✓ Implementation requirements (what to build)
  ✓ Acceptance criteria (how to verify)

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Architecture, components, APIs → DESIGN.md
  ✗ Why a specific approach was chosen → ADR/

CDSL PSEUDO-CODE:
  Optional. Use for complex flows or when precise behavior must be
  communicated. Skip for simple features to avoid overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This feature is the plugin control plane of the OAGW gear: the plugin type catalog in the
types-registry-compatible GTS id space, the builtin plugin registries with their resolvable and
catalog-only identifiers, the tenant-scoped custom plugin definition CRUD — `POST /oagw/v1/plugins`,
`GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}` and
`GET /oagw/v1/plugins/{id}/source` — the plugin-in-use conflict detection that guards the delete, and
the plugin binding validation that the upstream and route write path of entry 2.2 calls before it
stores a binding. Custom plugin definitions are immutable after creation and are stored in the
in-memory store entry 2.2 introduces (DECOMPOSITION assumption 3), which keeps
`cpt-cf-oagw-db-schema` as the logical data model and enforces its invariants. Custom Starlark
definitions are stored and served but not executed (DECOMPOSITION assumption 4): no Starlark
interpreter dependency exists in the crate's dependency set, so this feature delivers the storage and
source contract of `cpt-cf-oagw-nfr-starlark-sandbox` and no execution path. The registries this
feature constructs are the dataset entry 2.5 resolves the request plugin chain through; entry 2.5
owns everything that happens when a plugin actually runs.

### 1.2 Purpose

Entry 2.3 sits between the control plane that stores configuration and the data plane that executes
it. It comes after entry 2.2 because plugin-in-use conflict detection has to scan the upstream and
route plugin bindings and the upstream auth-plugin references that entry 2.2 validates and stores —
the store and its invariants must already exist — and it comes before entry 2.5 because the request
plugin chain resolves its named and custom plugins through the registries this feature constructs.
This feature owns the management half of the plugin system: the three plugin types and their GTS id
patterns, the builtin catalog split into resolvable plugins and catalog-only identifiers, custom
definition CRUD including the source read, definition immutability, plugin-in-use conflict detection
and binding validation. Entry 2.5 owns the other half of the same shared requirements — the
execution order, the credential injection, the guard decisions and the response-phase transforms —
per the decomposition's control-plane/data-plane split.

Shared-requirement split: `cpt-cf-oagw-fr-plugin-system` and `cpt-cf-oagw-fr-builtin-plugins` are
cited by entries 2.3 and 2.5. This entry owns the CRUD and registry behaviour (the type catalog, the
builtin registries, the resolvable and catalog-only identifier sets, definition immutability and
binding validation); entry 2.5 owns the request-path behaviour (the Auth → Guards → Transform
execution order, upstream-before-route chain composition, credential injection and plugin
timeouts). Nothing in this feature resolves a plugin into an executable instance at request time.

This feature realizes `cpt-cf-oagw-principle-plugin-immutable` as the absence of a replace operation
and the delete-only lifecycle of a definition, `cpt-cf-oagw-principle-tenant-scope` as store-level
tenant isolation of the `oagw_plugin` logical table, and it keeps the plugin registries in the
`infra/` layer and the plugin traits in the `domain/` layer of `cpt-cf-oagw-component-model`.

**Requirements**:

- [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
- [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
- [ ] `p1` - `cpt-cf-oagw-contract-types-registry`
- [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`, `cpt-cf-oagw-principle-tenant-scope`

**Feature-local deviations from platform baselines** (each inherited from the decomposition's
task-level assumptions or from a documented DESIGN/schema gap; none is a new decision taken here):

- Gear-relative management paths — DECOMPOSITION assumption 1. The five plugin endpoints are
  registered as `/oagw/v1/plugins...` without the `/api` prefix, because the host api-gateway nests
  the gear router under its own `prefix_path`, which is empty in the graded configuration. The
  `/api/oagw/v1/plugins/...` paths in `cpt-cf-oagw-interface-management-api` are the absolute form
  behind an operator gateway and are not what this gear registers. Review owner: OAGW component
  maintainer (`cf-gears-oagw`).
- In-memory store in place of SeaORM and `toolkit-db` — DECOMPOSITION assumption 3, recorded against
  `cpt-cf-oagw-constraint-multi-sql`. The `oagw_plugin`, `oagw_upstream_plugin` and
  `oagw_route_plugin` tables of `cpt-cf-oagw-db-schema` are kept as the logical data model with the
  DESIGN table and column names, the store is built on the crate's existing
  `dashmap`/`parking_lot`/`arc-swap` dependencies, and the DESIGN's relational invariants —
  `UNIQUE (tenant_id, name)`, plugin binding positions contiguous from 0, the `plugin_ref` /
  `plugin_uuid` pairing — are enforced by the store. No FK exists from a binding row to
  `oagw_plugin`, so the plugin-in-use scan is the integrity mechanism that replaces it. Review
  owner: OAGW component maintainer. Validation: in-crate tests assert each plugin-side invariant is
  refused on write and that no record belonging to another tenant is reachable through any read
  path.
- Custom Starlark plugins are stored and served, not executed — DECOMPOSITION assumption 4,
  recorded against `cpt-cf-oagw-nfr-starlark-sandbox`. No Starlark interpreter dependency exists in
  the crate's dependency set, so the sandbox thresholds the requirement states (no network I/O, no
  file I/O, no imports, execution timeout of at most 100 ms and memory of at most 10 MB per
  invocation) are recorded here as the execution-time contract and are not implemented; `source_code`
  is stored verbatim and returned verbatim by `GET {id}/source`, and no semantic validation of the
  script happens on this path. Review owner: OAGW component maintainer, with the security reviewer
  as second approver. Validation: in-crate tests assert a create stores `source_code` verbatim, that
  `GET {id}/source` returns it unchanged, and that this feature's scope registers no plugin
  execution path.
- Plugin garbage collection is out of scope — DECOMPOSITION entry 2.3 out-of-scope against the
  DESIGN lifecycle, which specifies a periodic GC job that marks an unlinked plugin through
  `gc_eligible_at` and deletes it after a TTL (default 30 days). The `last_used_at` and
  `gc_eligible_at` columns stay in the logical model but nothing populates or consumes them in this
  deployment; an unlinked definition is deletable by its owning tenant through the same `DELETE`
  every definition uses. Review owner: OAGW component maintainer. Validation: in-crate tests assert
  `DELETE` of an unlinked definition returns `204` and that no periodic job is registered.
- `PluginInUse` extension keys — the DESIGN error table in `cpt-cf-oagw-interface-api` fixes
  `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` for the 409 but its extension-field list
  (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`) does not name the fields that
  identify the conflict. This feature returns the 409 with `plugin_id` set to the definition's GTS
  identifier and `referenced_by` set to the referencing resource identifiers with their binding
  positions, so an operator can unbind the plugin without a second lookup, and it does not extend
  the error table on its own. Review owner: OAGW component maintainer, with the API contract owner
  as second approver. Validation: an in-crate test asserts the 409 body is
  `application/problem+json`, carries that `type` identifier and both extension keys, and that
  `referenced_by` names every referencing resource.
- No shipped plugin JSON schema — `docs/schemas/` carries `upstream.v1.schema.json` and
  `route.v1.schema.json` but no plugin schema. The create and read representation therefore follows
  the DESIGN `Plugin` class (`id`, `tenant_id`, `plugin_type`, `name`, `config_schema`,
  `source_code`, `last_used_at`, `gc_eligible_at`) and the ADR 0002 Appendix A definition field set
  (`name`, `description`, `plugin_type`, `phases`, `config_schema`, `source_code`), with `id`,
  `tenant_id` and the two timestamps server-managed and `plugin_type` drawn from `auth|guard|
  transform`. Review owner: OAGW component maintainer, with the API contract owner as second
  approver. Validation: in-crate tests assert the accepted and required field set, the recorded
  defaults and the rejection of unknown properties.
- Binding row shape versus schema item form — the DESIGN binding model stores
  `(position, plugin_ref, plugin_uuid, config)` per binding, the shipped upstream schema declares
  `plugins.items[]` as a bare identifier string (GTS identifier or UUID) and the route schema as a
  bare GTS identifier, while the ADR 0009 configuration example carries `plugin_ref` with inline
  `config`. This feature validates and stores the normalized binding row of the DESIGN model and
  leaves the request-side item form to the create and replace DTO of entry 2.2, which validates the
  schema form; a binding created from a bare identifier item carries an empty `config`. Review
  owner: OAGW component maintainer, with the API contract owner as second approver. Validation:
  in-crate tests assert a named binding row keeps `plugin_uuid` NULL, a custom binding row keeps
  both fields set and matching, and a bare identifier item is stored with an empty `config`.

Recorded non-goal carried with the binding validator: the `config` value of a stored binding is kept
as declared and is not validated against the referenced plugin's `config_schema` — the entry's
binding-validation scope lists the positions and the `plugin_ref`/`plugin_uuid` pairing only, and
`config_schema` is stored and served as metadata. Review owner: OAGW component maintainer.

Coverage note: `cpt-cf-oagw-constraint-toolkit-deploy` is inherited from dependency entries 2.1 (gear
deployment, REST wiring and canonical error mapping) and 2.2 (the management write path this feature
extends) and is cited on the DoDs that sit on those layers for that reason, not as a constraint this
feature adopts on its own. `cpt-cf-oagw-constraint-multi-sql` is cited as DECOMPOSITION entry 2.3
records it: the plugin tables are part of the same logical data model, satisfied at the logical level
only (assumption 3). `cpt-cf-oagw-principle-tenant-scope` is realized for the plugin tables and
inherited from dependency entry 2.2: the tenant scoping of the `oagw_plugin` table is the same
store-level isolation entry 2.2 owns, applied to the plugin tables, and this feature defines no second
isolation mechanism of its own.

**Cross-cutting concerns**:

- Security: every plugin operation requires Bearer authentication and the permission string of the
  plugin's type — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}` and the guard and transform
  equivalents — tenant scoping is applied on every read and write before any other rule, and a
  catalog-only identifier is refused at bind time so a reserved id can never be placed in a live
  chain. The management path performs no credential resolution: a plugin definition carries
  configuration schema and Starlark source, never secret material, and no `cred://` reference is
  resolved here.
- Reliability: a plugin write is all-or-nothing — a rejected invariant or a detected in-use conflict
  leaves the store exactly as it was and returns the mapped error, so a client retry is the whole
  remedy. The store is process-local by assumption 3, so a host restart drops the stored definitions
  and recovery is re-provisioning through this same API; there is no persistence to recover from.
- Data integrity: `UNIQUE (tenant_id, name)`, plugin binding positions contiguous from 0, the
  `plugin_ref` / `plugin_uuid` pairing (always stored, UUID only for UUID-backed plugins and matching
  when present) and the atomicity of the in-use check and the delete are enforced inside a single
  write critical section, and readers observe either the previous or the new snapshot, never a
  partial write. Because no FK joins a binding row to `oagw_plugin` (named plugins have no row), the
  plugin-in-use scan is the only guard that keeps a referenced definition from disappearing.
- Observability: structured log lines for each plugin write (operation, plugin type, plugin id,
  tenant, principal, timestamp, outcome) are in scope; the ADR 0001 audit record contract and the
  metrics families are entry 2.7, and no metrics endpoint is exposed (DECOMPOSITION assumption 9).
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable and re-creating the previous definitions through this same API; the only
  in-gear rollback is the staged-write discard of a rejected change.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer and integration tests under the crate's `tests/` directory that boot the gear router and
  exercise the five endpoints — and the `testing/e2e/gears/oagw/` directory is not used
  (DECOMPOSITION assumption 5).
- Compile-time gate: the plugin endpoints exist in the host executable only when the host feature
  `oagw` is enabled and the crate is linked for inventory registration (entry 2.1's gate); this
  feature adds no gate of its own and no new gear.
- Performance: not applicable in this feature — the management path has no latency budget to state
  or measure, and the cost of resolving and executing a plugin chain on the proxy hot path belongs
  to entry 2.5 under `cpt-cf-oagw-nfr-low-latency`. This feature only guarantees that a write
  publishes its snapshot once, so no reader can observe a partially applied catalog.
- Compliance/Privacy: not applicable in this feature — a plugin definition holds configuration
  metadata and script source, not personal data; nothing is written to disk (assumption 3), so there
  is no retention, residency or subject-right surface; and no secret material enters the store, so
  there is no credential exposure surface beyond the rule that `source_code` is never executed here.
- Accessibility: not applicable in this feature — no user-facing interface is authored beyond the
  `application/problem+json` error body contract of entry 2.1, whose machine-readable `type`, `title`
  and `detail` fields are the only surface an accessibility concern could attach to.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Owns the builtin plugin catalog: observes the startup registration of the plugin type schemas and reserved identifiers, creates, reads and deletes system-wide custom plugin definitions, and sees which identifiers are resolvable and which are catalog-only. |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, reads and deletes tenant-scoped custom Starlark plugin definitions inside its own tenant and retrieves their source, and cannot see or address a definition owned by another tenant. |
| `cpt-cf-oagw-actor-types-registry` | Holds the plugin type schemas (`auth_plugin`, `guard_plugin`, `transform_plugin`) and the reserved catalog identifiers; the plugin identifiers this feature issues and accepts in path parameters and in `auth.plugin_type` / `plugins.items[].plugin_ref` are GTS identifiers in that space. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-db-schema`
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.3 and assumptions 1 to 9
- **ADRs**: [0002 Plugin System](../ADR/0002-plugin-system.md) (`cpt-cf-oagw-adr-plugin-system`, the three plugin types, the identifier patterns and the custom definition field set); supporting baselines: [0008 OAuth2 Client Credentials Auth Plugin](../ADR/0008-oauth2-client-credentials-auth-plugin.md) (`cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`, the Form and Basic variants registered in `AuthPluginRegistry::with_builtins`), [0009 Required Headers Guard Plugin](../ADR/0009-required-headers-guard-plugin.md) (`cpt-cf-oagw-adr-required-headers-guard-plugin`, the only guard identifier bindable through `plugins.items[].plugin_ref`, and the catalog-only guard identifiers)
- **Dependencies**: `cpt-cf-oagw-feature-upstream-route-management` — the store and its invariants must already exist so the plugin-in-use scan can walk the upstream and route plugin bindings and the upstream auth-plugin references; this feature also reuses its OData list translation, its resource-identifier resolution and the mount root, error mapping and response-header layers of `cpt-cf-oagw-feature-gear-foundation`
- **Resolved gear dependencies used here**: `types-registry` (plugin type schemas and reserved catalog identifiers), `tenant-resolver` (calling tenant), `authz-resolver` (permission checks); `credstore` is declared but not called on the plugin management path
- **Platform baselines**: toolkit canonical error contract (`toolkit_canonical_errors::CanonicalError` serialized as RFC 9457 `application/problem+json` with GTS `type` identifiers in the `gts.cf.core.errors.err.v1~cf.oagw....v1` space) and the `X-OAGW-Error-Source` response header, both delivered by the entry-2.1 cross-cutting layer; toolkit Bearer authentication and the plugin permission strings of `cpt-cf-oagw-interface-api`; `tenant-resolver` tenant hierarchy; the anonymous GTS resource identifier pattern `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` of `cpt-cf-oagw-interface-api`; the host OpenAPI registry entry 2.1 registers against; the `dashmap`/`parking_lot`/`arc-swap` store primitives already present in the crate's `Cargo.toml`

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the
end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to
actor actions. Every failure below returns through the entry-2.1 mapping layer, so each error body is
`application/problem+json` with a GTS `type` identifier and every response carries
`X-OAGW-Error-Source`.

**Use cases**: None in this feature — the PRD use cases call the proxy path or the upstream and route
management endpoints, not the plugin registry. The plugin definitions and registries this feature
delivers are the preconditions those use cases resolve at request time.

**Referenced, not covered here**:

- `cpt-cf-oagw-usecase-proxy-request` — covered by DECOMPOSITION entry 2.4, which owns the proxy request path whose plugin chain resolves through the registries this feature builds.
- `cpt-cf-oagw-interface-management-api` — covered by DECOMPOSITION entries 2.1 (mount root and canonical response contract) and 2.2 (the upstream and route operations); this feature registers the five plugin operations of the same interface under that contract.
- `cpt-cf-oagw-nfr-multi-tenancy` — covered by DECOMPOSITION entry 2.2, which owns the tenant scoping of the store; this feature reuses the same tenant-keyed store mechanics for the plugin tables and defines no second isolation mechanism.
- `cpt-cf-oagw-nfr-low-latency` — covered by DECOMPOSITION entry 2.4, which owns the proxy hot path whose latency budget the management path of this feature does not share.

### Register Builtin Plugin Catalog

- [x] `p2` - **ID**: `cpt-cf-oagw-flow-plugin-catalog-bootstrap`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The gear registers the three plugin type schemas — `auth_plugin`, `guard_plugin` and
  `transform_plugin` with the base identifiers `gts.cf.core.oagw.auth_plugin.v1~`,
  `gts.cf.core.oagw.guard_plugin.v1~` and `gts.cf.core.oagw.transform_plugin.v1~` — and the reserved
  catalog-only identifiers with the types-registry during gear initialization.
- The three builtin registries are constructed with their resolvable plugins: `AuthPluginRegistry`
  with `noop`, `apikey`, `oauth2_client_cred` and the `oauth2_client_cred_basic` variant;
  `GuardPluginRegistry` with `required_headers`; the transform registry with `request_id`.
- A caller that afterwards sends `auth.plugin_type` of `basic` or `bearer`, or binds `timeout`,
  `cors`, `logging` or `metrics`, is refused, which proves the catalog-only identifiers are
  registered but not resolvable.

**Error Scenarios**:
- A type schema or a reserved identifier cannot be registered with the types-registry: the gear
  init step fails with the offending identifier named, so host startup aborts instead of serving a
  plugin contract whose type catalog is incomplete.
- A builtin plugin identifier collides with an identifier already present in a registry: the
  registry construction fails and host startup aborts.

**Steps**:
1. [x] - `p2` - The host starts the executable with the `oagw` feature enabled and the gear init step runs after entry 2.1 has loaded the configuration and entry 2.2 has built the store - `inst-pcat-01`
2. [x] - `p2` - Register the plugin type schemas for `auth|guard|transform` with `cpt-cf-oagw-actor-types-registry` through the types-registry contract `cpt-cf-oagw-contract-types-registry` - `inst-pcat-02`
3. [x] - `p2` - Register the reserved catalog-only identifiers as catalog entries with no backing implementation: `basic` and `bearer` for auth, `timeout` and `cors` for guards, `logging` and `metrics` for transforms - `inst-pcat-03`
4. [x] - `p2` - **IF** a type schema or a reserved identifier registration fails - `inst-pcat-04`
   1. [x] - `p2` - Fail the gear init step with the offending identifier named in the startup failure, so host startup aborts instead of serving a plugin contract whose catalog is incomplete (entry 2.1's fail-closed startup posture) - `inst-pcat-05`
5. [x] - `p2` - **ELSE** - `inst-pcat-06`
   1. [x] - `p2` - Build the three registries with `cpt-cf-oagw-algo-plugin-catalog-register`, in `infra/plugin/` per `cpt-cf-oagw-component-model` - `inst-pcat-07`
   2. [x] - `p2` - Store the registries and the catalog-only identifier set on the gear state where entry 2.5 resolves the request plugin chain from them - `inst-pcat-08`
6. [x] - `p2` - Register the five plugin operations and their schemas in the host OpenAPI registry during the mount step of entry 2.1 - `inst-pcat-09`
7. [x] - `p2` - Emit the structured startup log line naming the registered plugin types, the resolvable identifier set and the catalog-only identifier set, never credential material - `inst-pcat-10`
8. [x] - `p2` - Treat the resolution of a registry entry into an executable plugin instance and the execution order as data-plane behaviour (entry 2.5), not as a control-plane action - `inst-pcat-11`
9. [x] - `p2` - **RETURN** the initialized catalog: three resolvable registries, the catalog-only identifier set and the registered type schemas - `inst-pcat-12`

### Create Custom Plugin Definition

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-create`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `POST /oagw/v1/plugins` with `plugin_type`, `name`, `description`, `config_schema`,
  `phases` and `source_code` and receives `201` with the stored representation, including the
  server-generated identifier `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` and the server-managed
  `tenant_id`.
- A definition that carries a `config_schema` object with no `source_code` is stored as a
  definition whose source is empty, so the catalog can describe a builtin-shaped contract without a
  script body.
- Two definitions of the same tenant with different `name` values coexist, and the same `name` in
  another tenant is a different resource.

**Error Scenarios**:
- `400` for an invalid body: a `plugin_type` outside `auth|guard|transform`, a missing or empty
  `name`, a `config_schema` that is not a JSON object, a `phases` value outside
  `on_request|on_response|on_error`, or an unknown property.
- `409` when another definition of the same tenant already holds the `(tenant_id, name)` key.
- `401` when the Bearer token is missing or invalid, and `403` when the `create` permission of the
  requested `plugin_type` is not granted.

**Steps**:
1. [x] - `p1` - Actor sends the create request with `plugin_type`, `name`, `description`, `config_schema`, `phases` and `source_code` - `inst-pcre-01`
2. [x] - `p1` - API: `POST /oagw/v1/plugins` authenticates the Bearer token and requires the `gts.cf.core.oagw.{type}_plugin.v1~:create` permission derived from the body's `plugin_type` - `inst-pcre-02`
3. [x] - `p1` - Resolve the calling tenant from the security context; a request without a resolvable tenant is rejected - `inst-pcre-03`
4. [x] - `p1` - Parse the body into the plugin create DTO and reject unknown properties - `inst-pcre-04`
5. [x] - `p1` - Validate the body with `cpt-cf-oagw-algo-plugin-definition-validate`, which also classifies the requested `plugin_type` with `cpt-cf-oagw-algo-plugin-identifier-resolve` - `inst-pcre-05`
6. [x] - `p1` - **IF** validation fails - `inst-pcre-06`
   1. [x] - `p1` - Map the domain error through the entry-2.1 mapping layer and **RETURN** `400` with the validation problem body naming the offending field - `inst-pcre-07`
7. [x] - `p1` - **ELSE** - `inst-pcre-08`
   1. [x] - `p1` - Check the `(tenant_id, name)` key against the tenant's own definitions - `inst-pcre-09`
   2. [x] - `p1` - Store: insert the `oagw_plugin` row with the generated `id`, the calling `tenant_id`, the declared `plugin_type`, `name`, `description`, `config_schema`, `phases` and `source_code`, inside one atomic write that enforces `UNIQUE (tenant_id, name)` - `inst-pcre-10`
   3. [x] - `p1` - Publish the new store snapshot so a reader sees the previous or the new catalog and never a partial write - `inst-pcre-11`
8. [x] - `p1` - Emit the structured log line for the plugin write (operation, plugin type, plugin id, tenant, principal, outcome) - `inst-pcre-12`
9. [x] - `p1` - **RETURN** `201` with the stored representation carrying the identifier `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`, or `400` or `409` as described above - `inst-pcre-13`

### List and Read Plugin Definitions

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-list-read`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor lists definitions with `GET /oagw/v1/plugins` and the OData parameters `$filter`,
  `$select`, `$top` and `$skip`, and receives `200` with the tenant-scoped, filtered page — for
  example `$filter type eq 'guard'` returns only the guard definitions of the calling tenant.
- An actor reads a single definition by its anonymous GTS identifier
  `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` and receives `200` with the stored representation.
- A builtin plugin is not addressable through this flow: it is resolved through its registry and has
  no stored row, so no builtin identifier ever appears in a list response.

**Error Scenarios**:
- `404` when `{id}` does not resolve to a definition of the calling tenant, including a definition
  owned by another tenant, a deleted identifier and a catalog-only identifier, which has no row.
- `400` for a malformed identifier, an identifier whose type prefix does not match the stored
  `plugin_type`, an unsupported `$filter` operator, an unknown `$select` field, an out-of-range
  `$top` or a negative `$skip`.
- `401` when the Bearer token is missing or invalid.

**Steps**:
1. [x] - `p1` - Actor sends a list or read request for plugin definitions - `inst-plst-01`
2. [x] - `p1` - API: `GET /oagw/v1/plugins` or `GET /oagw/v1/plugins/{id}` authenticates the Bearer token and requires the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission - `inst-plst-02`
3. [x] - `p1` - Resolve the calling tenant and restrict the collection to that tenant's own definitions before any filtering or paging - `inst-plst-03`
4. [x] - `p1` - **IF** the request targets a single identifier - `inst-plst-04`
   1. [x] - `p1` - Classify it with `cpt-cf-oagw-algo-plugin-identifier-resolve`: a UUID-backed identifier is looked up in the plugin store, a named identifier has no row and resolves as missing - `inst-plst-05`
5. [x] - `p1` - **ELSE** - `inst-plst-06`
   1. [x] - `p1` - Translate the query parameters with `cpt-cf-oagw-algo-plugin-list-query`; an unsupported expression is **RETURN**ed as `400` - `inst-plst-07`
6. [x] - `p1` - Project the page onto the `$select` fields when given, and onto the full stored representation otherwise - `inst-plst-08`
7. [x] - `p1` - Never include credential material or resolved secret values in any projected response - `inst-plst-09`
8. [x] - `p1` - **RETURN** `200` with the definition or the page, or `400` or `404` as described above - `inst-plst-10`

### Retrieve Custom Plugin Source

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-source-read`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor sends `GET /oagw/v1/plugins/{id}/source` for a custom definition and receives `200` with
  the stored `source_code` returned verbatim as the response body, so a caller can inspect exactly
  what was stored.
- The source is returned for a definition regardless of whether any upstream or route references it,
  because the read is a storage read and not a resolution of an executable plugin.

**Error Scenarios**:
- `404` when `{id}` does not resolve to a definition of the calling tenant, including a named
  builtin plugin, which has no stored source.
- `401` when the Bearer token is missing or invalid.

**Steps**:
1. [x] - `p1` - Actor sends the source read request for an existing definition - `inst-psrc-01`
2. [x] - `p1` - API: `GET /oagw/v1/plugins/{id}/source` authenticates the Bearer token and requires the `gts.cf.core.oagw.{type}_plugin.v1~:read` permission - `inst-psrc-02`
3. [x] - `p1` - Classify `{id}` with `cpt-cf-oagw-algo-plugin-identifier-resolve` and look the definition up in the calling tenant's key space - `inst-psrc-03`
4. [x] - `p1` - **IF** the identifier does not resolve to a stored definition, including a named builtin plugin - `inst-psrc-04`
   1. [x] - `p1` - **RETURN** `404` with the not-found problem body; a foreign definition is indistinguishable from a missing one - `inst-psrc-05`
5. [x] - `p1` - **ELSE** - `inst-psrc-06`
   1. [x] - `p1` - **RETURN** `200` with the stored `source_code` verbatim as the body, with the media type `text/plain; charset=utf-8` the registered OpenAPI operation records and no JSON envelope around it - `inst-psrc-07`
6. [x] - `p1` - Treat the execution of that source as out of scope: no interpreter is invoked on this path (DECOMPOSITION assumption 4) - `inst-psrc-08`

### Delete Plugin Definition

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-delete`

**Actor**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- An actor deletes an unlinked definition with `DELETE /oagw/v1/plugins/{id}` and receives `204`;
  the `oagw_plugin` row disappears from the store.
- After the last referencing binding is removed through an upstream or route replace or delete in
  entry 2.2, a repeated delete of the same definition succeeds with `204`.

**Error Scenarios**:
- `409` with the `PluginInUse` problem body when the definition is still referenced by an upstream
  or route plugin binding or by an upstream `auth` reference; the body carries the GTS `type`
  identifier `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` and the extension keys `plugin_id`
  and `referenced_by`, and the store is left untouched.
- `404` when `{id}` does not resolve to a definition of the calling tenant, including a
  catalog-only identifier and a named builtin plugin, which cannot be deleted because it has no row.
- `401` when the Bearer token is missing or invalid, and `403` when the `delete` permission of the
  definition's `plugin_type` is not granted.

**Steps**:
1. [x] - `p1` - Actor sends the delete request - `inst-pdel-01`
2. [x] - `p1` - API: `DELETE /oagw/v1/plugins/{id}` authenticates the Bearer token and requires the `gts.cf.core.oagw.{type}_plugin.v1~:delete` permission - `inst-pdel-02`
3. [x] - `p1` - Classify `{id}` with `cpt-cf-oagw-algo-plugin-identifier-resolve` and resolve the definition inside the calling tenant's key space - `inst-pdel-03`
4. [x] - `p1` - **IF** the identifier does not resolve to a stored definition of this tenant - `inst-pdel-04`
   1. [x] - `p1` - **RETURN** `404` with the not-found problem body; a named builtin plugin and a catalog-only identifier resolve as missing - `inst-pdel-05`
5. [x] - `p1` - **ELSE** - `inst-pdel-06`
   1. [x] - `p1` - Run `cpt-cf-oagw-algo-plugin-in-use-scan` over the upstream and route plugin bindings and the upstream auth-plugin references inside the same critical section as the delete - `inst-pdel-07`
   2. [x] - `p1` - **IF** at least one live reference resolves to the definition - `inst-pdel-08`
      1. [x] - `p1` - Discard the staged delete and **RETURN** `409` with `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`, `plugin_id` set to the definition's identifier and `referenced_by` listing every referencing resource identifier with its binding position - `inst-pdel-09`
   3. [x] - `p1` - **ELSE** - `inst-pdel-10`
      1. [x] - `p1` - Store: delete the `oagw_plugin` row in the same atomic unit as the scan, so no binding can start referencing the definition between the check and the write - `inst-pdel-11`
      2. [x] - `p1` - Publish the new store snapshot so the data plane never resolves a chain through a deleted definition - `inst-pdel-12`
6. [x] - `p1` - Emit the structured log line for the plugin write - `inst-pdel-13`
7. [x] - `p1` - **RETURN** `204` with no body, or `404` or `409` as described above - `inst-pdel-14`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are the
reusable building blocks called by the actor flows above and, for binding validation, by the
upstream and route write path of entry 2.2 — its create, replace and store-invariant algorithms call
`cpt-cf-oagw-algo-plugin-binding-validate` before a binding row set is committed.

### Builtin Catalog and Registry Construction

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-catalog-register`

**Input**: the initialized gear state (the loaded configuration of entry 2.1 and the store of entry
2.2) and the builtin plugin set the PRD enumerates.

**Output**: the three registries with their resolvable identifiers, the catalog-only identifier set,
and the registered type schemas.

**Steps**:
1. [x] - `p1` - Register the three plugin base types with `cpt-cf-oagw-actor-types-registry`: `gts.cf.core.oagw.auth_plugin.v1~`, `gts.cf.core.oagw.guard_plugin.v1~` and `gts.cf.core.oagw.transform_plugin.v1~` - `inst-pcrg-01`
2. [x] - `p1` - Build `AuthPluginRegistry` with the four resolvable auth plugins `noop`, `apikey`, `oauth2_client_cred` and `oauth2_client_cred_basic`, the last two being the Form and Basic client-auth-method variants of `OAuth2ClientCredAuthPlugin` that ADR 0008 registers in the same constructor - `inst-pcrg-02`
3. [x] - `p1` - Build `GuardPluginRegistry` with the single resolvable guard plugin `required_headers`, the only guard identifier bindable through `plugins.items[].plugin_ref` - `inst-pcrg-03`
4. [x] - `p1` - Build the transform registry with the single resolvable transform plugin `request_id`, the X-Request-ID injection and propagation plugin - `inst-pcrg-04`
5. [x] - `p1` - Record the catalog-only identifier set with no registry entry: `basic` and `bearer` for auth, `timeout` and `cors` for guards, `logging` and `metrics` for transforms - `inst-pcrg-05`
6. [x] - `p1` - Register each catalog-only identifier as a reserved GTS catalog entry so a caller that uses one as `auth.plugin_type` fails with `unknown auth plugin` and a caller that binds one through `plugins.items[].plugin_ref` is refused - `inst-pcrg-06`
7. [x] - `p1` - **FOR EACH** resolvable identifier in the three registries - `inst-pcrg-07`
   1. [x] - `p1` - Record the mapping from the full GTS identifier `gts.cf.core.oagw.{type}_plugin.v1~cf.core.oagw.{name}.v1` to the named registry key, so classification and resolution agree - `inst-pcrg-08`
8. [x] - `p1` - Keep the registries in `infra/plugin/` and the plugin trait definitions in `domain/plugin/` per the layering of `cpt-cf-oagw-component-model`, so the domain layer holds no registry dependency - `inst-pcrg-09`
9. [x] - `p1` - Store the registries on the gear state as read-only for the lifetime of the process: no management operation mutates a registry, because builtin plugins are not stored, not addressable and not subject to deletion - `inst-pcrg-10`
10. [x] - `p1` - **CATCH** a registry construction or catalog registration failure - `inst-pcrg-11`
   1. [x] - `p1` - Fail the gear init step with the offending identifier named, so host startup aborts instead of serving a plugin contract whose catalog is incomplete - `inst-pcrg-12`
11. [x] - `p1` - **RETURN** the three registries, the catalog-only identifier set and the registered type schemas - `inst-pcrg-13`

### Plugin Identifier Classification

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-identifier-resolve`

**Input**: a plugin reference in API form — an `auth.plugin_type` value, a
`plugins.items[].plugin_ref` value, or a path `{id}` parameter — and the expected plugin type when
one is known.

**Output**: the classification of the identifier (named builtin, UUID-backed custom, or catalog-only)
with the extracted instance part, or the `400` / `404` outcome to map.

**Steps**:
1. [x] - `p1` - Parse the value as an anonymous GTS identifier of the form `gts.cf.core.oagw.{type}_plugin.v1~{instance}`, extracting the base type and the instance part after `~` - `inst-pidr-01`
2. [x] - `p1` - Accept a bare UUID `{id}` path parameter by inferring the base type from the request path, resolve the `oagw_plugin` row by that `id` and require its stored `plugin_type` to match the inferred type, else fail as the list-and-read flow specifies: `404` for a missing row and `400` for a mismatched `plugin_type` - `inst-pidr-12`
3. [x] - `p1` - Fail with `400` when the value does not parse, when the base type is none of `auth_plugin`, `guard_plugin` and `transform_plugin`, or when the base type does not match the expected plugin type of the slot it is used in - `inst-pidr-02`
4. [x] - `p1` - **IF** the instance part parses as a UUID - `inst-pidr-03`
   1. [x] - `p1` - Classify the identifier as UUID-backed and require the `oagw_plugin` row with that `id` to exist and to carry the matching `plugin_type`; a mismatched type fails with `400` at bind time and with `400` on a direct read, and a missing row fails with `404` on a direct read - `inst-pidr-04`
5. [x] - `p1` - **ELSE IF** the instance part is `cf.core.oagw.{name}.v1` - `inst-pidr-05`
   1. [x] - `p1` - Classify the identifier as named and resolve it through the registry of its base type: auth through `noop`, `apikey`, `oauth2_client_cred` and `oauth2_client_cred_basic`, guard through `required_headers`, transform through `request_id` - `inst-pidr-06`
   2. [x] - `p1` - **IF** the name is one of `basic`, `bearer`, `timeout`, `cors`, `logging` or `metrics` - `inst-pidr-07`
      1. [x] - `p1` - Classify it as catalog-only: registered in the types-registry catalog with no backing implementation, not resolvable through any registry and not bindable, so `basic` or `bearer` as `auth.plugin_type` fails with `unknown auth plugin` and the guard and transform catalog identifiers fail the binding check - `inst-pidr-08`
6. [x] - `p1` - **ELSE** - `inst-pidr-09`
   1. [x] - `p1` - Classify the identifier as unknown and fail with `400` unknown plugin, naming the offending value - `inst-pidr-10`
7. [x] - `p1` - **RETURN** the classification with the extracted instance part, which the binding validator stores as `plugin_ref` and, for a UUID-backed plugin, as the matching `plugin_uuid` - `inst-pidr-11`

### Plugin Definition Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-definition-validate`

**Input**: the plugin create body and the calling tenant.

**Output**: a typed plugin definition with the server-managed fields applied, or the first validation
failure with the offending field.

**Steps**:
1. [x] - `p1` - Require `plugin_type`, `name` and `source_code`, and reject unknown properties, since no shipped plugin schema exists and the accepted field set is the deviation record of section 1.2 - `inst-pdef-01`
2. [x] - `p1` - Require `plugin_type` to be `auth`, `guard` or `transform`, and derive the GTS base type and the permission string `gts.cf.core.oagw.{type}_plugin.v1~:{create;read;delete}` from it - `inst-pdef-02`
3. [x] - `p1` - Require `name` to be a non-empty string unique per tenant, the uniqueness itself being enforced by the store's `UNIQUE (tenant_id, name)` - `inst-pdef-03`
4. [x] - `p1` - Require `config_schema`, when present, to be a JSON object and store it verbatim as the declared configuration contract of the plugin - `inst-pdef-04`
5. [x] - `p1` - Require every `phases` entry, when the field is present, to be `on_request`, `on_response` or `on_error`, and store the declared phase set - `inst-pdef-05`
6. [x] - `p1` - Require `source_code` to be a string and store it verbatim, with no syntax check and no execution, per assumption 4 - `inst-pdef-06`
7. [x] - `p1` - Carry no credential material: the definition and its `config_schema` hold configuration metadata only, and no `cred://` reference is resolved on this path - `inst-pdef-07`
8. [x] - `p1` - Apply the server-managed fields: `id` as a generated UUID, `tenant_id` from the security context, and `last_used_at` and `gc_eligible_at` left unset because no usage tracking or GC job exists in this deployment (deviation record in section 1.2) - `inst-pdef-08`
9. [x] - `p1` - Treat the resulting definition as immutable: the validator is called on create only, because no replace operation exists for a plugin - `inst-pdef-09`
10. [x] - `p1` - **TRY** to build the typed plugin definition - `inst-pdef-10`
11. [x] - `p1` - **CATCH** a validation failure - `inst-pdef-11`
   1. [x] - `p1` - Return the offending field and reason to the caller flow for the entry-2.1 mapping layer - `inst-pdef-12`
12. [x] - `p1` - **RETURN** the typed plugin definition - `inst-pdef-13`

### Plugin Binding Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-binding-validate`

**Input**: the binding row set of one upstream or route write — each row with its `position`,
`plugin_ref`, `plugin_uuid` and `config` — and, for an upstream write, the scalar
`auth_plugin_ref` / `auth_plugin_uuid` pair.

**Output**: the accepted binding row set with the normalized `plugin_ref` / `plugin_uuid` pairing, or
the first validation failure to map as `400`.

**Steps**:
1. [x] - `p1` - Classify every `plugin_ref` with `cpt-cf-oagw-algo-plugin-identifier-resolve` before any row is written - `inst-pbnd-01`
2. [x] - `p1` - **FOR EACH** binding row in the set - `inst-pbnd-02`
   1. [x] - `p1` - Require the base type of the `plugin_ref` to be `guard_plugin` or `transform_plugin`, and reject an `auth_plugin` reference in the chain with `400`, because `plugins.items[]` carries guard and transform plugins and the auth plugin lives in the upstream's scalar `auth` field - `inst-pbnd-03`
   2. [x] - `p1` - Require the referenced plugin to resolve: a named plugin through its registry, a UUID-backed plugin through a stored definition whose `plugin_type` matches the base type of the reference - `inst-pbnd-04`
   3. [x] - `p1` - Refuse a catalog-only identifier (`timeout`, `cors`, `logging`, `metrics`) with `400`, since those identifiers are not bindable through `plugins.items[].plugin_ref` - `inst-pbnd-05`
   4. [x] - `p1` - Store `plugin_ref` always, and store `plugin_uuid` only for a UUID-backed plugin, with the two values matching when both are present - `inst-pbnd-06`
3. [x] - `p1` - Require the `position` values of the written row set to be contiguous from `0` in the order the items appear, with no gap and no duplicate - `inst-pbnd-07`
4. [x] - `p1` - For an upstream write, classify the scalar `auth_plugin_ref` the same way and require `auth_plugin_uuid` to be `NULL` for a named plugin and to equal the instance UUID of the reference for a custom plugin - `inst-pbnd-08`
5. [x] - `p1` - Refuse `basic` or `bearer` as `auth.plugin_type` with the `unknown auth plugin` failure, per the catalog-only rule of the auth registry - `inst-pbnd-09`
6. [x] - `p1` - Keep the `config` value of a binding as declared and perform no validation of it against the referenced plugin's `config_schema`, per the recorded non-goal of section 1.2 - `inst-pbnd-10`
7. [x] - `p1` - Leave the read-time composition of the effective chain (upstream bindings before route bindings, inherited ancestor plugins appended) to entry 2.5; this algorithm validates the stored rows only - `inst-pbnd-11`
8. [x] - `p1` - **CATCH** a binding failure - `inst-pbnd-12`
   1. [x] - `p1` - Return the offending position and reason to the calling write path of entry 2.2 for the entry-2.1 mapping layer, leaving the store untouched - `inst-pbnd-13`
9. [x] - `p1` - **RETURN** the normalized binding row set - `inst-pbnd-14`

### Plugin-in-Use Conflict Detection

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-in-use-scan`

**Input**: the `id` and `plugin_ref` of the definition about to be deleted, and the store's upstream
and route records with their binding rows and auth references.

**Output**: the list of live references, or the empty result that lets the delete proceed.

**Steps**:
1. [x] - `p1` - Take the store's write lock for the affected tenant key, so the scan and the delete observe the same snapshot - `inst-pinu-01`
2. [x] - `p1` - Scan `oagw_upstream_plugin` and `oagw_route_plugin` for rows whose `plugin_ref` or `plugin_uuid` resolves to the definition - `inst-pinu-02`
3. [x] - `p1` - Scan the upstream `auth_plugin_ref` / `auth_plugin_uuid` columns for a reference to the definition, since the auth plugin identity is a scalar column precisely so this check does not depend on JSON scanning - `inst-pinu-03`
4. [x] - `p1` - Collect for each live reference the referencing resource type, its identifier and the binding `position` or the `auth` position, and nothing else: the result names where the definition is used and carries no configuration content - `inst-pinu-04`
5. [x] - `p1` - Match on the stored reference columns only, so a definition is never reported as used because of a string that merely resembles its identifier - `inst-pinu-05`
6. [x] - `p1` - Treat the references found as within the calling tenant's key space by construction: binding validation resolves a reference inside the caller's tenant key space, so no foreign reference exists to find - `inst-pinu-06`
7. [x] - `p1` - **IF** the reference list is empty - `inst-pinu-07`
   1. [x] - `p1` - Allow the delete to proceed in the same critical section - `inst-pinu-08`
8. [x] - `p1` - **ELSE** - `inst-pinu-09`
   1. [x] - `p1` - Discard the staged delete and return the reference list to the calling flow, which maps it to `409` with `plugin_id` and `referenced_by` - `inst-pinu-10`
9. [x] - `p1` - Perform no garbage collection and set no `gc_eligible_at` value: the GC job of the DESIGN lifecycle is out of scope in this deployment (deviation record in section 1.2) - `inst-pinu-11`
10. [x] - `p1` - **RETURN** the reference list - `inst-pinu-12`

### Plugin List Query Translation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-list-query`

**Input**: the query parameters `$filter`, `$select`, `$top` and `$skip`, and the tenant-scoped
plugin collection.

**Output**: the projected and paged list, or a `400` for an unsupported expression.

**Steps**:
1. [x] - `p1` - Scope the collection to the calling tenant before any filtering or paging - `inst-pqry-01`
2. [x] - `p1` - Apply the entry-2.2 OData translation rules of `cpt-cf-oagw-algo-odata-query` to the plugin field set, so one translation implementation serves both management surfaces - `inst-pqry-02`
3. [x] - `p1` - Parse `$filter` over the plugin fields, supporting the comparison form the DESIGN documents, for example `type eq 'guard'` - `inst-pqry-03`
4. [x] - `p1` - Reject an unsupported operator or an unknown field with `400` - `inst-pqry-04`
5. [x] - `p1` - Parse `$select` as a field list and reject an unknown field with `400` - `inst-pqry-05`
6. [x] - `p1` - Parse `$top` and `$skip` with the same recorded bounds the upstream and route lists use, a `$top` default of `50`, a cap of `100` and a non-negative `$skip`, and reject an out-of-range value with `400` - `inst-pqry-06`
7. [x] - `p1` - Support the four parameters the DESIGN declares for the plugin list (`$filter`, `$select`, `$top`, `$skip`) and no `$orderby`, which the DESIGN declares only for the upstream and route lists - `inst-pqry-07`
8. [x] - `p1` - Apply filtering, then offset and limit, so paging is stable for a given tenant - `inst-pqry-08`
9. [x] - `p1` - Never include credential material in a projected response - `inst-pqry-09`
10. [x] - `p1` - **RETURN** the page - `inst-pqry-10`

### Plugin Store Write and Invariant Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-store-write`

**Input**: a validated change (insert or delete) for one tenant's plugin definition, or a binding row
set handed over by the entry-2.2 write path.

**Output**: the committed record or the invariant violation that maps to `409` or `400`.

**Steps**:
1. [x] - `p1` - Take the store's write lock for the affected tenant key - `inst-pstr-01`
2. [x] - `p1` - Check `UNIQUE (tenant_id, name)` for a plugin insert inside the same critical section as the write - `inst-pstr-02`
3. [x] - `p1` - Check the binding invariants for a binding write through `cpt-cf-oagw-algo-plugin-binding-validate`: positions contiguous from 0, `plugin_ref` always stored, `plugin_uuid` stored only for UUID-backed plugins and matching when present - `inst-pstr-03`
4. [x] - `p1` - Apply the whole change as one atomic unit over the `oagw_plugin` row and any affected binding rows, so a rejected invariant or a detected in-use conflict leaves no partial write behind - `inst-pstr-04`
5. [x] - `p1` - Stamp the definition's `id` and `tenant_id` on insert and never change them afterwards, since the definition is immutable - `inst-pstr-05`
6. [x] - `p1` - Publish the new immutable store snapshot so a reader sees the previous or the new catalog and never an intermediate one - `inst-pstr-06`
7. [x] - `p1` - Invalidate the consumer caches after a successful write, so entry 2.5 does not resolve a chain through a stale snapshot - `inst-pstr-07`
8. [x] - `p1` - Keep `cpt-cf-oagw-db-schema` as the logical model with the DESIGN table and column names, on the crate's existing `dashmap`/`parking_lot`/`arc-swap` dependency set with no `toolkit-db` dependency and no SQL (assumption 3) - `inst-pstr-08`
9. [x] - `p1` - **CATCH** an invariant violation - `inst-pstr-09`
   1. [x] - `p1` - Discard the staged change, release the lock, and return the violation to the calling flow for the entry-2.1 mapping layer - `inst-pstr-10`
10. [x] - `p1` - **RETURN** the committed record or the confirmed deletion - `inst-pstr-11`

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

The only lifecycle this feature owns is the one of a stored custom plugin definition. Named builtin
plugins have no lifecycle here — they are not stored, not addressable and not subject to deletion —
and the runtime states of the request path (the resolved plugin chain and its execution) belong to
entries 2.4 and 2.5.

### Plugin Definition State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-plugin-lifecycle`

**States**: `absent`, `available`, `in_use`

**Initial State**: `absent`

**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `available` **WHEN** a create passes validation and the store's `UNIQUE (tenant_id, name)` check - `inst-psta-01`
2. [x] - `p1` - **FROM** `available` **TO** `in_use` **WHEN** an upstream or route write of entry 2.2 stores a binding row that resolves to the definition, or an upstream write stores an `auth_plugin_uuid` reference to it - `inst-psta-02`
3. [x] - `p1` - **FROM** `in_use` **TO** `available` **WHEN** the last referencing binding or auth reference is removed by an entry-2.2 replace or delete - `inst-psta-03`
4. [x] - `p1` - **FROM** `available` **TO** `absent` **WHEN** a delete removes the definition and returns `204` - `inst-psta-04`
5. [x] - `p1` - **FROM** `in_use` **TO** `absent` **WHEN** a delete arrives and the in-use scan finds no live reference any more - `inst-psta-05`

**Closed transition set**: the transitions above are the only ones possible. A delete that finds a
live reference causes no transition — the definition stays `in_use` and the caller receives `409` —
and a failed validation or a rejected invariant leaves the definition in its current state. No state
is skipped and no state is re-entered on its own. `in_use` is a derived condition computed by
`cpt-cf-oagw-algo-plugin-in-use-scan` over the store's binding rows and auth references, not a stored
column: nothing writes it, and the `last_used_at` and `gc_eligible_at` columns of the logical model
stay unset in this deployment because usage tracking and the GC job are out of scope. A definition
never changes its `plugin_type`, `name` or `source_code`, because no replace operation exists.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Plugin Type Catalog and GTS Identifier Patterns

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-type-catalog`

The system **MUST** register the three plugin types `auth`, `guard` and `transform` with
`cpt-cf-oagw-actor-types-registry` through the contract `cpt-cf-oagw-contract-types-registry`, base
identifiers `gts.cf.core.oagw.auth_plugin.v1~`, `gts.cf.core.oagw.guard_plugin.v1~` and
`gts.cf.core.oagw.transform_plugin.v1~`, **MUST** accept a plugin identifier only in the anonymous
GTS form `gts.cf.core.oagw.{type}_plugin.v1~{instance}` with the instance being either the named
form `cf.core.oagw.{name}.v1` or a UUID, **MUST** register the reserved catalog-only identifiers
`basic` and `bearer` (auth), `timeout` and `cors` (guards) and `logging` and `metrics` (transforms)
as catalog entries with no backing implementation, and **MUST** fail gear init with the offending
identifier named when a type schema or a reserved identifier cannot be registered.

**Implements**:
- `cpt-cf-oagw-flow-plugin-catalog-bootstrap`
- `cpt-cf-oagw-algo-plugin-catalog-register`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none — registration happens on the gear init path, not on an endpoint
- Entities: `Plugin` (type catalog only)

### Builtin Plugin Registries

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-builtin-registries`

The system **MUST** construct `AuthPluginRegistry::with_builtins` with `noop`, `apikey`,
`oauth2_client_cred` and `oauth2_client_cred_basic` (the Form and Basic client-auth-method variants
of `OAuth2ClientCredAuthPlugin` per ADR 0008), `GuardPluginRegistry::with_builtins` with
`required_headers` and the transform registry with `request_id`, all in the `infra/plugin/` location
of `cpt-cf-oagw-component-model`, **MUST** keep the registries read-only for the process lifetime and
keyed by the named identifier each full GTS identifier maps to, **MUST** leave `basic`, `bearer`,
`timeout`, `cors`, `logging` and `metrics` outside every registry so they are registered but not
resolvable, and **MUST** reject `basic` or `bearer` as `auth.plugin_type` with the
`unknown auth plugin` failure.

**Implements**:
- `cpt-cf-oagw-flow-plugin-catalog-bootstrap`
- `cpt-cf-oagw-algo-plugin-catalog-register`
- `cpt-cf-oagw-algo-plugin-identifier-resolve`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: none — the registries are resolved by entry 2.5, not exposed as endpoints
- Entities: `Plugin` (named builtin plugins; not stored, not subject to deletion)

### Plugin Definition Model

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-model`

The system **MUST** define the `Plugin` model with the field set the DESIGN class and the ADR 0002
definition examples fix (deviation record in section 1.2) — `id`, `tenant_id`, `plugin_type` in
`auth|guard|transform`, `name`, `description`, `config_schema`, `phases` in
`on_request|on_response|on_error`, `source_code`, `last_used_at`, `gc_eligible_at` — with `id`,
`tenant_id`, `last_used_at` and `gc_eligible_at` server-managed and the last two left unset in this
deployment, **MUST** key the stored definition by `id` under `UNIQUE (tenant_id, name)`, **MUST**
scope every definition to the calling tenant, **MUST** treat the whole definition as immutable after
creation, and **MUST** carry no credential material, only configuration metadata and script source.

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-algo-plugin-definition-validate`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`
- DB: `cpt-cf-oagw-db-schema` (logical model only — in-memory store, DECOMPOSITION assumption 3)
- Entities: `Plugin`

### Plugin Management Endpoints

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-endpoints`

The system **MUST** register `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`,
`GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}` and `GET /oagw/v1/plugins/{id}/source`
under the gear mount root with the status codes `201` create, `200` get/list/source and `204`
delete, **MUST** authenticate each call and require the
`gts.cf.core.oagw.{type}_plugin.v1~:{create;read;delete}` permission derived from the definition's or
the request's `plugin_type`, **MUST** register no other endpoint for plugins, **MUST** register the
operations and schemas in the host OpenAPI registry, and **MUST** return every failure through the
entry-2.1 mapping layer as `application/problem+json` with `X-OAGW-Error-Source: gateway` — `400`
for a validation failure, `404` for an out-of-tenant, unknown or catalog-only identifier, and `409`
`PluginInUse` for a referenced definition.

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-flow-plugin-list-read`
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-algo-plugin-list-query`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Plugin`

### Plugin Immutability

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-immutability`

The system **MUST** register no replace operation for a plugin definition — `PUT /oagw/v1/plugins/{id}`
is not a registered operation, so a replace request resolves to the router's method-not-allowed
response rather than to a stored mutation — **MUST** keep `plugin_type`, `name`, `config_schema`,
`phases` and `source_code` unchanged for the lifetime of the definition, **MUST NOT** provide a
replace operation for plugin definitions — changed behaviour is obtained by creating a new definition
and re-binding referencing upstreams and routes through entry 2.2's replace operation — and **MUST**
leave the delete of the superseded definition to `cpt-cf-oagw-flow-plugin-delete` once no reference
remains.

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-state-plugin-lifecycle`
- `cpt-cf-oagw-algo-plugin-definition-validate`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: the plugin endpoints without a `PUT` operation
- DB: `cpt-cf-oagw-db-schema` (`oagw_plugin`)
- Entities: `Plugin`

### Plugin Source Endpoint

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-source-endpoint`

The system **MUST** serve `GET /oagw/v1/plugins/{id}/source` with the stored `source_code` returned
verbatim as the response body, with no JSON envelope around it and with the media type
`text/plain; charset=utf-8` the registered OpenAPI operation records — a non-HTML type, so the stored
source is never served as an active-content vector — **MUST** return `404` when the identifier does
not resolve to a stored definition of the calling tenant, including a named builtin plugin, which has
no stored source, and
**MUST** treat the source as a storage and contract artifact only: no interpreter is invoked, no
syntax check is performed and no sandbox is built on this path (DECOMPOSITION assumption 4), so the
sandbox thresholds of `cpt-cf-oagw-nfr-starlark-sandbox` are recorded as the execution-time contract
and not implemented here.

**Implements**:
- `cpt-cf-oagw-flow-plugin-source-read`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}/source`
- DB: `cpt-cf-oagw-db-schema` (`oagw_plugin.source_code`)
- Entities: `Plugin`

### Plugin-in-Use Conflict Detection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-in-use-conflict`

The system **MUST** scan `oagw_upstream_plugin`, `oagw_route_plugin` and the upstream
`auth_plugin_ref` / `auth_plugin_uuid` columns for references to a definition inside the same write
critical section as its delete, **MUST** return `409` with an `application/problem+json` body whose
`type` is `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`, whose `plugin_id` carries the
definition's GTS identifier and whose `referenced_by` lists every referencing resource identifier
with its binding or auth position (deviation record in section 1.2), **MUST** leave the store
untouched when the conflict is detected, **MUST NOT** depend on JSON scanning for the auth reference
and **MUST** succeed with `204` on a later delete once the last referencing binding or auth reference
has been removed.

**Implements**:
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-algo-plugin-in-use-scan`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-plugin-immutable`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}` (`409` failure response, `application/problem+json`)
- DB: `cpt-cf-oagw-db-schema` (`oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_plugin`)
- Entities: `Plugin`, `PluginBinding`

### Plugin Binding Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-binding-validation`

The system **MUST** validate every upstream and route plugin binding row set on write through
`cpt-cf-oagw-algo-plugin-binding-validate` — positions contiguous from 0 with no gap and no
duplicate, `plugin_ref` always stored, `plugin_uuid` stored only for a UUID-backed plugin and equal
to the reference's instance part when both are present, a named row keeping `plugin_uuid` `NULL` —
**MUST** reject a binding whose reference does not resolve, whose base type does not match the
referenced definition's `plugin_type`, whose reference is a catalog-only identifier, or whose
`auth_plugin` reference is placed in `plugins.items[]` instead of the upstream's scalar `auth` field,
**MUST** reject `basic` or `bearer` as `auth.plugin_type` with the `unknown auth plugin` failure, and
**MUST** keep the `config` value of a binding as declared without validating it against
`config_schema` (recorded non-goal in section 1.2). The validation runs on the write path entry 2.2
drives; the request-time resolution of a binding into an executable plugin is entry 2.5.

**Implements**:
- `cpt-cf-oagw-algo-plugin-binding-validate`
- `cpt-cf-oagw-algo-plugin-identifier-resolve`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: the upstream and route create and replace operations of entry 2.2, which call this validation
- DB: `cpt-cf-oagw-db-schema` (`oagw_upstream_plugin`, `oagw_route_plugin`, upstream `auth_plugin_ref` / `auth_plugin_uuid`)
- Entities: `PluginBinding`, `PluginsConfig`, `Plugin`

### Tenant Scoping of Plugin Definitions

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-tenant-scoping`

The system **MUST** scope every plugin read and write to the calling tenant resolved from the
security context, **MUST** key every `oagw_plugin` row by `tenant_id` and enforce
`UNIQUE (tenant_id, name)` inside the write critical section, **MUST** present a foreign definition
as `404` on get, source and delete, and **MUST** keep the identifier namespace tenant-neutral: a
definition of another tenant never resolves for this caller, so a foreign resource is
indistinguishable from a missing one.

**Implements**:
- `cpt-cf-oagw-flow-plugin-list-read`
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`

**Touches**:
- API: every plugin endpoint under `/oagw/v1/plugins`
- DB: `cpt-cf-oagw-db-schema` (tenant-keyed lookups)
- Entities: `Plugin`

### In-Memory Plugin Store and Logical Invariants

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-store-invariants`

The system **MUST** hold the plugin tables in the store entry 2.2 introduces, on the crate's existing
`dashmap`/`parking_lot`/`arc-swap` dependency set with no `toolkit-db` dependency and no SQL,
**MUST** keep `cpt-cf-oagw-db-schema` as the logical model with the DESIGN table and column names —
`oagw_plugin` with `UNIQUE (tenant_id, name)`, `oagw_upstream_plugin` and `oagw_route_plugin` keyed
by `(parent_id, position)` with no FK to `oagw_plugin` — **MUST** enforce the plugin-side invariants
inside a single write critical section, **MUST** apply a change atomically, **MUST** publish a new
immutable snapshot and invalidate the consumer caches after each successful write, and **MUST** hold
no `gc_eligible_at` writer, because the GC job is out of scope in this deployment (deviation record
in section 1.2).

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-algo-plugin-store-write`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: every plugin write endpoint
- DB: `cpt-cf-oagw-db-schema` (logical model only, DECOMPOSITION assumption 3)
- Entities: `Plugin`, `PluginBinding`

### Test Layering

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside
`#[cfg(test)]` modules per layer for the catalog and registry construction, the identifier
classification, the definition validation, the binding validation, the in-use scan, the list
translation and the store invariants, and integration tests under the crate's `tests/` directory that
boot the gear router and exercise the five endpoints including the `201`/`200`/`204` statuses, the
`409` `PluginInUse` body with its extension keys, the `404` tenant scoping, the immutability of a
stored definition and the error contract — and **MUST NOT** add an e2e suite under
`testing/e2e/gears/oagw/` (DECOMPOSITION assumption 5).

**Implements**:
- `cpt-cf-oagw-flow-plugin-create`
- `cpt-cf-oagw-flow-plugin-delete`
- `cpt-cf-oagw-algo-plugin-binding-validate`
- `cpt-cf-oagw-algo-plugin-in-use-scan`
- `cpt-cf-oagw-state-plugin-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: the five plugin endpoints (asserted by the integration tests)
- Entities: `Plugin`, `PluginBinding`

## 6. Acceptance Criteria

- [x] The gear registers the plugin type schemas for `auth|guard|transform` and the six reserved catalog-only identifiers with the types-registry at startup, and a registration failure aborts host startup with the offending identifier named.
- [x] The builtin registries resolve exactly `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `required_headers` and `request_id`, and no catalog-only identifier resolves through any registry.
- [x] `POST /oagw/v1/plugins` with `plugin_type`, `name` and `source_code` returns `201` with the stored representation and the server-generated identifier `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
- [x] A create with a `plugin_type` outside `auth|guard|transform`, an empty `name`, a non-object `config_schema`, a `phases` value outside `on_request|on_response|on_error` or an unknown property returns `400` with the offending field named.
- [x] A second definition with the same `name` in the same tenant returns `409` with an `application/problem+json` body carrying a GTS `type` identifier, while the same `name` in another tenant is a distinct resource.
- [x] `GET /oagw/v1/plugins` honours `$filter` (for example `type eq 'guard'`), `$select`, `$top` and `$skip`, defaults `$top` to `50`, caps it at `100`, and returns `400` for an unsupported expression.
- [x] `GET /oagw/v1/plugins/{id}` resolves the anonymous GTS identifier form and a bare UUID, returns `404` for a foreign, deleted or catalog-only identifier, and returns `400` when the identifier's base type does not match the stored `plugin_type`.
- [x] `PUT /oagw/v1/plugins/{id}` is not registered; no request can change `plugin_type`, `name`, `config_schema`, `phases` or `source_code` of a stored definition.
- [x] `DELETE /oagw/v1/plugins/{id}` of an unlinked definition returns `204` and removes the row; a delete of a definition still referenced by an upstream or route binding, or by an upstream `auth` reference, returns `409` and leaves the store unchanged.
- [x] The `409` body carries `type` `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`, `plugin_id` set to the definition's identifier and `referenced_by` naming every referencing resource with its binding or auth position.
- [x] After the last referencing binding or auth reference is removed through an entry-2.2 replace or delete, a repeated `DELETE` returns `204`.
- [x] `GET /oagw/v1/plugins/{id}/source` returns the stored `source_code` verbatim with the media type `text/plain; charset=utf-8` and no JSON envelope, a non-HTML type so the stored source is not an active-content vector, and returns `404` for a named builtin plugin.
- [x] A binding write with positions `0,1,3` is refused, a named binding row keeps `plugin_uuid` `NULL`, a custom binding row keeps both fields set and matching, and a binding whose `config` is declared is stored verbatim.
- [x] A binding that references an unresolvable identifier, a catalog-only identifier, a mismatched `plugin_type`, or an `auth_plugin` identifier placed in `plugins.items[]` returns `400`; `basic` or `bearer` as `auth.plugin_type` fails with `unknown auth plugin`.
- [x] Every plugin failure returns `application/problem+json` with the GTS `type` identifier, `title`, `status`, `detail` and `instance`, and every plugin response carries `X-OAGW-Error-Source`.
- [x] No response body, error body or log line contains credential material, and no plugin operation resolves a `cred://` reference.
- [x] The store refuses a write that would break `UNIQUE (tenant_id, name)`, the contiguous-position rule or the `plugin_ref` / `plugin_uuid` pairing, and a reader never observes a partially applied catalog.
- [x] The in-crate unit and integration tests pass with the host feature set used by the graded configuration, and no test artifact is added under `testing/e2e/gears/oagw/`.
