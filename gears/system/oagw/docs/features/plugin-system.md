# Feature: Plugin System


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Domain Applicability](#15-domain-applicability)
  - [1.6 Graded Deviations](#16-graded-deviations)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Custom Plugin Registration](#custom-plugin-registration)
  - [Plugin Catalog Retrieval](#plugin-catalog-retrieval)
  - [Plugin Source Retrieval](#plugin-source-retrieval)
  - [Plugin Deletion With Reference Guard](#plugin-deletion-with-reference-guard)
  - [Plugin Binding Resolution](#plugin-binding-resolution)
  - [Plugin Chain Composition](#plugin-chain-composition)
  - [Plugin Chain Execution](#plugin-chain-execution)
  - [Credential Resolution at Request Time](#credential-resolution-at-request-time)
  - [OAuth2 Token Cache Retrieval](#oauth2-token-cache-retrieval)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Plugin Identifier Resolution](#plugin-identifier-resolution)
  - [Plugin Chain Composition](#plugin-chain-composition-1)
  - [Plugin Chain Execution Order](#plugin-chain-execution-order)
  - [Required Headers Guard Decision](#required-headers-guard-decision)
  - [OAuth2 Token Cache Key and TTL](#oauth2-token-cache-key-and-ttl)
  - [Credential Isolation Enforcement](#credential-isolation-enforcement)
  - [Plugin In-Use Reference Scan](#plugin-in-use-reference-scan)
- [4. States (CDSL)](#4-states-cdsl)
  - [Plugin Record State Machine](#plugin-record-state-machine)
  - [Plugin Chain Execution State Machine](#plugin-chain-execution-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Plugin Traits and Deterministic Execution Order](#plugin-traits-and-deterministic-execution-order)
  - [Plugin Registries With Built-ins](#plugin-registries-with-built-ins)
  - [Built-in Auth Plugins](#built-in-auth-plugins)
  - [OAuth2 Client Credentials Token Cache](#oauth2-client-credentials-token-cache)
  - [Built-in Required Headers Guard Plugin](#built-in-required-headers-guard-plugin)
  - [Built-in Request ID Transform Plugin](#built-in-request-id-transform-plugin)
  - [Plugin Identifier Resolution and Catalog-Only Rejection](#plugin-identifier-resolution-and-catalog-only-rejection)
  - [Plugin Chain Composition and Ordering](#plugin-chain-composition-and-ordering)
  - [Credential Resolution at Request Time](#credential-resolution-at-request-time-1)
  - [Credential Isolation Across Every Surface](#credential-isolation-across-every-surface)
  - [Plugin Management REST API](#plugin-management-rest-api)
  - [Plugin Immutability and Reference-Guarded Deletion](#plugin-immutability-and-reference-guarded-deletion)
  - [Registry-Reference-Only Custom Plugins](#registry-reference-only-custom-plugins)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-plugin-system-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-plugin-system`
## 1. Feature Context

### 1.1 Overview

Implements the three plugin types with trait-based extensibility, the registries that resolve them, the built-in plugins shipped in the `oagw` crate, credential resolution from the credential store, and the plugin management REST API. The plugin chain executes inside the proxy pipeline delivered by `cpt-cf-oagw-feature-request-proxy` at the extension points Auth, Guards, Transform(request), the upstream call, and Transform(response/error), and it resolves every plugin reference against the domain types, the merge engine, and the types-registry catalog delivered by `cpt-cf-oagw-feature-gear-foundation`.

### 1.2 Purpose

The plugin system is the extension point that delivers credential injection, request guarding, and request/response mutation without modifying the gateway core. It exists so that an operator can require headers, inject an API key or an OAuth2 client-credentials token, and propagate a request identifier by binding configuration rather than by changing gateway code. It realizes the decision recorded in `cpt-cf-oagw-adr-plugin-system`, the token-cache decision in `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`, and the guard decision in `cpt-cf-oagw-adr-required-headers-guard-plugin`, and it delivers the plugin half of `cpt-cf-oagw-interface-api` at the gear-relative `/oagw/v1/plugins` prefix.

**Requirements** delivered by this feature:

- `p2` - `cpt-cf-oagw-fr-plugin-system`
- `p2` - `cpt-cf-oagw-fr-builtin-plugins`
- `p1` - `cpt-cf-oagw-fr-auth-injection` - delivered for API Key and OAuth2 Client Credentials; HTTP Basic and Bearer remain catalog-only identifiers per graded deviation 10
- [x] `p1` - `cpt-cf-oagw-nfr-credential-isolation` - secret resolution is owned by this feature; entry 2.1 covers the configuration-boundary slice only
- [x] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox` - covered with the plugin-trait boundary as the sandboxing surface (graded deviation 6)
- `p1` - `cpt-cf-oagw-contract-cred-store`
- [x] `p1` - `cpt-cf-oagw-interface-management-api` - the plugin half of the management REST surface (`/oagw/v1/plugins`)

**Principles**: `cpt-cf-oagw-principle-cred-isolation`, `cpt-cf-oagw-principle-plugin-immutable`

**Constraints**: none. No DESIGN constraint is introduced by this feature; `cpt-cf-oagw-constraint-toolkit-deploy` is covered by entry 2.1 and `cpt-cf-oagw-constraint-https-only` by entry 2.4.

**Sequences**: none. Plugin execution is a set of steps inside `cpt-cf-oagw-seq-proxy-flow`, which is owned by entry 2.4; DESIGN.md defines no separate plugin sequence.

**Components and data**: the plugin registries, the built-in plugin implementations, the plugin chain executor, and the plugin management handlers of `cpt-cf-oagw-component-model` are delivered here, and the plugin-shaped records of `cpt-cf-oagw-db-schema` — `oagw_plugin` with its unique `(tenant_id, name)` key and the ordered `oagw_upstream_plugin` / `oagw_route_plugin` bindings — are materialized as the in-memory, config-backed repository delivered by entry 2.1, with `plugin_ref` always stored and `plugin_uuid` only for UUID-backed plugins. No plugin garbage-collection lifecycle is implemented, so no persisted plugin row exists in the graded configuration.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Owns the built-in plugin set shipped in the crate and the registry posture of that built-in set, and is the actor of the source flow `GET /oagw/v1/plugins/{id}/source`, exercising the `read` element of the permission set of the plugin's base type. The platform-operator does not create, list, or delete a tenant-scoped plugin record |
| `cpt-cf-oagw-actor-tenant-admin` | Creates, lists, reads, and deletes tenant-scoped custom plugin records under `/oagw/v1/plugins` and binds plugin references onto tenant-owned upstreams and routes, holding the `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, and `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` permission sets of the DESIGN §3.2 permission table and exercising the element each operation requires. The tenant-admin does not retrieve plugin source content and does not resolve a built-in catalog entry as a bindable plugin |
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests whose credentials are injected and whose requests and responses are guarded and transformed, with no knowledge of the chain |
| `cpt-cf-oagw-actor-cred-store` | Resolves `cred://` references to secret material at request time and enforces tenant accessibility of each reference |
| `cpt-cf-oagw-actor-types-registry` | Holds the three plugin base types and the catalog-only identifiers that are resolvable as references but never bindable |
| `cpt-cf-oagw-actor-upstream-service` | Receives the authenticated and transformed request and returns the response that the guard and transform plugins inspect |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADRs**: [ADR 0002](../ADR/0002-plugin-system.md) (`cpt-cf-oagw-adr-plugin-system`), [ADR 0008](../ADR/0008-oauth2-client-credentials-auth-plugin.md) (`cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`), [ADR 0009](../ADR/0009-required-headers-guard-plugin.md) (`cpt-cf-oagw-adr-required-headers-guard-plugin`)
- **Dependencies**: `cpt-cf-oagw-feature-request-proxy` (the proxy pipeline and the extension points the chain plugs into), `cpt-cf-oagw-feature-gear-foundation` (transitive, via `cpt-cf-oagw-feature-request-proxy`; the domain types, merge engine, repository boundary and GTS type provisioning are delivered there)

### 1.5 Domain Applicability

The requirement domains below are either owned by another entry or not applicable to this feature; they are named here so no domain is left implicit by the sections that follow:

- **Observability, audit records, and health**: owned by `cpt-cf-oagw-feature-observability-and-operability`. This feature implements no metric registry, no audit emitter, and no health or readiness surface; it only contributes the outcome values, the `error_type` values, and the correlation identifier listed in the observability statement of the chain-execution flow.
- **Error rendering and problem+json bodies**: owned by `cpt-cf-oagw-feature-error-handling`. This feature raises `DomainError` outcomes for the shared error contract and renders no body itself.
- **Proxy path, body, streaming, and scheme posture**: owned by `cpt-cf-oagw-feature-request-proxy`. The chain executes inside that pipeline's extension points, so no connection, scheme, body-size, buffering, or streaming behavior is owned here.
- **Rate-limit rejection semantics**: owned by `cpt-cf-oagw-feature-rate-limiting`. No `429` outcome, `Retry-After` guidance, or limiter state is produced by this feature.
- **CORS response headers**: owned by `cpt-cf-oagw-feature-cors`. No header this feature injects into an outbound request participates in a CORS decision, and no CORS response header is emitted here.
- **Performance**: the per-phase bound of the timeout posture is the only latency behavior this feature owns; the below-10 ms p95 budget of `cpt-cf-oagw-nfr-low-latency` is proxy-scoped and owned by `cpt-cf-oagw-feature-request-proxy`.
- **Integration (INT)**: not applicable as an owned concern because the Control Plane L1 invalidation and the Data Plane hot-config flush that follow a plugin create or delete are delegated to `cpt-cf-oagw-feature-observability-and-operability`, which owns the mechanism.
- **Compliance (COMPL)**: not applicable because no regulatory regime applies to a plugin record, a plugin binding, or a resolved credential value held in memory for the life of one invocation.
- **User experience (UX)**: not applicable because the surface is a REST management API; there is no human-facing UI journey for this entry to cover.
- **Browser accessibility**: not applicable because the gear exposes no browser-rendered surface for this feature to make accessible.

### 1.6 Graded Deviations

Each graded deviation this entry carries is recorded once here with its review owner and the artefact that validates it:

- **Graded deviation 6** (sandboxing surface): `cpt-cf-oagw-nfr-starlark-sandbox` is covered with the plugin-trait boundary as the sandboxing surface and no Starlark or other script interpreter is shipped. owner: oagw gear owner (platform-operator review). validation: `tests/plugin_registry_tests.rs`, alongside the sibling-module coverage of `src/infra/plugin/registry_reference_tests.rs` that asserts no execution path exists for registered source content.
- **Graded deviation 10** (built-in handler set): `basic`, `bearer`, `timeout`, `cors`, `logging`, and `metrics` remain catalog-only identifiers with no handler behind them, so the core timeout, CORS, logging, and metrics behavior is never reachable through a plugin binding. owner: oagw gear owner (platform-operator review). validation: `tests/plugin_registry_tests.rs`.
- **Graded deviation 4** (test placement): every test for this feature lives inside the `oagw` crate and no `testing/e2e/gears/oagw/` directory is created. owner: oagw gear owner (platform-operator review). validation: the declared sibling/integration test layout — the `src/infra/plugin/*_tests.rs` modules and the `tests/` files named by `cpt-cf-oagw-dod-plugin-system-unit-tests` and `cpt-cf-oagw-dod-plugin-system-integration-tests`.

## 2. Actor Flows (CDSL)

**Use cases**: this feature exposes no end-user use case of its own. `cpt-cf-oagw-usecase-configure-upstream` and `cpt-cf-oagw-usecase-configure-route` bind the plugin references resolved here, and `cpt-cf-oagw-usecase-proxy-request` runs the resulting chain.

### Custom Plugin Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-plugin-create`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A custom plugin carrying one of the three plugin base types, a tenant-unique name, a configuration schema, and a source reference is created at `POST /oagw/v1/plugins`, is assigned a server-generated UUID, is addressable as `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`, and is immutable from creation onward.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the `create` element of the permission set of the base type named by `plugin_type` — one of `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}`: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted and no plugin record is created or disclosed.
- `plugin_type` names a base type other than the auth, guard, or transform plugin base types, or names a catalog-only identifier: the create is rejected with a validation error.
- A plugin with the same `(tenant_id, name)` already exists: the create is rejected with `409 Conflict`.
- A required field is absent or the configuration schema is not a valid JSON schema object: the create is rejected with a validation error.

**Steps**:
1. [x] - `p1` - Receive the create request carrying `plugin_type`, `name`, `config_schema`, and the source reference - `inst-ps-create-1`
2. [x] - `p1` - Authorize the caller's security context against the permission set of the plugin base type named by `plugin_type` per the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`) — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and perform the check before the store is consulted so an unauthorized caller fails as a `DomainError` before any lookup - `inst-ps-create-10`
3. [x] - `p1` - Resolve the plugin base type named by `plugin_type` through the types-registry and confirm it is one of `gts.cf.core.oagw.auth_plugin.v1~`, `gts.cf.core.oagw.guard_plugin.v1~`, and `gts.cf.core.oagw.transform_plugin.v1~` - `inst-ps-create-2`
4. [x] - `p1` - **IF** `plugin_type` names a catalog-only identifier or an unknown base type - `inst-ps-create-3`
   1. [x] - `p1` - Reject the create with a validation error naming the offending base type and store nothing - `inst-ps-create-4`
5. [x] - `p1` - Validate that `name` is present and that `config_schema` is a JSON schema object, and that the pair `(tenant_id, name)` is unique within the calling tenant - `inst-ps-create-5`
6. [x] - `p1` - **IF** a plugin with the same `(tenant_id, name)` already exists - `inst-ps-create-6`
   1. [x] - `p1` - Reject the create with a conflict and leave the store unchanged - `inst-ps-create-7`
7. [x] - `p1` - Generate the server-side UUID, persist the plugin record in the in-memory config-backed repository under the calling tenant, and record the source reference without interpreting it as executable content - `inst-ps-create-8`
8. [x] - `p1` - **RETURN** the created plugin addressed by its anonymous GTS identifier, carrying `plugin_type`, `name`, `config_schema`, and the source reference, and carrying no secret material and no resolved credential value - `inst-ps-create-9`

### Plugin Catalog Retrieval

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-plugin-read`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- `GET /oagw/v1/plugins` lists the calling tenant's custom plugins with the OData query parameters `$filter` (for example `type eq 'guard'`), `$select`, `$top`, and `$skip`.
- `GET /oagw/v1/plugins/{id}` returns one plugin by its anonymous GTS identifier.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the `read` element of the permission set of the plugin's base type — one of `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}`: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted and no record is disclosed.
- The identifier belongs to another tenant, including an ancestor: the result is not-found and the foreign record is never disclosed.

**Steps**:
1. [x] - `p1` - Bind every catalog read to the calling tenant identifier before the store is consulted - `inst-ps-read-1`
2. [x] - `p1` - Authorize the caller's security context against the `read` element of the permission set of the plugin's base type per the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`), resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and perform the check before the store is consulted so an unauthorized caller fails as a `DomainError` before any lookup - `inst-ps-read-8`
3. [x] - `p1` - Apply the OData query parameters to the list result and return the plugin records with `plugin_type`, `name`, and `config_schema` - `inst-ps-read-2`
4. [x] - `p1` - **IF** a requested identifier resolves to a record owned by a different tenant or to no record at all - `inst-ps-read-3`
   1. [x] - `p1` - Return not-found without disclosing the foreign record's existence - `inst-ps-read-4`
5. [x] - `p1` - **RETURN** the catalog result or the single plugin record, with no secret material and no resolved credential value in any returned field - `inst-ps-read-7`

### Plugin Source Retrieval

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-plugin-source`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- `GET /oagw/v1/plugins/{id}/source` returns the registered source content of a custom plugin as an opaque reference artifact, bound to the calling tenant.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the `read` element of the permission set of the plugin's base type — one of `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}`: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted and no source content is disclosed.
- The identifier belongs to another tenant, including an ancestor: the result is not-found and the foreign record is never disclosed.
- A source retrieval names a built-in or named plugin: the result is not-found, because named plugins are resolved by the in-process registry and have no stored record or source.

**Steps**:
1. [x] - `p1` - Bind the source retrieval to the calling tenant identifier before the store is consulted - `inst-ps-source-1`
2. [x] - `p1` - Authorize the caller's security context against the `read` element of the permission set of the plugin's base type per the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`) — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and perform the check before the store is consulted so an unauthorized caller fails as a `DomainError` before any lookup - `inst-ps-source-2`
3. [x] - `p1` - **IF** a requested identifier resolves to a record owned by a different tenant or to no record at all - `inst-ps-source-3`
   1. [x] - `p1` - Return not-found without disclosing the foreign record's existence - `inst-ps-source-4`
4. [x] - `p1` - **IF** a source retrieval targets a named plugin that is resolved by the in-process registry rather than persisted - `inst-ps-read-5`
   1. [x] - `p1` - Return not-found, because a named plugin carries no stored source - `inst-ps-read-6`
5. [x] - `p1` - **RETURN** the source content as an opaque reference artifact, with no secret material and no resolved credential value in any returned field - `inst-ps-source-5`

### Plugin Deletion With Reference Guard

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-plugin-delete`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- `DELETE /oagw/v1/plugins/{id}` removes a custom plugin that no upstream auth block, upstream plugin binding, or route plugin binding references.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the `delete` element of the permission set of the plugin's base type — one of `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}`: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted and no record is deleted or disclosed.
- The plugin is still referenced: the delete is rejected with `409 PluginInUse` and a `referenced_by` body identifying the referencing upstreams and routes, and the record is left unchanged.
- The identifier belongs to another tenant or does not exist: the result is not-found.

**Concurrency posture**: the reference scan of step 4 and the removal of step 6 execute as one critical section under the repository's write exclusion for the target plugin record, the store being the single-process in-memory repository `cpt-cf-oagw-feature-gear-foundation` delivers, so no binding can be persisted against the record between the scan and the removal; a binding attempt that arrives while the delete holds that exclusion fails with the `409` conflict outcome instead of persisting against a record being deleted, and a detected in-use reference leaves the store unchanged. Cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation rather than claimed as a guarantee.

**Steps**:
1. [x] - `p1` - Resolve the target plugin by its anonymous GTS identifier, scoped to the calling tenant - `inst-ps-del-1`
2. [x] - `p1` - Authorize the caller's security context against the `delete` element of the permission set of the plugin's base type per the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`) — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, and perform the check before the store is consulted so an unauthorized caller fails as a `DomainError` before any lookup - `inst-ps-del-10`
3. [x] - `p1` - **IF** the identifier resolves to no record or to a record owned by another tenant - `inst-ps-del-2`
   1. [x] - `p1` - Return not-found without disclosing the foreign record - `inst-ps-del-3`
4. [x] - `p1` - Scan the upstream plugin bindings, the route plugin bindings, and the upstream auth plugin reference columns for references to the target, per `cpt-cf-oagw-algo-plugin-system-in-use-scan`, inside the same critical section as the removal below - `inst-ps-del-4`
5. [x] - `p1` - **IF** at least one reference exists - `inst-ps-del-5`
   1. [x] - `p1` - Reject the delete with `409 PluginInUse` carrying a `referenced_by` body that lists the referencing upstreams and routes, and leave the record and every binding unchanged - `inst-ps-del-6`
6. [x] - `p1` - **IF** no reference exists - `inst-ps-del-7`
   1. [x] - `p1` - Remove the plugin record from the in-memory repository inside the same critical section as the scan above - `inst-ps-del-8`
7. [x] - `p1` - **RETURN** success for an unreferenced plugin, or the `409 PluginInUse` rejection with its `referenced_by` body - `inst-ps-del-9`

### Plugin Binding Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-binding-resolution`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A plugin reference supplied in an upstream `auth` block or in a `plugins.items[]` entry resolves to a registry entry for a named plugin or to a persisted record for a UUID-backed plugin, and the binding is stored with `plugin_ref` always and `plugin_uuid` only when the reference is UUID-backed.
- A reference to a custom plugin registered by the calling tenant resolves through the plugin registry as a reference.
- A UUID-backed custom-plugin reference bound by an ancestor tenant resolves at proxy time against the owning tenant's plugin record through the tenant-chain walk, mirroring the DESIGN §3.2 `Proxy (data plane) — Inherited via tenant chain walk` row, while the management API stays strictly caller-scoped and keeps returning not-found for the same ancestor-owned record.

**Error Scenarios**:
- The caller presents no security context or an invalid one: `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`. A valid security context that lacks the `read` element of the permission set of the referenced plugin's base type: `403`, rendered through the shared canonical permission-denied error surface, where the platform canonical error `PermissionDenied` maps to `403`, with the problem+json body rendered by `cpt-cf-oagw-feature-error-handling`; no new OAGW error type is minted and no binding is stored or disclosed.
- A catalog-only identifier (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) is submitted: the binding is rejected at binding time as unresolvable (graded deviation 10).
- A UUID-backed reference has no matching record, or its UUID disagrees with the plugin schema type: the binding is rejected.
- A `plugin_uuid` value disagrees with its `plugin_ref`: the binding is rejected.
- The instance configuration does not validate against the resolved plugin's registered `config_schema`: an unknown key, a missing required key such as `client_id_ref` or `client_secret_ref`, a mutually exclusive key pair such as `token_endpoint` supplied together with `issuer_url`, or a guard binding whose required-header entries are all blank is rejected with `400` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` before any binding row is stored, so no invalid binding is persisted.

**Steps**:
1. [x] - `p1` - Parse the plugin GTS identifier from the binding to extract the instance part after the `~` separator - `inst-ps-bind-1`
2. [x] - `p1` - Authorize the caller's security context against the `read` element of the permission set of the referenced plugin's base type per the DESIGN §3.2 permission table (`cpt-cf-oagw-component-model`) — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — resolved through the `authz_resolver` handle `cpt-cf-oagw-feature-gear-foundation` delivers, with the write-side permission of the owning upstream or route flow already established by that flow; perform the check before the plugin repository is consulted so an unauthorized caller fails as a `DomainError` before any lookup - `inst-ps-bind-11`
3. [x] - `p1` - Resolve the identifier per `cpt-cf-oagw-algo-plugin-system-identifier-resolution` against the plugin registry for named plugins and the in-memory repository for UUID-backed plugins - `inst-ps-bind-2`
4. [x] - `p1` - **IF** the reference is UUID-backed and the binding was written by an ancestor tenant - `inst-ps-bind-12`
   1. [x] - `p1` - Resolve it against the owning tenant's plugin record through the tenant-chain walk, per the DESIGN §3.2 `Proxy (data plane) — Inherited via tenant chain walk` row, and keep the management API strictly caller-scoped so the same ancestor-owned record returns not-found through that surface - `inst-ps-bind-13`
5. [x] - `p1` - **IF** the identifier is one of the catalog-only identifiers `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1`, `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1`, `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1`, or `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1` - `inst-ps-bind-3`
   1. [x] - `p1` - Reject the binding as unresolvable, naming the identifier and stating that it is catalog-only, and store no binding row - `inst-ps-bind-4`
6. [x] - `p1` - **IF** the instance part parses as a UUID and no record matches it, or the matching record's plugin schema type disagrees with the binding's expected type - `inst-ps-bind-5`
   1. [x] - `p1` - Reject the binding as unresolvable - `inst-ps-bind-6`
7. [x] - `p1` - **IF** a submitted `plugin_uuid` disagrees with its `plugin_ref` - `inst-ps-bind-7`
   1. [x] - `p1` - Reject the binding rather than storing the disagreement - `inst-ps-bind-8`
8. [x] - `p1` - **IF** the resolved plugin carries a registered `config_schema` - `inst-ps-bind-14`
   1. [x] - `p1` - Validate `plugins.items[].config` and the upstream `auth.config` against it before the binding row is stored, rejecting unknown keys, missing required keys such as `client_id_ref` and `client_secret_ref`, mutually exclusive key pairs such as `token_endpoint` together with `issuer_url`, and a guard binding whose required-header entries are all blank, with `400` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and no binding row persisted - `inst-ps-bind-15`
9. [x] - `p1` - Persist the binding with `plugin_ref` always and `plugin_uuid` only for a UUID-backed plugin, at a position contiguous from zero - `inst-ps-bind-9`
10. [x] - `p1` - **RETURN** the stored binding, which entry 2.1's merge engine later concatenates into the effective chain - `inst-ps-bind-10`

### Plugin Chain Composition

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-chain-composition`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The effective chain for a proxied request is composed as upstream-level plugins followed by route-level plugins, `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`, in binding order within each level, with enforced ancestor bindings retained and private ancestor bindings invisible.
- The single auth plugin for the request is taken from the resolved upstream `auth` block.

**Error Scenarios**:
- A bound reference cannot be resolved at proxy time: the request is rejected with `PluginNotFound` rather than silently skipped.
- An ancestor binding carries `sharing: private` and the requester is a descendant: that binding contributes nothing.

**Steps**:
1. [x] - `p1` - Collect the ordered upstream-level bindings and the ordered route-level bindings from the effective configuration produced by the entry 2.1 merge engine - `inst-ps-comp-1`
2. [x] - `p1` - Concatenate them in upstream-then-route order, preserving binding order within each level, so `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]` - `inst-ps-comp-2`
3. [x] - `p1` - **IF** an ancestor binding carries `sharing: enforce` - `inst-ps-comp-3`
   1. [x] - `p1` - Retain the enforced binding in the chain so a descendant cannot remove it - `inst-ps-comp-4`
4. [x] - `p1` - **IF** an ancestor binding carries `sharing: private` and the requester is a descendant - `inst-ps-comp-5`
   1. [x] - `p1` - Omit that binding from the chain - `inst-ps-comp-6`
5. [x] - `p1` - Resolve every reference in the composed chain - `inst-ps-comp-7`
6. [x] - `p1` - **IF** any reference cannot be resolved at proxy time - `inst-ps-comp-8`
   1. [x] - `p1` - Fail the request with `PluginNotFound` for the shared error contract to render, rather than skipping the unresolvable plugin - `inst-ps-comp-9`
7. [x] - `p1` - **RETURN** one ordered chain of resolved plugins plus the single resolved auth plugin from the upstream `auth` block - `inst-ps-comp-10`

### Plugin Chain Execution

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-chain-execution`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A proxied request is executed in the deterministic order auth, guards, transform(request), the upstream call, transform(response) or transform(error), with the upstream call itself owned by entry 2.4.

**Error Scenarios**:
- An auth plugin fails to inject credentials: the request fails with `AuthenticationFailed`.
- A guard rejects the request in the request phase with status 400, or rejects the upstream response in the response phase with status 502.
- A transform plugin raises an error: the request fails and no upstream call is made for a request-phase failure.
- A transform plugin fails in the response phase: the upstream response is discarded, the failure maps to the downstream-error class of the shared error contract for entry 2.5 to render, and the client receives the gateway error rather than a partial response.
- A transform plugin fails in the error phase: the chain falls back to the untransformed error context, which entry 2.5 then renders, the transform failure is logged, and the original error type is never masked.
- A guard returns an error rather than a reject decision: the outcome is classified as a phase error that carries the guard's status when the guard produced one and otherwise the same status mapping as a guard reject.

**Timeout posture**: every built-in phase is bounded per invocation by the remaining request budget, which is derived from the configured `proxy_timeout_secs` that `cpt-cf-oagw-feature-gear-foundation` delivers as the total request budget; the per-plugin bound is that remaining budget and no new configuration key is introduced for it. The `cred_store` resolution call and the OAuth2 IdP exchange are bounded inside the auth phase that invokes them, so a hung store or IdP cannot outlive the request budget. A phase that exceeds its bound is treated as a phase error and never as a reject decision: it is mapped onto `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`, the request-timeout row of the shared error contract, for entry 2.5 to render as `504`, with no new OAGW error type minted, while an auth plugin that itself reports a failure keeps the `AuthenticationFailed` outcome this flow already assigns to it.

**Observability contributions**: this feature CONTRIBUTES, and does not itself emit, the outcome values `allow`, `reject`, `error`, and `resolve_failure` per plugin identifier and per phase, the `error_type` values the shared error contract assigns to the guard-reject, auth-failure, transform-error, and resolve-failure terminal outcomes, and the correlation identifier the shared error contract carries as `trace_id`. `allow` is the completed request, `reject` is a guard rejection in either phase, `error` covers the auth-failure, guard-error, and transform-error outcomes, and `resolve_failure` is the `PluginNotFound` outcome of an unresolvable reference. `cpt-cf-oagw-feature-observability-and-operability` is the owning emitter of the metric registry and of the audit record; this feature **MUST NOT** implement either.

**Steps**:
1. [x] - `p1` - Bound each built-in phase per invocation by the remaining request budget derived from the configured `proxy_timeout_secs` of `OagwConfig`, and classify a phase that exceeds its bound as a phase error rather than as a reject decision - `inst-ps-exec-15`
2. [x] - `p1` - Execute the single auth plugin once, before every guard, so it injects credentials into the outbound request context - `inst-ps-exec-1`
3. [x] - `p1` - **IF** the auth plugin fails - `inst-ps-exec-2`
   1. [x] - `p1` - Stop the chain, produce `AuthenticationFailed` for the shared error contract, and make no upstream call - `inst-ps-exec-3`
4. [x] - `p1` - Execute every guard plugin in chain order against the request, each returning an allow or reject decision - `inst-ps-exec-4`
5. [x] - `p1` - **IF** a guard rejects the request - `inst-ps-exec-5`
   1. [x] - `p1` - Stop the chain at the first rejection, make no upstream call, and produce the guard's phase-specific status for the shared error contract - `inst-ps-exec-6`
6. [x] - `p1` - **IF** a guard returns an error rather than a reject decision - `inst-ps-exec-16`
   1. [x] - `p1` - Classify the outcome as a phase error, preserving the guard's status when one was produced and otherwise mapping it as a guard reject is mapped, and make no upstream call - `inst-ps-exec-17`
7. [x] - `p1` - Execute every transform plugin in chain order against the request - `inst-ps-exec-7`
8. [x] - `p1` - **IF** a request-phase transform fails - `inst-ps-exec-8`
   1. [x] - `p1` - Stop the chain and make no upstream call - `inst-ps-exec-9`
9. [x] - `p1` - Hand the transformed request to the entry 2.4 upstream call and await its outcome - `inst-ps-exec-10`
10. [x] - `p1` - Execute every transform plugin in chain order against the upstream response, or against the error context when the upstream call failed - `inst-ps-exec-11`
11. [x] - `p1` - **IF** a transform plugin fails in the response phase - `inst-ps-exec-18`
   1. [x] - `p1` - Discard the upstream response, classify the failure as a downstream error of the shared error contract for entry 2.5 to render, and return the gateway error to the client instead of a partial response - `inst-ps-exec-19`
12. [x] - `p1` - **IF** a transform plugin fails in the error phase - `inst-ps-exec-20`
   1. [x] - `p1` - Fall back to the untransformed error context for entry 2.5 to render, log the transform failure, and never mask the original error type - `inst-ps-exec-21`
13. [x] - `p1` - **IF** a guard rejects the upstream response in the response phase - `inst-ps-exec-12`
   1. [x] - `p1` - Stop the chain and produce status 502 for the shared error contract instead of returning the upstream response - `inst-ps-exec-13`
14. [x] - `p1` - **RETURN** the transformed response to the entry 2.4 response pipeline, or the transformed error for rendering - `inst-ps-exec-14`

### Credential Resolution at Request Time

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-credential-resolution`

**Actor**: `cpt-cf-oagw-actor-cred-store`

**Success Scenarios**:
- A `cred://` reference carried in plugin or auth configuration is resolved through `cred_store` at request time, and only when a request needs it.
- `cred_store` confirms that the reference is accessible to the requesting tenant, either owned by it or shared by an ancestor, and returns the secret material to the plugin in memory.

**Error Scenarios**:
- The reference is not accessible to the requesting tenant, or does not resolve: the credential step fails and the shared error contract renders the outcome, with no secret material in the rendered body.
- `cred_store` is unreachable: the plugin reports an internal failure and does not cache the failure; a cached token, where one exists, continues to be served until its TTL expires.
- The `cred_store` resolution call exceeds the bound of the phase that invoked it: the phase-error classification of the chain-execution flow applies and the outcome is mapped to the shared error contract for entry 2.5 to render, with no secret material in the rendered body and nothing cached from the attempt.

**Steps**:
1. [x] - `p1` - Read the `cred://` reference from the plugin or auth configuration at request time rather than at configuration time - `inst-ps-cred-1`
2. [x] - `p1` - Resolve the reference through the `cred_store` client, letting `cred_store` decide whether the reference is accessible to the requesting tenant, either directly or through an ancestor sharing policy - `inst-ps-cred-2`
3. [x] - `p1` - Bound the `cred_store` resolution call by the remaining request budget of the phase that invoked it, per the timeout posture of `cpt-cf-oagw-flow-plugin-system-chain-execution`, so a hung store cannot outlive the request budget - `inst-ps-cred-9`
4. [x] - `p1` - Hold the returned secret material only inside the resolving plugin invocation and only for the duration it is needed, wrapping it so that the wrapper zeroes it on drop; the zeroing guarantee is the one the Known Residual Plaintext section of `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` records, under which the injected `Authorization` header string and the transient token held inside the fetch remain recorded residual-plaintext surfaces and are documented as exceptions with that ADR as the authority - `inst-ps-cred-3`
5. [x] - `p1` - **IF** `cred_store` cannot resolve the reference or reports it inaccessible to the tenant - `inst-ps-cred-4`
   1. [x] - `p1` - Fail the credential step, produce the corresponding domain error for entry 2.5 to render, and write no secret material into any log line, error message, or API response - `inst-ps-cred-5`
6. [x] - `p1` - **IF** the `cred_store` client is unreachable - `inst-ps-cred-6`
   1. [x] - `p1` - Report an internal plugin failure, cache nothing from the failed attempt, and let the next request retry the resolution - `inst-ps-cred-7`
7. [x] - `p1` - **RETURN** the resolved secret material to the calling plugin only, never to a log sink, an error body, or an API response - `inst-ps-cred-8`

### OAuth2 Token Cache Retrieval

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-plugin-system-token-cache`

**Actor**: `cpt-cf-oagw-actor-cred-store`

**Success Scenarios**:
- The OAuth2 client credentials plugin serves an access token from its internal in-memory cache on a hit, and performs exactly one IdP exchange per distinct cache key on a miss, per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`.
- The Form and Basic client-auth variants are two registered plugin identifiers that share the same cache configuration and differ only in `auth_method`.

**Error Scenarios**:
- The IdP is unavailable or rejects the exchange: the fetch fails, nothing is cached, and the next request for the same key retries the IdP.
- The IdP exchange exceeds the bound of the auth phase that invoked it: the phase-error classification of the chain-execution flow applies, nothing is cached, and the next request for the same key retries the exchange.
- A cache hit carries a stored key that does not match the lookup key: the hit is treated as a miss and the token is never used.

**Steps**:
1. [x] - `p1` - Parse the OAuth2 plugin configuration from the request context, requiring `client_id_ref` and `client_secret_ref` as `cred://` references and exactly one of `token_endpoint` and `issuer_url` - `inst-ps-tok-1`
2. [x] - `p1` - Build the cache key from the subject tenant identifier, the subject identifier, the client-auth method tag, and a deterministic hash of the plugin configuration, per `cpt-cf-oagw-algo-plugin-system-token-cache-key` - `inst-ps-tok-2`
3. [x] - `p1` - Look the key up in the plugin's internal in-memory cache - `inst-ps-tok-3`
4. [x] - `p1` - **IF** the cache returns an entry whose stored key matches the lookup key - `inst-ps-tok-4`
   1. [x] - `p1` - Inject the cached bearer token into the request context and skip the IdP exchange - `inst-ps-tok-5`
5. [x] - `p1` - **IF** the entry is absent, expired, or its stored key does not match the lookup key - `inst-ps-tok-6`
   1. [x] - `p1` - Resolve `client_id_ref` and `client_secret_ref` through `cred_store`, perform exactly one token exchange bounded by the remaining request budget of the auth phase that invoked it so a hung IdP cannot outlive the request budget, and drop the credential material when the exchange returns - `inst-ps-tok-7`
   2. [x] - `p1` - Compute the cache TTL as the lesser of the configured `token_cache_ttl_secs` and the IdP-reported expiry reduced by the 30-second safety margin, and store the entry under the lookup key only when the exchange succeeded - `inst-ps-tok-8`
6. [x] - `p1` - **IF** the token exchange fails, or the reported expiry is at or under the 30-second safety margin and therefore leaves no usable lifetime - `inst-ps-tok-9`
   1. [x] - `p1` - Cache nothing and report the failure so the next request retries the exchange - `inst-ps-tok-10`
7. [x] - `p1` - **IF** the upstream later rejects the injected credential with a 401 - `inst-ps-tok-11`
   1. [x] - `p1` - Make no attempt to re-issue the original client request with fresh credentials, because `cpt-cf-oagw-principle-no-retry` forbids re-issuing the original request - `inst-ps-tok-12`
8. [x] - `p1` - **RETURN** an authenticated request context carrying the injected `Authorization` header, with the token material held only inside the plugin and its cache - `inst-ps-tok-13`

## 3. Processes / Business Logic (CDSL)

### Plugin Identifier Resolution

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-identifier-resolution`

**Input**: a plugin GTS identifier from an upstream `auth` block or a `plugins.items[]` entry, and the plugin schema type the binding expects.
**Output**: a resolved plugin, or an unresolvable-rejection.

**Steps**:
1. [x] - `p1` - Parse the GTS identifier and extract the instance part after the `~` separator - `inst-ps-res-1`
2. [x] - `p1` - Classify the instance: a UUID denotes a custom plugin persisted in the plugin repository, anything else denotes a named plugin resolved by the in-process registry - `inst-ps-res-2`
3. [x] - `p1` - **IF** the instance parses as a UUID and the reference was bound by an ancestor tenant - `inst-ps-res-13`
   1. [x] - `p1` - Resolve it against the owning tenant's plugin record through the tenant-chain walk, per the DESIGN §3.2 `Proxy (data plane) — Inherited via tenant chain walk` row, and keep the management API strictly caller-scoped so the same ancestor-owned record returns not-found through that surface - `inst-ps-res-14`
4. [x] - `p1` - **IF** the instance parses as a UUID - `inst-ps-res-3`
   1. [x] - `p1` - Resolve the record from the in-memory repository, requiring it to exist and to match the plugin schema type of the base identifier - `inst-ps-res-4`
5. [x] - `p1` - **IF** the instance does not parse as a UUID - `inst-ps-res-5`
   1. [x] - `p1` - Resolve the identifier against the registry for the matching plugin type - `inst-ps-res-6`
6. [x] - `p1` - Compare the identifier against the catalog-only set `basic`, `bearer`, `timeout`, `cors`, `logging`, and `metrics`, which the types-registry catalogs but no plugin registry resolves - `inst-ps-res-7`
7. [x] - `p1` - **IF** the identifier is in the catalog-only set - `inst-ps-res-8`
   1. [x] - `p1` - Reject it as unresolvable at binding time, and never fall back to core Data Plane behavior for it - `inst-ps-res-9`
8. [x] - `p1` - **IF** no registry entry and no record matches the identifier at proxy time - `inst-ps-res-10`
   1. [x] - `p1` - Fail the request with `PluginNotFound` - `inst-ps-res-11`
9. [x] - `p1` - **RETURN** the resolved plugin, recording `plugin_ref` on the binding always and `plugin_uuid` only for a UUID-backed plugin - `inst-ps-res-12`

### Plugin Chain Composition

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-chain-order`

**Input**: the ordered upstream-level plugin bindings, the ordered route-level plugin bindings, the inherited ancestor bindings, their sharing modes, and the upstream `auth` block.
**Output**: one ordered effective plugin chain plus one resolved auth plugin.

**Steps**:
1. [x] - `p1` - Take the ancestor-contributed bindings in ancestor-then-descendant order, dropping any ancestor binding whose sharing mode is `private` when the requester is a descendant - `inst-ps-ord-1`
2. [x] - `p1` - Concatenate the upstream-level bindings and then the route-level bindings, preserving binding order within each level, so `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]` - `inst-ps-ord-2`
3. [x] - `p1` - Keep every binding inherited under `sharing: enforce` in place, so no descendant composition can remove it - `inst-ps-ord-3`
4. [x] - `p1` - Validate that the resulting binding positions remain contiguous from zero after composition - `inst-ps-ord-4`
5. [x] - `p1` - Take the auth plugin from the upstream `auth` block, at most one per request, and resolve it by the same identifier rules as the chain - `inst-ps-ord-5`
6. [x] - `p1` - Resolve every composed reference before execution begins, failing the request with `PluginNotFound` when one cannot be resolved - `inst-ps-ord-6`
7. [x] - `p1` - **RETURN** the ordered chain and the resolved auth plugin - `inst-ps-ord-7`

### Plugin Chain Execution Order

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-execution-order`

**Input**: the composed chain, the resolved auth plugin, and the request context.
**Output**: a transformed request handed to the upstream call, or a transformed response or error returned to the response pipeline.

**Steps**:
1. [x] - `p1` - Run the auth plugin exactly once, before any guard, to inject credentials - `inst-ps-seq-1`
2. [x] - `p1` - Run every guard plugin in chain order, taking the first reject as terminal and making no upstream call after a rejection - `inst-ps-seq-2`
3. [x] - `p1` - Run every transform plugin in chain order against the request - `inst-ps-seq-3`
4. [x] - `p1` - Release the transformed request to the entry 2.4 upstream call and suspend chain execution until the call returns or fails - `inst-ps-seq-4`
5. [x] - `p1` - On a successful upstream response, run every transform plugin in chain order against the response - `inst-ps-seq-5`
6. [x] - `p1` - On a failed upstream call, run every transform plugin in chain order against the error context instead - `inst-ps-seq-6`
7. [x] - `p1` - Run the response-phase guard check after the upstream response is available and before it is returned - `inst-ps-seq-7`
8. [x] - `p1` - **IF** any phase fails or rejects - `inst-ps-seq-8`
   1. [x] - `p1` - Short-circuit the remaining phases of that request and surface the failure to the shared error contract - `inst-ps-seq-9`
9. [x] - `p1` - **RETURN** the phase-ordered result, having executed no plugin out of order and no plugin twice in the same phase - `inst-ps-seq-10`

### Required Headers Guard Decision

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-required-headers-decision`

**Input**: the guard configuration keys `required_request_headers` and `required_response_headers`, and the request or response headers of the phase under evaluation.
**Output**: an allow decision, or a reject carrying the phase-specific status and the first missing header name.

**Steps**:
1. [x] - `p1` - Read the configuration key for the phase under evaluation: `required_request_headers` for the request phase and `required_response_headers` for the response phase, independently of each other - `inst-ps-hdr-1`
2. [x] - `p1` - **IF** the key is absent or blank after trimming - `inst-ps-hdr-2`
   1. [x] - `p1` - Return an allow decision, so an upstream that does not opt in sees no behavior change - `inst-ps-hdr-3`
3. [x] - `p1` - Parse the value by splitting on the comma separator, trimming each entry, lowercasing it, and dropping empty entries - `inst-ps-hdr-4`
4. [x] - `p1` - Scan the phase's headers for each required name case-insensitively, in configuration order, checking presence only and never comparing header values - `inst-ps-hdr-5`
5. [x] - `p1` - **IF** every required name is present - `inst-ps-hdr-6`
   1. [x] - `p1` - Return an allow decision - `inst-ps-hdr-7`
6. [x] - `p1` - **IF** a required name is missing - `inst-ps-hdr-8`
   1. [x] - `p1` - Return a reject that reports only the first missing name, with status 400 and `REQUIRED_HEADER_MISSING` in the request phase and status 502 and `REQUIRED_HEADER_MISSING` in the response phase - `inst-ps-hdr-9`
7. [x] - `p1` - **RETURN** the decision, having kept the plugin stateless with no cache and no security-sensitive material - `inst-ps-hdr-10`

### OAuth2 Token Cache Key and TTL

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-token-cache-key`

**Input**: the request context with its security context and plugin configuration, and the client-auth method of the plugin variant in use.
**Output**: a cache key, a cache TTL, and a cache decision.

**Steps**:
1. [x] - `p1` - Compose the cache key from the subject tenant identifier, the subject identifier, the client-auth method tag, and a deterministic hash of every plugin configuration key and value taken in sorted order - `inst-ps-key-1`
2. [x] - `p1` - Include the tenant component so a token minted for one tenant can never satisfy a lookup from another tenant - `inst-ps-key-2`
3. [x] - `p1` - Include the subject component so the `private` sharing mode of `cred_store` is honored across subjects of the same tenant - `inst-ps-key-3`
4. [x] - `p1` - Include the client-auth method component so the Form and Basic variants never collide even when their configuration is otherwise identical - `inst-ps-key-4`
5. [x] - `p1` - Include the configuration hash component so distinct upstream configurations, such as different scope sets, occupy distinct cache entries - `inst-ps-key-5`
6. [x] - `p1` - Store each entry as a wrapper that carries the original key alongside the token, and treat a hit whose stored key does not equal the lookup key as a miss, so a hash collision can never serve another tenant's token - `inst-ps-key-6`
7. [x] - `p1` - Compute the TTL as the lesser of the configured `token_cache_ttl_secs` and the IdP-reported expiry reduced by the 30-second safety margin, and store no entry whose remaining lifetime is not positive, so a reported expiry at or under the 30-second margin yields no cache entry - `inst-ps-key-7`
8. [x] - `p1` - **IF** the token exchange failed - `inst-ps-key-8`
   1. [x] - `p1` - Store no entry, so the next request for the same key retries the exchange - `inst-ps-key-9`
9. [x] - `p1` - **RETURN** the cache decision, having capped the cache at the configured `token_cache_capacity` and zeroed the token material on eviction - `inst-ps-key-10`

### Credential Isolation Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-credential-isolation`

**Input**: a credential-bearing value at any surface this feature touches: a plugin record, a binding configuration, a log line, an error message, an API response, and the in-memory plugin state.
**Output**: a surface that carries no secret material.

**Steps**:
1. [x] - `p1` - Resolve secret material only at request time and only inside the plugin invocation that needs it, never at configuration time and never into a stored record - `inst-ps-iso-1`
2. [x] - `p1` - Emit only the `cred://` reference into any record, binding, API response, error message, or log line, and never the resolved value - `inst-ps-iso-2`
3. [x] - `p1` - Wrap resolved secret material so that releasing it zeroes the buffer, and hold it only for the lifetime of the invocation or of the cache entry that stores it, distinguishing that zeroed storage from the recorded residual-plaintext exceptions — the injected `Authorization` header string and the transient token held inside the fetch — that the Known Residual Plaintext section of `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` documents as its authority - `inst-ps-iso-3`
4. [x] - `p1` - Exclude credential-bearing fields from the plugin catalog, the plugin source retrieval, and every management API response body - `inst-ps-iso-4`
5. [x] - `p1` - **IF** a rendered error path would otherwise echo a credential-bearing value - `inst-ps-iso-5`
   1. [x] - `p1` - Redact it before the error reaches the shared error contract - `inst-ps-iso-6`
6. [x] - `p1` - **RETURN** surfaces that expose no secret material, so zero credential exposure holds across logs, errors, and API outputs - `inst-ps-iso-7`

### Plugin In-Use Reference Scan

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-system-in-use-scan`

**Input**: the identifier of a custom plugin targeted for deletion, and the tenant-scoped repository state.
**Output**: either an empty reference set, or the set of referencing upstreams and routes.

**Steps**:
1. [x] - `p1` - Scan every upstream plugin binding and every route plugin binding for a `plugin_ref` equal to the target identifier - `inst-ps-scan-1`
2. [x] - `p1` - Scan the upstream auth plugin reference columns, which store the auth plugin identity in scalar form so the check does not depend on scanning JSON - `inst-ps-scan-2`
3. [x] - `p1` - Collect the referencing resources into the `referenced_by` set, recording whether each reference is an upstream auth binding, an upstream chain binding, or a route chain binding - `inst-ps-scan-3`
4. [x] - `p1` - **IF** the set is non-empty - `inst-ps-scan-4`
   1. [x] - `p1` - Reject the deletion with `409 PluginInUse` and return the `referenced_by` set in the response body - `inst-ps-scan-5`
5. [x] - `p1` - **RETURN** the reference set, leaving the plugin record and every binding untouched whenever the set is non-empty - `inst-ps-scan-6`

## 4. States (CDSL)

### Plugin Record State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-plugin-system-plugin-record`

**States**: `absent`, `registered`, `bound`, `unbound`, `deleted`
**Initial State**: `absent`
**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `registered` **WHEN** a create is accepted at `POST /oagw/v1/plugins` for a valid plugin type and a tenant-unique name - `inst-ps-st-rec-1`
2. [x] - `p1` - **FROM** `registered` **TO** `bound` **WHEN** an upstream auth block, an upstream plugin binding, or a route plugin binding resolves to the record - `inst-ps-st-rec-2`
3. [x] - `p1` - **FROM** `bound` **TO** `unbound` **WHEN** the last referencing binding or auth reference is removed - `inst-ps-st-rec-3`
4. [x] - `p1` - **FROM** `unbound` **TO** `bound` **WHEN** a new binding or auth reference resolves to the record - `inst-ps-st-rec-4`
5. [x] - `p1` - **FROM** `bound` **TO** `bound` **WHEN** a deletion is attempted, which is rejected with `409 PluginInUse` and a `referenced_by` body and leaves the record and every binding unchanged - `inst-ps-st-rec-5`
6. [x] - `p1` - **FROM** `unbound` **TO** `deleted` **WHEN** a deletion is accepted for a record the calling tenant owns - `inst-ps-st-rec-6`
7. [x] - `p1` - **FROM** `registered` **TO** `deleted` **WHEN** a deletion is accepted for a record that no binding and no auth reference resolves to - `inst-ps-st-rec-8`
8. [x] - `p1` - **FROM** `deleted` **TO** `absent` **WHEN** the in-memory entry is released, leaving no persisted plugin row in the graded configuration - `inst-ps-st-rec-7`

**Invariant**: every state other than `bound` is deletable by its owning tenant; only a `bound` record must first have its references removed.

### Plugin Chain Execution State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-plugin-system-chain-execution`

**States**: `resolving`, `authenticating`, `guarding`, `transforming_request`, `awaiting_upstream`, `transforming_response`, `transforming_error`, `completed`
**Initial State**: `resolving`
**Transitions**:
1. [x] - `p1` - **FROM** `resolving` **TO** `authenticating` **WHEN** the composed chain resolves and the upstream carries an `auth` block - `inst-ps-st-exec-1`
2. [x] - `p1` - **FROM** `resolving` **TO** `transforming_request` **WHEN** the composed chain resolves, the upstream declares no auth plugin, and the composed chain carries no guard plugin - `inst-ps-st-exec-2`
3. [x] - `p1` - **FROM** `resolving` **TO** `guarding` **WHEN** the composed chain resolves, the upstream declares no auth plugin, and the composed chain carries at least one guard plugin - `inst-ps-st-exec-14`
4. [x] - `p1` - **FROM** `resolving` **TO** `transforming_error` **WHEN** a composed reference cannot be resolved, which surfaces as `PluginNotFound` - `inst-ps-st-exec-3`
5. [x] - `p1` - **FROM** `authenticating` **TO** `guarding` **WHEN** the auth plugin injects credentials successfully - `inst-ps-st-exec-4`
6. [x] - `p1` - **FROM** `authenticating` **TO** `transforming_error` **WHEN** the auth plugin fails to inject credentials or exceeds its per-invocation bound - `inst-ps-st-exec-5`
7. [x] - `p1` - **FROM** `guarding` **TO** `transforming_request` **WHEN** every guard plugin returns an allow decision - `inst-ps-st-exec-6`
8. [x] - `p1` - **FROM** `guarding` **TO** `transforming_error` **WHEN** a guard plugin rejects the request with status 400 - `inst-ps-st-exec-7`
9. [x] - `p1` - **FROM** `guarding` **TO** `transforming_error` **WHEN** a guard plugin returns an error rather than a reject decision, carrying the guard's status when one was produced and otherwise the same status mapping as a guard reject - `inst-ps-st-exec-17`
10. [x] - `p1` - **FROM** `transforming_request` **TO** `awaiting_upstream` **WHEN** every transform plugin completes its request phase - `inst-ps-st-exec-8`
11. [x] - `p1` - **FROM** `awaiting_upstream` **TO** `transforming_response` **WHEN** the entry 2.4 upstream call returns a response - `inst-ps-st-exec-9`
12. [x] - `p1` - **FROM** `awaiting_upstream` **TO** `transforming_error` **WHEN** the entry 2.4 upstream call fails or reports an error - `inst-ps-st-exec-10`
13. [x] - `p1` - **FROM** `transforming_response` **TO** `completed` **WHEN** every transform plugin completes its response phase and the response-phase guard check allows - `inst-ps-st-exec-11`
14. [x] - `p1` - **FROM** `transforming_response` **TO** `transforming_error` **WHEN** the response-phase guard check rejects the upstream response with status 502 - `inst-ps-st-exec-12`
15. [x] - `p1` - **FROM** `transforming_response` **TO** `transforming_error` **WHEN** a transform plugin fails in the response phase or exceeds its per-invocation bound, which discards the upstream response and maps to the downstream-error class of the shared error contract - `inst-ps-st-exec-15`
16. [x] - `p1` - **FROM** `transforming_error` **TO** `completed` **WHEN** every transform plugin completes its error phase and the error is handed to the shared error contract - `inst-ps-st-exec-13`
17. [x] - `p1` - **FROM** `transforming_error` **TO** `transforming_error` **WHEN** a transform plugin fails in the error phase, which falls back to the untransformed error context without masking the original error type - `inst-ps-st-exec-16`

## 5. Definitions of Done

### Plugin Traits and Deterministic Execution Order

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-plugin-traits`

The system **MUST** define the three plugin traits in the domain layer: an auth plugin that injects credentials and runs exactly once per request before any guard, a guard plugin that validates a request or an upstream response and returns an allow or reject decision, and a transform plugin that mutates request, response, and error contexts. The chain executor **MUST** run them in the order auth, guards, transform(request), the upstream call, transform(response) or transform(error), and **MUST** carry the execution payloads `AuthContext`, `RequestContext`, `ResponseContext`, and `ErrorContext`. The executor **MUST** bound each built-in phase per invocation by the remaining request budget derived from the configured `proxy_timeout_secs` of `OagwConfig`, the total request budget delivered by entry 2.1, with no new configuration key introduced for that per-plugin bound, and **MUST** treat a phase that exceeds its bound as a phase error rather than as a reject decision, mapping it onto `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` of the shared error contract for entry 2.5 to render as `504`. The executor **MUST** classify a transform plugin that fails in the response phase as a downstream error that discards the upstream response, **MUST** fall back to the untransformed error context when a transform plugin fails in the error phase without masking the original error type, and **MUST** classify a guard that returns an error rather than a reject decision as a phase error that preserves the guard's status when one was produced and otherwise maps as a guard reject is mapped.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-chain-execution`
- `cpt-cf-oagw-algo-plugin-system-execution-order`
- `cpt-cf-oagw-state-plugin-system-chain-execution`

**Touches**:
- API: none
- Entities: `AuthContext`, `RequestContext`, `ResponseContext`, `ErrorContext`
- Tests: unit tests in `src/infra/plugin/executor_tests.rs` asserting the phase order, the single auth invocation, the short-circuit on the first rejection, the response-phase transform failure that discards the upstream response, the error-phase transform failure that falls back to the untransformed error context, and the guard that returns an error rather than a reject decision, and unit tests in `src/infra/plugin/phase_bound_tests.rs` asserting that a phase exceeding the remaining request budget derived from `proxy_timeout_secs` is classified as a phase error rather than as a reject decision

### Plugin Registries With Built-ins

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-registries`

The system **MUST** provide `AuthPluginRegistry`, `GuardPluginRegistry`, and `TransformPluginRegistry` in `infra/plugin/`, each constructed through a `with_builtins()` constructor that registers the built-in plugins of its type, and **MUST** resolve a named plugin by its GTS identifier and a custom plugin by its UUID-backed record. A registry **MUST** return an unresolvable outcome for any identifier it does not carry, and **MUST NOT** resolve a catalog-only identifier.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-binding-resolution`
- `cpt-cf-oagw-algo-plugin-system-identifier-resolution`

**Touches**:
- API: none
- Entities: `Plugin`
- Tests: unit tests in `src/infra/plugin/registry_tests.rs` for built-in registration, named and UUID-backed resolution, and unresolvable-identifier rejection

### Built-in Auth Plugins

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-builtin-auth`

The system **MUST** ship the built-in auth plugins `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`, `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`, `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`, and `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1` in `AuthPluginRegistry::with_builtins()`, with the last two being the Form and Basic client-auth variants of one OAuth2 client credentials plugin that differ only in `auth_method` and share one token cache, per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`. The API key plugin **MUST** inject its credential from a `cred://` reference into the configured header or query location.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-credential-resolution`
- `cpt-cf-oagw-flow-plugin-system-token-cache`

**Touches**:
- API: none
- Entities: `AuthConfig`, `Plugin`
- Tests: unit tests in `src/infra/plugin/apikey_auth_tests.rs` and `src/infra/plugin/oauth2_client_cred_auth_tests.rs` for registration under both GTS identifiers, header and query injection, and the Form and Basic variants sharing one cache

### OAuth2 Client Credentials Token Cache

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-token-cache`

The system **MUST** cache access tokens inside the OAuth2 client credentials plugin in an in-memory cache bounded by `token_cache_capacity`, keyed by subject tenant identifier, subject identifier, client-auth method, and a deterministic hash of the plugin configuration, and holding each entry with the original key so a hit whose stored key differs is treated as a miss. The cache TTL **MUST** be the lesser of `token_cache_ttl_secs` and the IdP-reported expiry reduced by the 30-second safety margin, so a reported expiry at or under the 30-second margin yields no cache entry, a failed token exchange **MUST NOT** be cached, and the plugin **MUST NOT** re-issue the original client request after an upstream 401.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-token-cache-key`
- `cpt-cf-oagw-flow-plugin-system-token-cache`

**Touches**:
- API: none
- Entities: `OagwConfig`, `AuthConfig`
- Tests: unit tests in `src/infra/plugin/oauth2_client_cred_auth_tests.rs` for the cache key components, the TTL rule, the failed-fetch non-caching, the key-verification-on-hit behavior, and the absence of any retry after an upstream 401

### Built-in Required Headers Guard Plugin

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-required-headers-guard`

The system **MUST** ship `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` as the only guard plugin registered by `GuardPluginRegistry::with_builtins()`, enforcing presence-only header checks in both the request and the response phase from the independent configuration keys `required_request_headers` and `required_response_headers`, failing open when a key is absent or blank, reporting only the first missing header name, and rejecting with status 400 in the request phase and status 502 in the response phase, per `cpt-cf-oagw-adr-required-headers-guard-plugin`. Header values **MUST NOT** be validated.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-required-headers-decision`
- `cpt-cf-oagw-flow-plugin-system-chain-execution`

**Touches**:
- API: none
- Entities: `PluginsConfig`
- Tests: unit tests in `src/infra/plugin/required_headers_guard_tests.rs` for fail-open on absent and blank configuration, case-insensitive presence matching, first-missing-header reporting, and the 400 and 502 phase statuses

### Built-in Request ID Transform Plugin

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-request-id-transform`

The system **MUST** ship `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` in `TransformPluginRegistry::with_builtins()`. The injected value **MUST** be a server-generated lowercase hyphenated UUID. The correlation identifier **MUST** be minted once at proxy entry and propagated by the trace-propagation flow of `cpt-cf-oagw-feature-observability-and-operability`, and this plugin **MUST** be the only writer of the `X-Request-ID` header, so it adopts that value rather than minting a second identifier. A propagated inbound value **MUST** be validated as 1 to 128 characters drawn from the RFC 3986 unreserved set plus `-._~` before it is reused, and an invalid inbound value **MUST** be replaced by the freshly minted value rather than forwarded. The value **MUST NOT** be added to the response headers returned to the caller: the request-phase value is held in the request context through the response phase, and caller-visible correlation is the `trace_id` extension member of the shared error contract owned by `cpt-cf-oagw-feature-error-handling`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-execution-order`

**Touches**:
- API: none
- Entities: `PluginsConfig`
- Tests: unit tests in `src/infra/plugin/request_id_transform_tests.rs` named `injects_minted_lowercase_hyphenated_uuid_when_absent`, `adopts_the_proxy_entry_correlation_identifier`, `validates_inbound_value_and_replaces_an_invalid_one`, and `emits_no_response_header_to_the_caller`, which together also cover propagation of a valid inbound value and its hold-through of the response phase

### Plugin Identifier Resolution and Catalog-Only Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-identifier-resolution`

The system **MUST** resolve every plugin GTS identifier by parsing the instance part after the `~` separator, resolving a UUID instance through the tenant-scoped plugin repository with a matching plugin schema type, and resolving any other instance through the in-process registry of the matching plugin type, storing `plugin_ref` on every binding and `plugin_uuid` only for a UUID-backed plugin. A UUID-backed reference bound by an ancestor tenant **MUST** be resolved against the owning tenant's plugin record through the tenant-chain walk, mirroring the DESIGN §3.2 `Proxy (data plane) — Inherited via tenant chain walk` row, while the same lookup through the management API **MUST** keep returning not-found because that surface stays strictly caller-scoped. The system **MUST** register the catalog-only identifiers `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1`, `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1`, `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1`, and `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1` in the types-registry catalog and **MUST** reject each of them at binding time as unresolvable, and **MUST** return `PluginNotFound` when a bound reference cannot be resolved at proxy time.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-binding-resolution`
- `cpt-cf-oagw-algo-plugin-system-identifier-resolution`

**Touches**:
- API: none
- Entities: `Plugin`
- Tests: unit tests in `src/infra/plugin/identifier_resolution_tests.rs` and integration tests in `tests/plugin_binding_rejection.rs` for the catalog-only rejection set, the UUID-versus-named split, and the proxy-time `PluginNotFound` outcome

### Plugin Chain Composition and Ordering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-chain-ordering`

The system **MUST** compose the effective plugin chain by concatenating the upstream-level bindings and then the route-level bindings in binding order, so `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`, **MUST** retain a binding inherited under `sharing: enforce` so no descendant composition can remove it, **MUST** omit an ancestor binding whose sharing mode is `private` from a descendant's chain, and **MUST** take at most one auth plugin from the upstream `auth` block rather than from the chain.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-chain-composition`
- `cpt-cf-oagw-algo-plugin-system-chain-order`

**Touches**:
- API: none
- Entities: `PluginsConfig`, `AuthConfig`
- Tests: unit tests in `src/infra/plugin/composition_tests.rs` for the `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]` ordering, enforced-binding retention, private-binding omission, and the single-auth-plugin rule

### Credential Resolution at Request Time

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-credential-resolution`

The system **MUST** resolve every `cred://` reference through the `cred_store` client under `cpt-cf-oagw-contract-cred-store` at request time and only inside the plugin invocation that needs it, letting `cred_store` decide whether the reference is accessible to the requesting tenant, and **MUST** fail the credential step, without caching the failure, when the reference does not resolve or is not accessible. This is the secret-resolution slice of `cpt-cf-oagw-nfr-credential-isolation`; the configuration-boundary slice is owned by entry 2.1.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-credential-resolution`
- `cpt-cf-oagw-algo-plugin-system-credential-isolation`

**Touches**:
- API: none
- Entities: `AuthConfig`, `Plugin`
- Tests: unit tests in `src/infra/plugin/credential_resolution_tests.rs` for request-time-only resolution and for the inaccessible-reference and unreachable-store outcomes

### Credential Isolation Across Every Surface

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-credential-isolation`

The system **MUST** ensure that no resolved credential value is ever written to a log line, included in an error message, returned in an API response, or persisted by the gear, that only the `cred://` reference crosses any of those surfaces, and that resolved material is held in a zeroing wrapper for the lifetime of the invocation or of the cache entry that legitimately stores it. The zeroing guarantee is the one the Known Residual Plaintext section of `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin` records: the cache entry and the wrapper held by the resolving invocation are zeroized on drop, while the injected `Authorization` header string and the transient token held inside the IdP fetch remain recorded residual-plaintext surfaces, documented as exceptions with that ADR as the authority. This realizes `cpt-cf-oagw-principle-cred-isolation`.

**Implements**:
- `cpt-cf-oagw-algo-plugin-system-credential-isolation`
- `cpt-cf-oagw-flow-plugin-system-credential-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`
- Entities: `Plugin`, `AuthConfig`
- Tests: integration tests in `tests/credential_isolation.rs` asserting only the log, error, and API surface guarantee — that no log record, rendered error body, or management API response contains resolved secret material — and deliberately asserting nothing about the in-memory lifetime of the two residual-plaintext surfaces that ADR 0008 records as exceptions

### Plugin Management REST API

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-management-api`

The system **MUST** implement the plugin management endpoints at the gear-relative prefix: `POST /oagw/v1/plugins` to create a custom plugin with a server-generated UUID, `GET /oagw/v1/plugins` to list with the OData query parameters `$filter`, `$select`, `$top`, and `$skip`, `GET /oagw/v1/plugins/{id}` to read one by its anonymous GTS identifier, `DELETE /oagw/v1/plugins/{id}` to delete, and `GET /oagw/v1/plugins/{id}/source` to retrieve the registered source content. Every operation **MUST** be scoped to the calling tenant, ancestor records **MUST** return not-found, and the error outcomes **MUST** be raised as `DomainError` for entry 2.5 to render as problem+json. Every operation **MUST** authorize the caller's security context, resolved through the `authz_resolver` handle entry 2.1 delivers, against the permission set of the plugin's base type per the DESIGN §3.2 permission table — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, or `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — before the store is consulted: a missing or invalid security context yields `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, and a valid context lacking the required element yields `403` through the shared canonical permission-denied error surface rendered by `cpt-cf-oagw-feature-error-handling`, with no new OAGW error type minted and no record created, disclosed, or deleted.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-plugin-create`
- `cpt-cf-oagw-flow-plugin-system-plugin-read`
- `cpt-cf-oagw-flow-plugin-system-plugin-source`
- `cpt-cf-oagw-flow-plugin-system-plugin-delete`

**Touches**:
- API: `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`
- Entities: `Plugin`
- Tests: integration tests in `tests/plugin_management_api.rs` for the five endpoints, the OData parameters, tenant scoping, the not-found outcomes, and the `401` and `403` authorization outcomes

### Plugin Immutability and Reference-Guarded Deletion

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-immutability-delete`

The system **MUST** treat a custom plugin as immutable after creation, offering no `PUT` or `PATCH` on `/oagw/v1/plugins/{id}` so that an update is performed by creating a new plugin and re-binding references, and **MUST** reject the deletion of a plugin referenced by an upstream auth block, an upstream plugin binding, or a route plugin binding with `409 PluginInUse` and a `referenced_by` body identifying the referencing upstreams and routes, leaving the record and every binding unchanged. The reference scan and the removal **MUST** execute as one critical section under the repository's write exclusion for the target plugin record, the store being the single-process in-memory repository entry 2.1 delivers, so a binding attempt that arrives while the delete holds that exclusion fails with the `409` conflict outcome and a detected in-use reference leaves the store unchanged; cross-instance concurrency control is out of scope per DESIGN §4.7 and is recorded as a limitation rather than claimed as a guarantee.

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-plugin-delete`
- `cpt-cf-oagw-algo-plugin-system-in-use-scan`
- `cpt-cf-oagw-state-plugin-system-plugin-record`

**Touches**:
- API: `DELETE /oagw/v1/plugins/{id}`
- Entities: `Plugin`
- Tests: integration tests in `tests/plugin_delete_in_use.rs` for the `409 PluginInUse` body, the absence of a replacement method on the plugin path, the unchanged state after a rejected deletion, and the concurrent-bind-then-delete case in which a binding attempt arrives while the delete holds the write exclusion

### Registry-Reference-Only Custom Plugins

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-system-registry-reference-only`

The system **MUST** support custom plugins as registry references only: a custom plugin is created, cataloged, addressed by its GTS identifier, resolvable through the resolution algorithm, retrievable through the source endpoint, and bindable by reference, while its registered source content is stored and retrieved as an opaque reference artifact and is never interpreted or executed. The system **MUST NOT** implement Starlark or any other script interpretation, and **MUST** document the plugin-trait boundary as the sandboxing surface for `cpt-cf-oagw-nfr-starlark-sandbox` (graded deviation 6).

**Implements**:
- `cpt-cf-oagw-flow-plugin-system-plugin-create`
- `cpt-cf-oagw-flow-plugin-system-plugin-read`
- `cpt-cf-oagw-flow-plugin-system-plugin-source`
- `cpt-cf-oagw-algo-plugin-system-identifier-resolution`

**Touches**:
- API: `GET /oagw/v1/plugins/{id}/source`
- Entities: `Plugin`
- Tests: unit tests in `src/infra/plugin/registry_reference_tests.rs` asserting that a registered custom plugin resolves as a reference and that no execution path exists for its source content

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering the execution order and short-circuit semantics, the three unspecified error branches of the chain executor, the enforced per-phase bound derived from `proxy_timeout_secs` and its phase-error classification, registry construction and resolution, each built-in plugin's behavior, the token cache key and TTL rules, credential isolation, the in-use reference scan, and catalog-only identifier rejection, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-plugin-system-plugin-traits`
- `cpt-cf-oagw-dod-plugin-system-registries`
- `cpt-cf-oagw-dod-plugin-system-builtin-auth`
- `cpt-cf-oagw-dod-plugin-system-token-cache`
- `cpt-cf-oagw-dod-plugin-system-required-headers-guard`
- `cpt-cf-oagw-dod-plugin-system-request-id-transform`
- `cpt-cf-oagw-dod-plugin-system-identifier-resolution`
- `cpt-cf-oagw-dod-plugin-system-chain-ordering`
- `cpt-cf-oagw-dod-plugin-system-credential-isolation`

**Touches**:
- API: none
- Entities: `Plugin`, `AuthContext`, `RequestContext`, `ResponseContext`, `ErrorContext`
- Tests: `src/infra/plugin/executor_tests.rs`, `src/infra/plugin/phase_bound_tests.rs`, `src/infra/plugin/registry_tests.rs`, `src/infra/plugin/apikey_auth_tests.rs`, `src/infra/plugin/oauth2_client_cred_auth_tests.rs`, `src/infra/plugin/required_headers_guard_tests.rs`, `src/infra/plugin/request_id_transform_tests.rs`, `src/infra/plugin/identifier_resolution_tests.rs`, `src/infra/plugin/composition_tests.rs`, `src/infra/plugin/credential_resolution_tests.rs`, `src/infra/plugin/registry_reference_tests.rs`

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-plugin-system-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering the plugin management REST surface, the `401` and `403` authorization denial outcomes of that surface, the `409 PluginInUse` deletion semantics with a `referenced_by` body, the concurrent-bind-then-delete case against the delete critical section, the binding-time rejection of an instance configuration that fails the resolved plugin's `config_schema`, catalog-only identifier rejection at binding time, upstream-before-route chain ordering against the proxy pipeline, and the absence of resolved secret material in any log record or API response, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-plugin-system-management-api`
- `cpt-cf-oagw-dod-plugin-system-immutability-delete`
- `cpt-cf-oagw-dod-plugin-system-identifier-resolution`
- `cpt-cf-oagw-dod-plugin-system-chain-ordering`
- `cpt-cf-oagw-dod-plugin-system-credential-isolation`

**Touches**:
- API: `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`
- Entities: `Plugin`, `Upstream`, `Route`
- Tests: `tests/plugin_management_api.rs`, `tests/plugin_delete_in_use.rs`, `tests/plugin_binding_rejection.rs`, `tests/plugin_chain_ordering.rs`, `tests/credential_isolation.rs`

## 6. Acceptance Criteria

- [x] A request proxied through an upstream with an auth plugin, two guards, and a transform plugin executes in the order auth, guards, transform(request), upstream call, transform(response) or transform(error), and the first guard rejection prevents any upstream call (DoD `cpt-cf-oagw-dod-plugin-system-plugin-traits`).
- [x] The three registries are constructible through `with_builtins()` and resolve the built-in identifiers `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic`, `required_headers`, and `request_id`, and no catalog-only identifier resolves through any of them (DoD `cpt-cf-oagw-dod-plugin-system-registries`).
- [x] Binding `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1`, `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1`, `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1`, `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1`, or `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1` is rejected at binding time as unresolvable, and none of them triggers the core timeout, CORS, logging, or metrics behavior (DoD `cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
- [x] A UUID-backed reference with no matching record, and a named reference absent from the registry, produce `PluginNotFound` at proxy time rather than a silently skipped plugin (DoD `cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
- [x] An instance configuration that fails the resolved plugin's registered `config_schema` — an unknown key, a missing required key such as `client_id_ref` or `client_secret_ref`, a mutually exclusive key pair such as `token_endpoint` supplied together with `issuer_url`, or a guard binding whose required-header entries are all blank — is rejected at binding time with `400` `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`, and no binding row is stored (DoD `cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
- [x] A descendant request resolves a custom plugin record bound by an ancestor tenant through the tenant-chain walk of the owning tenant, and the same ancestor-owned record returns not-found when looked up through the management API (DoD `cpt-cf-oagw-dod-plugin-system-identifier-resolution`).
- [x] An upstream chain of two plugins and a route chain of two plugins execute in the order `[U1, U2, R1, R2]`, an enforced ancestor binding survives in a descendant's chain, and a private ancestor binding is absent from it (DoD `cpt-cf-oagw-dod-plugin-system-chain-ordering`).
- [x] The `required_headers` guard allows a request when its configuration key is absent or blank, rejects a request missing the first required header with 400, and rejects an upstream response missing its first required header with 502, reporting one missing header per rejection (DoD `cpt-cf-oagw-dod-plugin-system-required-headers-guard`).
- [x] The OAuth2 client credentials plugin serves a second request for the same tenant, subject, auth method, and configuration from cache, performs exactly one IdP exchange for that key within a TTL window, and stores nothing after a failed exchange (DoD `cpt-cf-oagw-dod-plugin-system-token-cache`).
- [x] A token cached for one tenant is never served to a lookup from another tenant, a different subject, the other client-auth method, or a different configuration hash, and a cache entry whose stored key does not match the lookup key is treated as a miss (DoD `cpt-cf-oagw-dod-plugin-system-token-cache`).
- [x] A `cred://` reference is resolved only at request time, a reference that `cred_store` cannot resolve for the tenant fails the credential step without caching the failure, and no upstream request is re-issued after an upstream 401 (DoD `cpt-cf-oagw-dod-plugin-system-credential-resolution`).
- [x] No log record, error message, or management API response body emitted by this feature contains a resolved credential value, and only the `cred://` reference is ever visible on those surfaces (DoD `cpt-cf-oagw-dod-plugin-system-credential-isolation`).
- [x] `POST /oagw/v1/plugins`, `GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `DELETE /oagw/v1/plugins/{id}`, and `GET /oagw/v1/plugins/{id}/source` are served under the gear-relative prefix with no `/api` segment, and a plugin owned by an ancestor tenant is not found by a descendant (DoD `cpt-cf-oagw-dod-plugin-system-management-api`).
- [x] A request to any of the five plugin management operations that carries no security context or an invalid one returns `401` with `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, and a valid security context that lacks the required element of the plugin base type's permission set returns `403` through the shared canonical permission-denied error surface, with no record created, disclosed, or deleted and no new OAGW error type minted (DoD `cpt-cf-oagw-dod-plugin-system-management-api`, `cpt-cf-oagw-dod-plugin-system-immutability-delete`).
- [x] A second plugin with the same `(tenant_id, name)` is rejected with a conflict, no `PUT` or `PATCH` method exists on `/oagw/v1/plugins/{id}`, and deleting a plugin referenced by an upstream or route returns `409 PluginInUse` with a `referenced_by` body and leaves every binding unchanged (DoD `cpt-cf-oagw-dod-plugin-system-immutability-delete`).
- [x] A custom plugin registered through the management API is addressable, resolvable, bindable, and readable through the source endpoint, and no code path interprets or executes its registered source content (DoD `cpt-cf-oagw-dod-plugin-system-registry-reference-only`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-plugin-system-unit-tests`, `cpt-cf-oagw-dod-plugin-system-integration-tests`).
