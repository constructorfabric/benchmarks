# Feature: Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Scope Exclusions](#15-scope-exclusions)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Wiring and Initialization](#gear-wiring-and-initialization)
  - [Gear-Relative REST Registration](#gear-relative-rest-registration)
  - [Configuration Load and Validation](#configuration-load-and-validation)
  - [Effective Configuration Resolution](#effective-configuration-resolution)
  - [Tenant-Scoped Repository Access](#tenant-scoped-repository-access)
  - [GTS Type Provisioning](#gts-type-provisioning)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Hierarchical Configuration Merge](#hierarchical-configuration-merge)
  - [Tenant-Scoped Repository Operations](#tenant-scoped-repository-operations)
  - [Credential-Reference Boundary](#credential-reference-boundary)
  - [Performance Cost Expectations](#performance-cost-expectations)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Wiring State Machine](#gear-wiring-state-machine)
  - [Stored Configuration Record State Machine](#stored-configuration-record-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [ToolKit Gear Wiring](#toolkit-gear-wiring)
  - [Gear-Relative REST Registration Skeleton](#gear-relative-rest-registration-skeleton)
  - [OagwConfig Parsing and Defaults](#oagwconfig-parsing-and-defaults)
  - [Hierarchical Configuration Merge Engine](#hierarchical-configuration-merge-engine)
  - [Shared Domain Model Types](#shared-domain-model-types)
  - [Domain Error Type](#domain-error-type)
  - [Repository Traits and In-Memory Implementation](#repository-traits-and-in-memory-implementation)
  - [Strict Tenant Scoping](#strict-tenant-scoping)
  - [Credential Isolation at the Configuration Boundary](#credential-isolation-at-the-configuration-boundary)
  - [GTS Type Provisioning](#gts-type-provisioning-1)
  - [Documented Schema Contract](#documented-schema-contract)
  - [DDD-Light Layer Boundaries](#ddd-light-layer-boundaries)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-gear-foundation`

## 1. Feature Context

### 1.1 Overview

Establishes `oagw` as a Gears ToolKit gear and delivers every primitive the other entries consume: gear wiring with gear-relative REST registration, the `OagwConfig` model, the hierarchical configuration merge engine, the shared domain model types, repository traits with an in-memory config-backed implementation, and GTS type provisioning.

### 1.2 Purpose

No configuration can be stored, resolved, or proxied until the gear is wired, its configuration model parses, its domain types exist, and a repository boundary is available. This feature is the root of the OAGW decomposition: it establishes the DDD-Light layer skeleton (`api/rest/`, `domain/`, `infra/`) recorded in `cpt-cf-oagw-design-layers`, the domain model recorded in `cpt-cf-oagw-design-domain-model`, the external dependency wiring recorded in `cpt-cf-oagw-design-dependencies`, and the gear-relative route tree recorded in `cpt-cf-oagw-interface-api` that entries 2.2, 2.3, 2.4, and 2.6 implement endpoints against. It realizes the architecture drivers collected in `cpt-cf-oagw-design-drivers` and the Control Plane / Data Plane split of `cpt-cf-oagw-design-overview` at the level of wiring and boundaries, declares the technology surface of `cpt-cf-oagw-tech-dependencies` as it exists in the crate manifest, and delivers the foundation components of `cpt-cf-oagw-component-model` (gear wiring, `config.rs`, `domain/dto.rs`, `domain/error.rs`, `domain/repo.rs`, `infra/storage/`, `infra/type_provisioning.rs`).

**Requirements** (already satisfied upstream and inherited, not re-delivered): `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config`. The merge engine in this feature is the realization the PRD marks as done for both.

Delivered by this feature:

- `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
- [x] `p1` - `cpt-cf-oagw-nfr-credential-isolation` - configuration-boundary slice only; secret resolution is owned by entry 2.6
- `p1` - `cpt-cf-oagw-contract-types-registry`

**Principles**: `cpt-cf-oagw-principle-tenant-scope`, `cpt-cf-oagw-principle-cred-isolation`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`, `cpt-cf-oagw-constraint-no-direct-internet`, `cpt-cf-oagw-constraint-multi-sql` (covered as a documented deviation, graded deviation 5)

**Sequences**: none. DESIGN.md defines no interaction sequence for gear wiring, configuration parsing, or repository access; the only sequence DESIGN.md defines is `cpt-cf-oagw-seq-proxy-flow`, which is owned by entry 2.4.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Supplies the `oagw.config` block, deploys the gear inside the single platform executable, and owns the gear-relative route tree registered here |
| `cpt-cf-oagw-actor-tenant-admin` | Owns tenant-scoped configuration records; is the subject of every effective-configuration merge and of every tenant-scoped repository read and write |
| `cpt-cf-oagw-actor-types-registry` | Receives the base GTS type registrations performed by the gear at initialization |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Schemas**: [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json), [schemas/route.v1.schema.json](../schemas/route.v1.schema.json)
- **Dependencies**: None. This is the root feature of the decomposition; `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`, and `cpt-cf-oagw-feature-request-proxy` depend on it directly, and the remaining entries depend on it transitively.

### 1.5 Scope Exclusions

The following areas are excluded from this feature and remain owned by the entries named below; nothing delivered here pre-implements them:

- Performance targets and hot-path latency: the proxy request-path latency budget is owned by `cpt-cf-oagw-feature-request-proxy` per `cpt-cf-oagw-nfr-low-latency`. This feature delivers the configuration, merge, and repository primitives that path consumes, and declares no latency target of its own.
- Metrics, audit logging, trace identifiers, and the health/readiness surface: owned by `cpt-cf-oagw-feature-observability-and-operability` per `cpt-cf-oagw-nfr-observability`. Foundation code emits no metric, no audit record, and no trace identifier.
- Cache integration: the Data Plane and Control Plane L1 caches are owned by `cpt-cf-oagw-feature-observability-and-operability` per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management`. The repository and the merge engine defined here sit behind those caches and hold no cache of their own.
- Rate limiting and throttling: owned by `cpt-cf-oagw-feature-rate-limiting`. `RateLimitConfig` is delivered here as configuration data only; no counter is constructed and no limit is evaluated by foundation code.
- Data privacy and regulatory compliance: no personal data is held in upstream, route, or plugin configuration, so no privacy or compliance processing surface is provided here.
- Accessibility: the gear exposes no user interface surface, so no accessibility requirement applies to it.

## 2. Actor Flows (CDSL)

**Use cases**: this feature exposes no end-user use case of its own. `cpt-cf-oagw-usecase-configure-upstream` and `cpt-cf-oagw-usecase-configure-route` consume the repository boundary and the domain types delivered here, and `cpt-cf-oagw-usecase-proxy-request` consumes the configuration model, the merge engine, and the gear-relative proxy route registration.

### Gear Wiring and Initialization

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-gear-init`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The ToolKit runtime constructs the gear, parses `oagw.config`, constructs the repositories and the services that consume them, registers the base GTS types, and reports the gear ready inside the single platform executable.

**Error Scenarios**:
- `oagw.config` cannot be parsed or a configured value is outside its legal range: initialization fails and the gear registers no REST routes.
- `types_registry` is unreachable or rejects a base type registration: initialization fails.
- Initialization is attempted on an already initialized gear instance: the second attempt fails with an already-initialized error and the first configuration is preserved.

The base-type registration call in step 5 is a synchronous SDK call made during initialization: its failure handling is owned by the platform's startup supervision, so a failed registration fails gear startup, and no gear-level timeout is configured around it. `cred_store`, `authz_resolver`, and `tenant_resolver` are resolved as handles only at initialization; they are first invoked by `cpt-cf-oagw-feature-plugin-system` and `cpt-cf-oagw-feature-request-proxy`, and this entry invokes none of them on the request path.

**Steps**:
1. [x] - `p1` - The ToolKit runtime instantiates the gear declared with `#[toolkit::gear]` and invokes `Gear::init` with the gear context - `inst-gf-init-1`
2. [x] - `p1` - Resolve `OagwConfig` from the gear context through the toolkit configuration lookup, applying the declared defaults for absent keys - `inst-gf-init-2`
3. [x] - `p1` - **IF** parsing or validation of `OagwConfig` fails - `inst-gf-init-3`
   1. [x] - `p1` - Abort initialization with the validation error and register no REST routes - `inst-gf-init-4`
4. [x] - `p1` - Construct the in-memory config-backed repositories behind the repository traits and the services that consume them, and resolve the external dependency clients for `types_registry`, `cred_store`, `authz_resolver`, and `tenant_resolver` from the client hub - `inst-gf-init-5`
5. [x] - `p1` - Register the base GTS types through `types_registry` - `inst-gf-init-6`
6. [x] - `p1` - **IF** a base type registration is rejected or the registry is unreachable - `inst-gf-init-7`
   1. [x] - `p1` - Abort initialization with the provisioning error - `inst-gf-init-8`
7. [x] - `p1` - Publish the service handles to the client hub and mark the gear ready - `inst-gf-init-9`
8. [x] - `p1` - **RETURN** an initialized gear whose REST surface is contributed through `RestApiCapability` and whose state is entirely in-process - `inst-gf-init-10`

### Gear-Relative REST Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-rest-registration`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The gear registers the gear-relative route tree under its own (empty) `prefix_path`: `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]`, with no leading `/api` segment.

**Error Scenarios**:
- The REST capability is unavailable: registration fails and initialization reports the failure instead of silently serving no routes.
- A registered path is requested before its owning entry delivers the handler: no configuration data is served and no domain service call is made.

**Authentication posture**: inbound bearer authentication on `/oagw/v1/...` is required, per the Authentication & Authorization subsection of DESIGN §3.3 (`cpt-cf-oagw-interface-api`), which requires authentication for all OAGW API requests. In the graded configuration the api-gateway's `require_auth_by_default` policy covers the registered tree, and a route with no matching OpenAPI specification falls back to that default, so no path in the tree is reachable unauthenticated. The handler-owning entries `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`, `cpt-cf-oagw-feature-request-proxy`, and `cpt-cf-oagw-feature-plugin-system` resolve the caller's security context and enforce the DESIGN permission table through the `authz_resolver` handle this entry resolves at initialization; this entry registers the tree unauthenticated at the handler level and delivers no handler, so no path serves data before a handler lands.

**Steps**:
1. [x] - `p1` - The ToolKit runtime invokes `RestApiCapability::register_rest` with the axum router owned by the api-gateway gear, whose `prefix_path` is empty in the graded configuration - `inst-gf-rest-1`
2. [x] - `p1` - Register the upstream, route, and plugin management prefixes and the proxy prefix under `/oagw/v1/...` with no `/api` segment and no gear-supplied prefix - `inst-gf-rest-2`
3. [x] - `p1` - Register `/oagw/v1/proxy/{alias}[/{path_suffix}]` for every HTTP method, because the proxy path is method-agnostic - `inst-gf-rest-3`
4. [x] - `p1` - Attach the shared service handles as router state so entries 2.2, 2.3, 2.4, and 2.6 can supply handler bodies without altering the registration skeleton - `inst-gf-rest-4`
5. [x] - `p1` - **IF** a registered path is addressed before its owning entry delivers the handler - `inst-gf-rest-5`
   1. [x] - `p1` - Return the framework unmatched-response behavior without invoking a repository or service call - `inst-gf-rest-6`
6. [x] - `p1` - **RETURN** the router with the gear-relative route tree merged into the router owned by the api-gateway gear - `inst-gf-rest-7`

### Configuration Load and Validation

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-config-load`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- An `oagw.config` block carrying any subset of the recognized keys parses into `OagwConfig` with the declared defaults applied for absent keys.
- An absent `oagw.config` block produces the full default configuration, so the gear still initializes.

**Error Scenarios**:
- A recognized key carries a value outside its legal range: the block is rejected with a validation error naming the key.
- An unrecognized key is present: the block is rejected, so a misspelled key cannot silently disable a control.
- A configuration sub-structure carries secret material instead of a `cred://` reference: the record is rejected at the configuration boundary.

**Steps**:
1. [x] - `p1` - Deserialize the `oagw.config` block into `OagwConfig` - `inst-gf-cfg-1`
2. [x] - `p1` - Apply the declared defaults: `allow_http_upstream` false, `token_cache_ttl_secs` 300, `token_cache_capacity` 10000, `ssrf_policy` disabled, the body-size limit of 100 MB, and `proxy_timeout_secs` 2 (seconds), the value the graded deployment configuration carries - `inst-gf-cfg-2`
3. [x] - `p1` - Validate ranges: positive `proxy_timeout_secs`, positive `token_cache_ttl_secs`, positive `token_cache_capacity`, and a body-size limit that does not exceed the 100 MB hard ceiling - `inst-gf-cfg-3`
4. [x] - `p1` - Admit the endpoint `scheme` value set `http | https | wss | wt | grpc` with `https` as the default wherever configuration names a scheme - `inst-gf-cfg-4`
5. [x] - `p1` - **IF** `allow_http_upstream` is false and a configured endpoint names the `http` scheme - `inst-gf-cfg-5`
   1. [x] - `p1` - Reject the record with a validation error naming the offending endpoint - `inst-gf-cfg-6`
6. [x] - `p1` - **IF** any credential-bearing field holds a value that is not a `cred://` reference - `inst-gf-cfg-7`
   1. [x] - `p1` - Reject the record with a validation error and write no secret material into any store, log, or error message - `inst-gf-cfg-8`
7. [x] - `p1` - **RETURN** the parsed `OagwConfig` with every default materialized - `inst-gf-cfg-9`

### Effective Configuration Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-effective-config`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- The effective configuration for a tenant is produced from the ordered layer list (upstream base, route, then tenant chain from root to leaf) using the per-field merge rules.
- A descendant that holds no override permission for an inherited field receives the ancestor's value as-is, with no error surfaced to the requester.

**Error Scenarios**:
- A descendant layer attempts to override a field whose ancestor layer carries `sharing: enforce`: the override is discarded and the enforced ancestor value is returned.
- An ancestor layer carries `sharing: private`: the layer is invisible to the requesting descendant and contributes nothing.

**Steps**:
1. [x] - `p1` - Collect the ordered layer list for the requesting tenant: the upstream base configuration, the matched route configuration, and the tenant chain from root to leaf - `inst-gf-eff-1`
2. [x] - `p1` - Resolve the requesting tenant's override permissions through `authz_resolver` before any `inherit` override is applied: an `inherit` override is honored only when the requester holds the corresponding override permission granted by the ancestor, which the DESIGN permission table names `oagw:upstream:override_auth`, `oagw:upstream:override_rate`, and `oagw:upstream:add_plugins` - `inst-gf-eff-8`
3. [x] - `p1` - Walk the layers from base to most specific and apply the per-field merge rules defined in `cpt-cf-oagw-algo-gear-foundation-config-merge` - `inst-gf-eff-2`
4. [x] - `p1` - **IF** an ancestor layer field carries `sharing: enforce` - `inst-gf-eff-3`
   1. [x] - `p1` - Keep the ancestor value and discard the descendant value for that field - `inst-gf-eff-4`
5. [x] - `p1` - **IF** an ancestor layer field carries `sharing: private` and the requester is a descendant - `inst-gf-eff-5`
   1. [x] - `p1` - Skip that field so it contributes nothing to the effective configuration - `inst-gf-eff-6`
6. [x] - `p1` - **RETURN** one effective configuration in which absent fields carry the inherited value and enforced ancestor constraints are retained - `inst-gf-eff-7`

### Tenant-Scoped Repository Access

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-repo-access`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:
- A read or write touches only records owned by the caller's tenant; ancestor records are never returned through this boundary.

**Error Scenarios**:
- A call targets a record owned by another tenant, including an ancestor: the result is not-found and the other tenant's record is never disclosed.
- A write would violate a per-tenant uniqueness key, such as `(tenant_id, alias)` for an upstream: the write is rejected with a conflict.

**Steps**:
1. [x] - `p1` - Receive a repository call carrying the caller's tenant identifier and the target record key - `inst-gf-repo-1`
2. [x] - `p1` - Bind every lookup and every write to the caller's tenant identifier before the store is consulted - `inst-gf-repo-2`
3. [x] - `p1` - **IF** the resolved record is owned by a different tenant - `inst-gf-repo-3`
   1. [x] - `p1` - Return not-found without disclosing the record's existence - `inst-gf-repo-4`
4. [x] - `p1` - **IF** a write would violate a per-tenant uniqueness key - `inst-gf-repo-5`
   1. [x] - `p1` - Reject the write with a conflict and leave the store unchanged - `inst-gf-repo-6`
5. [x] - `p1` - **RETURN** the tenant-scoped result or the specific error - `inst-gf-repo-7`

### GTS Type Provisioning

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-type-provisioning`

**Actor**: `cpt-cf-oagw-actor-types-registry`

**Success Scenarios**:
- The base types `gts.cf.core.oagw.upstream.v1~`, `gts.cf.core.oagw.route.v1~`, and the plugin base types `gts.cf.core.oagw.auth_plugin.v1~`, `gts.cf.core.oagw.guard_plugin.v1~`, and `gts.cf.core.oagw.transform_plugin.v1~` are registered during gear initialization, and repeating the registration is idempotent.

**Error Scenarios**:
- The registry rejects a base type or is unreachable: initialization fails rather than starting with a partial catalog.

**Steps**:
1. [x] - `p1` - Obtain the `types_registry` client from the client hub during initialization - `inst-gf-gts-1`
2. [x] - `p1` - Register the upstream, route, and plugin base types through `infra/type_provisioning.rs` - `inst-gf-gts-2`
3. [x] - `p1` - Treat an already registered base type as a successful no-op so initialization is repeatable - `inst-gf-gts-3`
4. [x] - `p1` - **IF** any registration fails - `inst-gf-gts-4`
   1. [x] - `p1` - Fail gear initialization with the provisioning error - `inst-gf-gts-5`
5. [x] - `p1` - **RETURN** a provisioned type catalog on which entry 2.2, 2.3, and 2.6 resource identifiers resolve - `inst-gf-gts-6`

## 3. Processes / Business Logic (CDSL)

### Hierarchical Configuration Merge

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-gear-foundation-config-merge`

**Input**: the ordered layer list for one requester: the upstream base configuration, the route configuration, and the tenant-chain entries from root to leaf. Each mergeable field optionally carries `sharing` with the value `private`, `inherit`, or `enforce`.
**Output**: one effective configuration.

**Steps**:
1. [x] - `p1` - Start from the upstream base layer as the initial effective value - `inst-gf-merge-1`
2. [x] - `p1` - Iterate the remaining layers in increasing priority: route, then tenant chain from root to leaf, honoring the priority order Upstream (base) < Route < Tenant - `inst-gf-merge-2`
3. [x] - `p1` - **IF** a layer field carries `sharing: private` and the requester is a descendant of the owning tenant - `inst-gf-merge-3`
   1. [x] - `p1` - Skip the field so it is not visible to the descendant - `inst-gf-merge-4`
4. [x] - `p1` - **IF** a layer field carries `sharing: enforce` - `inst-gf-merge-5`
   1. [x] - `p1` - Keep the ancestor value and discard every descendant value for that field, including across alias shadowing - `inst-gf-merge-6`
5. [x] - `p1` - Merge auth by override: a descendant value replaces an `inherit` ancestor value only when the requesting tenant holds `oagw:upstream:override_auth` through `authz_resolver`, an `enforce` ancestor value is always retained, and a descendant without that permission keeps the ancestor value as-is with no error surfaced to the requester - `inst-gf-merge-7`
6. [x] - `p1` - Merge rate limits by `min(ancestor, descendant)` across every present layer so the stricter value always wins and enforced ancestor limits are retained, where a descendant contributes its own limit only when it holds `oagw:upstream:override_rate` through `authz_resolver` and otherwise leaves the ancestor minimum standing unchanged with no error surfaced - `inst-gf-merge-8`
7. [x] - `p1` - Merge tags by add-only union so inherited tags cannot be removed and descendant tags remain tenant-local additions that never mutate an ancestor record - `inst-gf-merge-9`
8. [x] - `p1` - Merge CORS origins by union when the ancestor layer is `inherit`, and keep the enforced ancestor origin set as-is when it is `enforce` - `inst-gf-merge-10`
9. [x] - `p1` - Merge plugin chains by concatenation in ancestor-then-descendant order so enforced bindings cannot be removed, where a descendant appends its own bindings only when it holds `oagw:upstream:add_plugins` through `authz_resolver`, and otherwise the inherited chain stands unchanged with no error surfaced - `inst-gf-merge-11`
10. [x] - `p1` - Merge scalar fields with no sharing semantics by taking the more specific value when present - `inst-gf-merge-12`
11. [x] - `p1` - Leave a field that no layer specifies absent rather than substituting an implicit value - `inst-gf-merge-13`
12. [x] - `p1` - **RETURN** the effective configuration - `inst-gf-merge-14`

### Tenant-Scoped Repository Operations

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-gear-foundation-repo-scope`

**Input**: a repository operation (read, list, create, replace, delete) with the caller's tenant identifier, the target key, and the record payload for writes.
**Output**: the tenant-scoped result, or a specific error.

**Steps**:
1. [x] - `p1` - Bind the operation to the caller's tenant identifier as part of the store key, so no query path exists that omits it - `inst-gf-scope-1`
2. [x] - `p1` - Apply the per-tenant uniqueness keys on write: `(tenant_id, alias)` for an upstream, `(tenant_id, name)` for a plugin, and match-rule uniqueness for a route - `inst-gf-scope-2`
3. [x] - `p1` - **IF** a write touches more than one record, such as a record plus its ordered child bindings - `inst-gf-scope-3`
   1. [x] - `p1` - Apply the whole write atomically so no partially written record is ever observable - `inst-gf-scope-4`
4. [x] - `p1` - Validate ordered child binding positions as contiguous from zero and reject gaps or duplicates on write - `inst-gf-scope-5`
5. [x] - `p1` - Enforce route match determinism by rejecting a second enabled route under the same upstream that shares the same path prefix and priority for the same method - `inst-gf-scope-6`
6. [x] - `p1` - Store `plugin_ref` on every binding and store `plugin_uuid` only when the reference is UUID-backed, rejecting a binding whose two values disagree - `inst-gf-scope-7`
7. [x] - `p1` - **RETURN** the result, not-found for a foreign-tenant or missing key, or conflict for a uniqueness violation - `inst-gf-scope-8`

### Credential-Reference Boundary

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-gear-foundation-credential-boundary`

**Input**: a configuration payload at the configuration boundary, and any record, log line, error message, or API response derived from one.
**Output**: an accepted `cred://` reference, or a rejection or redaction.

**Steps**:
1. [x] - `p1` - Inspect every credential-bearing field of a configuration sub-structure before it is stored - `inst-gf-cred-1`
2. [x] - `p1` - Accept only a `cred://` URI reference as a credential value - `inst-gf-cred-2`
3. [x] - `p1` - **IF** a credential-bearing field holds secret material or any other non-`cred://` value - `inst-gf-cred-3`
   1. [x] - `p1` - Reject the record with a validation error that names the field but never echoes the rejected value - `inst-gf-cred-4`
4. [x] - `p1` - Write no secret material into a stored record, a log line, an error message, or an API response, and redact any credential-bearing field on the way out - `inst-gf-cred-5`
5. [x] - `p1` - Leave secret resolution to entry 2.6, which resolves `cred://` references through `cred_store` at request time - `inst-gf-cred-6`

### Performance Cost Expectations

- The effective-configuration merge is executed per request: its cost is proportional to the number of configuration layers walked and the number of fields merged, and its result is **not** cached downstream by `cpt-cf-oagw-feature-observability-and-operability`, because per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management` the L1 caches key resolved upstreams and routes, not merged configuration documents.
- Repository lookups are constant-time in-memory map reads, with the Data Plane L1 hot-config cache (1,000-entry LRU) and the Control Plane L1 configuration cache (10,000-entry LRU) in front of them; both caches are owned by `cpt-cf-oagw-feature-observability-and-operability`, and the repository this feature delivers is the miss path behind them.
- The in-memory store's capacity is bounded only by the volume of stored configuration, and it applies no eviction policy: a record leaves the store only through an explicit delete or through the restart rebuild from the configuration source.
- No request-path latency target is owned by this feature: the proxy-path latency budget is owned by `cpt-cf-oagw-feature-request-proxy` per `cpt-cf-oagw-nfr-low-latency`.

## 4. States (CDSL)

### Gear Wiring State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-foundation-gear-wiring`

**States**: `uninitialized`, `initializing`, `ready`, `failed`
**Initial State**: `uninitialized`
**Transitions**:
1. [x] - `p1` - **FROM** `uninitialized` **TO** `initializing` **WHEN** the ToolKit runtime invokes `Gear::init` with the gear context - `inst-gf-st-gear-1`
2. [x] - `p1` - **FROM** `initializing` **TO** `ready` **WHEN** `OagwConfig` parses, the repositories and services construct, and the base GTS types register - `inst-gf-st-gear-2`
3. [x] - `p1` - **FROM** `initializing` **TO** `failed` **WHEN** any initialization step errors, in which case no REST route is registered and no service handle is published - `inst-gf-st-gear-3`
4. [x] - `p1` - **FROM** `failed` **TO** `uninitialized` **WHEN** the runtime discards the failed instance so a later start begins from a clean state - `inst-gf-st-gear-4`

### Stored Configuration Record State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-foundation-config-record`

**States**: `absent`, `active`, `removed`
**Initial State**: `absent`
**Transitions**:
1. [x] - `p1` - **FROM** `absent` **TO** `active` **WHEN** a create is accepted for a key that satisfies tenant scoping and the per-tenant uniqueness key - `inst-gf-st-rec-1`
2. [x] - `p1` - **FROM** `active` **TO** `active` **WHEN** a full replacement is accepted for the same record identity, with `id` and `tenant_id` immutable - `inst-gf-st-rec-2`
3. [x] - `p1` - **FROM** `active` **TO** `removed` **WHEN** a delete is accepted for a record owned by the calling tenant - `inst-gf-st-rec-3`
4. [x] - `p1` - **FROM** `removed` **TO** `absent` **WHEN** the in-memory entry is released and no ordered child binding references it - `inst-gf-st-rec-4`

## 5. Definitions of Done

### ToolKit Gear Wiring

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-gear-wiring`

The system **MUST** declare the `oagw` crate as a single ToolKit gear in `gear.rs` annotated with `#[toolkit::gear]`, implement `Gear::init` to parse `OagwConfig`, construct the repositories and services, and publish the service handles to the client hub, and expose REST through `RestApiCapability`, so the gear runs inside the single platform executable with no separate deployment unit and no database capability. Initialization **MUST** be idempotent per process and **MUST** fail fast on an invalid configuration or a failed type registration.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-gear-init`
- `cpt-cf-oagw-state-gear-foundation-gear-wiring`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`, `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:
- API: none (route registration is covered by `cpt-cf-oagw-dod-gear-foundation-rest-registration`)
- Entities: `OagwConfig`
- Tests: unit tests in `src/gear_tests.rs` covering successful init, double-init rejection, and fail-fast on an invalid config block

### Gear-Relative REST Registration Skeleton

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-rest-registration`

The system **MUST** register, through `RestApiCapability::register_rest`, the gear-relative route tree `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]` under the gear's own (empty) `prefix_path`, with no leading `/api` segment, because the api-gateway gear owns the router in the graded configuration. The entry **MUST NOT** implement any endpoint business logic: handler bodies for upstreams, routes, plugins, and proxy requests belong to entries 2.2, 2.3, 2.4, and 2.6, and until they land no registered path may serve configuration data or invoke a repository.

Inbound bearer authentication on `/oagw/v1/...` **MUST** be required on the registered tree, per the Authentication & Authorization subsection of DESIGN §3.3 (`cpt-cf-oagw-interface-api`). In the graded configuration the api-gateway's `require_auth_by_default` policy covers the registered tree, and a route with no matching OpenAPI specification falls back to that default. This entry registers the tree unauthenticated at the handler level and delivers no handler, so no path serves data before a handler lands; the handler-owning entries `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`, `cpt-cf-oagw-feature-request-proxy`, and `cpt-cf-oagw-feature-plugin-system` resolve the caller's security context and enforce the DESIGN permission table through the `authz_resolver` handle this entry resolves at initialization.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-rest-registration`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: registration only - `GET /oagw/v1/upstreams`, `GET /oagw/v1/routes`, `GET /oagw/v1/plugins`, `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: none
- Tests: integration tests in `tests/gear_registration.rs` asserting the four gear-relative prefixes exist, no `/api/oagw` route exists, the proxy prefix accepts every HTTP method, and no registered path bypasses the api-gateway's authentication default

### OagwConfig Parsing and Defaults

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-config-model`

The system **MUST** parse `oagw.config` into `OagwConfig` in `config.rs`, applying one declared default per key so absent keys yield a complete, secure configuration: `allow_http_upstream` false, `token_cache_ttl_secs` 300, `token_cache_capacity` 10000, `ssrf_policy` disabled, the body-size limit of 100 MB, and `proxy_timeout_secs` 2 (seconds), the value the graded deployment configuration carries. The system **MUST** reject an out-of-range value, an unknown key, and any endpoint `scheme` outside `http | https | wss | wt | grpc`, and **MUST** admit the `http` scheme only while `allow_http_upstream` is true. `cpt-cf-oagw-constraint-https-only` is the default-TLS posture; admitting the `http` scheme under `allow_http_upstream: true` is the documented lift recorded as graded deviation 2 of the decomposition.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-config-load`
- `cpt-cf-oagw-algo-gear-foundation-credential-boundary`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`, `cpt-cf-oagw-constraint-https-only` (default-TLS posture, lifted under graded deviation 2)

**Touches**:
- API: none
- Entities: `OagwConfig`, `Endpoint`
- Tests: unit tests in `src/config_tests.rs` for the full default set including the `proxy_timeout_secs` default of 2, range rejection, unknown-key rejection, and the `http` scheme gate

### Hierarchical Configuration Merge Engine

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-gear-foundation-merge-engine`

The system **MUST** produce one effective configuration from the ordered layer list using the priority order Upstream (base) < Route < Tenant and the sharing modes `private`, `inherit`, and `enforce`, applying `min()` to rate limits, add-only union to tags, union to CORS origins, concatenation to plugin chains, and override to auth, so that an enforced ancestor constraint survives alias shadowing and a descendant can never remove an inherited tag or an enforced binding.

An `inherit` override **MUST** be permission-gated: it is honored only when the requesting tenant holds the corresponding override permission granted by the ancestor, resolved through `authz_resolver` at merge time, which the DESIGN permission table names `oagw:upstream:override_auth`, `oagw:upstream:override_rate`, and `oagw:upstream:add_plugins`. A descendant that holds no such permission **MUST** receive the ancestor value as-is, and no error **MUST** be surfaced to the requester for the withheld override.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-config-merge`
- `cpt-cf-oagw-flow-gear-foundation-effective-config`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `AuthConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
- Tests: unit tests in a sibling `*_tests.rs` module of the domain layer (for example `src/domain/merge_tests.rs`) covering each merge rule, all three sharing modes, enforcement across shadowing, and the permission-gated `inherit` override for auth, rate limits, and plugin additions, including the case where the override permission is absent and the ancestor value stands

### Shared Domain Model Types

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-domain-model`

The system **MUST** define, in `domain/dto.rs`, the domain model types shared by the Control Plane and the Data Plane: `Upstream`, `Route`, `Plugin`, `ServerConfig`, and `Endpoint`, together with their configuration sub-structures `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`, and `MatchConfig`, matching the shapes of `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`, with `Upstream` unique per `(tenant_id, alias)` and `Route` belonging to exactly one upstream.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-config-load`
- `cpt-cf-oagw-state-gear-foundation-config-record`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Plugin`, `ServerConfig`, `Endpoint`
- Tests: unit tests in `src/domain/dto_tests.rs` for serialization round-trips against the two schemas and for the `https` scheme default

### Domain Error Type

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-domain-error`

The system **MUST** define `DomainError` in `domain/error.rs` as the single error taxonomy carried across both planes and across the repository boundary, carrying enough information for entry 2.5 to map it onto the HTTP status and GTS error type of the DESIGN error table; this entry **MUST NOT** render any HTTP error body.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-repo-access`
- `cpt-cf-oagw-algo-gear-foundation-repo-scope`

**Touches**:
- API: none (problem+json rendering is owned by entry 2.5)
- Entities: `DomainError`
- Tests: unit tests in `src/domain/error_tests.rs` for the error taxonomy and for error text that contains no credential material

### Repository Traits and In-Memory Implementation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-gear-foundation-repository-boundary`

The system **MUST** define the repository traits `UpstreamRepository`, `RouteRepository`, and the plugin repository in `domain/repo.rs`, and **MUST** provide an in-memory, config-backed implementation in `infra/storage/` that preserves the DESIGN table shapes as the documented schema contract. The store **MUST** hold no durable storage, **MUST** be rebuilt from the configuration source on restart, and **MUST** be the only persistence path available to both planes.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-repo-scope`
- `cpt-cf-oagw-state-gear-foundation-config-record`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Plugin`
- Tests: unit tests in `src/infra/storage/*_tests.rs` for create, replace, delete, uniqueness conflicts, and atomic multi-record writes

### Strict Tenant Scoping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-tenant-scoping`

The system **MUST** scope every repository read and write to the caller's tenant identifier, so that zero cross-tenant data access is possible through the boundary, ancestor resources are invisible to descendants, and every result is derived from a query bound to the caller's tenant. A write that would cross a tenant boundary **MUST** be rejected rather than silently retargeted. This realizes the configuration-boundary half of `cpt-cf-oagw-nfr-multi-tenancy`; the alias-hierarchy walk used at proxy time is owned by entry 2.4.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-repo-access`
- `cpt-cf-oagw-algo-gear-foundation-repo-scope`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Plugin`
- Tests: unit tests in `src/infra/storage/tenant_scope_tests.rs` and integration tests in `tests/repository_tenant_scope.rs` asserting zero cross-tenant disclosure

### Credential Isolation at the Configuration Boundary

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-credential-boundary`

The system **MUST** ensure that configuration sub-structures carry `cred://` references only: no secret material is parsed, stored, logged, emitted in an error message, or returned in an API response by foundation code, and every credential-bearing field that does not hold a `cred://` reference is rejected at the configuration boundary. This is the configuration-boundary slice of `cpt-cf-oagw-nfr-credential-isolation`; resolution through `cred_store` is owned by entry 2.6 under `cpt-cf-oagw-contract-cred-store`.

**Implements**:
- `cpt-cf-oagw-algo-gear-foundation-credential-boundary`
- `cpt-cf-oagw-flow-gear-foundation-config-load`

**Touches**:
- API: none
- Entities: `AuthConfig`, `Plugin`
- Tests: unit tests in `src/config_tests.rs` for `cred://` acceptance and non-reference rejection, plus integration tests in `tests/credential_boundary.rs` asserting no secret material appears in any stored record, log line, or error body

### GTS Type Provisioning

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-type-provisioning`

The system **MUST** register the base types `gts.cf.core.oagw.upstream.v1~`, `gts.cf.core.oagw.route.v1~`, and the plugin base types `gts.cf.core.oagw.auth_plugin.v1~`, `gts.cf.core.oagw.guard_plugin.v1~`, and `gts.cf.core.oagw.transform_plugin.v1~` through `infra/type_provisioning.rs` using the `types_registry` client, idempotently and at initialization, and **MUST** fail initialization when the registry rejects a base type. The registry-reference posture of graded deviation 6 applies: plugin identifiers are resolvable as references, and no plugin execution surface is provisioned here.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-type-provisioning`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Plugin`
- Tests: unit tests in `src/infra/type_provisioning_tests.rs` and integration tests in `tests/type_provisioning.rs` for the registered type set and for repeat-registration idempotency

### Documented Schema Contract

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-gear-foundation-schema-contract`

The system **MUST** preserve the DESIGN table shapes for `oagw_upstream` (primary key `id`, unique `(tenant_id, alias)`), `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag` / `oagw_route_tag`, `oagw_plugin` (unique `(tenant_id, name)`), and `oagw_upstream_plugin` / `oagw_route_plugin` as the documented schema contract for a future SQL backend, while materializing them as the in-memory, config-backed repository in the graded configuration. The deviation from `cpt-cf-oagw-constraint-multi-sql` **MUST** remain recorded here, because the crate manifest declares no `toolkit-db` or SeaORM dependency and the graded configuration has no OAGW database block.

**Implements**:
- `cpt-cf-oagw-dod-gear-foundation-repository-boundary`
- `cpt-cf-oagw-algo-gear-foundation-repo-scope`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:
- API: none
- Entities: `Upstream`, `Route`, `Plugin`
- Tests: integration tests in `tests/schema_contract.rs` asserting that the in-memory store exposes the documented key and uniqueness shapes

### DDD-Light Layer Boundaries

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-gear-foundation-layer-boundaries`

The system **MUST** organize the crate into the DDD-Light layers `api/rest/`, `domain/`, and `infra/`, where the domain layer declares the service and repository contracts with no infrastructure dependency, the infrastructure layer implements the domain traits, and the transport layer maps between HTTP and domain types, so that each later entry can be implemented against the boundary without crossing it.

**Implements**:
- `cpt-cf-oagw-flow-gear-foundation-gear-init`
- `cpt-cf-oagw-flow-gear-foundation-rest-registration`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: transport layer skeleton only - `api/rest/`, `api/rest/routes/`, `api/rest/handlers/`
- Entities: none
- Tests: a unit test in `src/domain/layer_boundary_tests.rs` asserting that domain modules reference no infrastructure module

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering configuration parsing and defaults, merge semantics for every field rule and sharing mode, repository operations and their uniqueness invariants, tenant scoping, credential-reference rejection, and type provisioning, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4.

**Implements**:
- `cpt-cf-oagw-dod-gear-foundation-config-model`
- `cpt-cf-oagw-dod-gear-foundation-merge-engine`
- `cpt-cf-oagw-dod-gear-foundation-credential-boundary`
- `cpt-cf-oagw-dod-gear-foundation-type-provisioning`

**Touches**:
- API: none
- Entities: `OagwConfig`, `Upstream`, `Route`, `Plugin`
- Tests: `src/config_tests.rs`, `src/domain/merge_tests.rs`, `src/infra/storage/*_tests.rs`, `src/infra/type_provisioning_tests.rs`

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering gear initialization against a real configuration block, the registered gear-relative route tree, repository tenant scoping across two tenants, and the absence of secret material in stored records and rendered errors, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-gear-foundation-gear-wiring`
- `cpt-cf-oagw-dod-gear-foundation-rest-registration`
- `cpt-cf-oagw-dod-gear-foundation-tenant-scoping`
- `cpt-cf-oagw-dod-gear-foundation-credential-boundary`

**Touches**:
- API: registration verification - `GET /oagw/v1/upstreams`, `GET /oagw/v1/routes`, `GET /oagw/v1/plugins`, `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `OagwConfig`, `Upstream`, `Route`, `Plugin`
- Tests: `tests/gear_registration.rs`, `tests/repository_tenant_scope.rs`, `tests/credential_boundary.rs`, `tests/type_provisioning.rs`, `tests/schema_contract.rs`

## 6. Acceptance Criteria

- [x] `Gear::init` completes against a valid `oagw.config` block, publishes its services to the client hub, and a second initialization attempt on the same instance fails instead of overwriting state (DoD `cpt-cf-oagw-dod-gear-foundation-gear-wiring`).
- [x] The registered route tree exposes `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]` with no `/api` prefix, and no registered path serves configuration data before its owning entry delivers the handler (DoD `cpt-cf-oagw-dod-gear-foundation-rest-registration`).
- [x] Every path in the registered tree requires inbound bearer authentication, covered in the graded configuration by the api-gateway's `require_auth_by_default` policy with routes lacking a matching OpenAPI specification falling back to that default, and the handler-owning entries enforce permissions through the `authz_resolver` handle resolved at initialization (DoD `cpt-cf-oagw-dod-gear-foundation-rest-registration`).
- [x] An absent `oagw.config` block yields the full default set, including `allow_http_upstream` false, `token_cache_ttl_secs` 300, `token_cache_capacity` 10000, `ssrf_policy` disabled, the 100 MB body-size limit, and `proxy_timeout_secs` 2, and every default is asserted by a unit test (DoD `cpt-cf-oagw-dod-gear-foundation-config-model`).
- [x] An out-of-range value, an unknown key, or an endpoint `scheme` outside `http | https | wss | wt | grpc` fails initialization with a validation error naming the offending key, and `http` is admitted only while `allow_http_upstream` is true, which is the documented lift of `cpt-cf-oagw-constraint-https-only` recorded as graded deviation 2 (DoD `cpt-cf-oagw-dod-gear-foundation-config-model`).
- [x] A three-level tenant hierarchy produces the documented effective configuration: `min()` rate limits, add-only tag union, unioned CORS origins under `inherit`, concatenated plugin chains, and overridden auth under `inherit` (DoD `cpt-cf-oagw-dod-gear-foundation-merge-engine`).
- [x] A descendant that holds none of the ancestor's override permissions receives the ancestor's auth, rate-limit, and plugin-chain values as-is with no error surfaced, while a descendant that holds them obtains the documented override behavior (DoD `cpt-cf-oagw-dod-gear-foundation-merge-engine`).
- [x] An ancestor field with `sharing: enforce` survives descendant shadowing unchanged, and an ancestor field with `sharing: private` contributes nothing to a descendant's effective configuration (DoD `cpt-cf-oagw-dod-gear-foundation-merge-engine`).
- [x] A repository call from tenant B never returns a record owned by tenant A, including an ancestor record, and returns not-found instead of the foreign record (DoD `cpt-cf-oagw-dod-gear-foundation-tenant-scoping`).
- [x] A second upstream with the same `(tenant_id, alias)` is rejected with a conflict and the store is left unchanged (DoD `cpt-cf-oagw-dod-gear-foundation-repository-boundary`).
- [x] No credential-bearing field that is not a `cred://` reference is accepted, and no log line, stored record, error message, or API response emitted by foundation code contains secret material (DoD `cpt-cf-oagw-dod-gear-foundation-credential-boundary`).
- [x] The five base GTS types are registered at initialization, a repeated registration is a no-op, and a rejected registration fails initialization (DoD `cpt-cf-oagw-dod-gear-foundation-type-provisioning`).
- [x] The DESIGN table shapes, including the `oagw_upstream` unique key `(tenant_id, alias)` and the ordered plugin binding positions, are preserved by the in-memory store and asserted by an integration test (DoD `cpt-cf-oagw-dod-gear-foundation-schema-contract`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-gear-foundation-unit-tests`, `cpt-cf-oagw-dod-gear-foundation-integration-tests`).
