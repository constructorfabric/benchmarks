# Feature: Control Plane Configuration API


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Feature-Local Deviations from Shared Baselines](#15-feature-local-deviations-from-shared-baselines)
  - [1.6 Explicit Non-Applicability](#16-explicit-non-applicability)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Configure an Upstream](#configure-an-upstream)
  - [Configure a Route](#configure-a-route)
  - [Read and List Configuration](#read-and-list-configuration)
  - [Replace or Delete an Upstream](#replace-or-delete-an-upstream)
  - [Delete a Route](#delete-a-route)
  - [Enable or Disable Configuration](#enable-or-disable-configuration)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Management Write Validation](#management-write-validation)
  - [Alias Derivation and Enforcement](#alias-derivation-and-enforcement)
  - [Tenant Scoping and Ancestor Non-Addressability](#tenant-scoping-and-ancestor-non-addressability)
  - [OData List Parameter Parsing and Bounding](#odata-list-parameter-parsing-and-bounding)
  - [Full-Replacement Diff](#full-replacement-diff)
  - [Route Match Uniqueness](#route-match-uniqueness)
- [4. States (CDSL)](#4-states-cdsl)
  - [Upstream and Route Effective Lifecycle](#upstream-and-route-effective-lifecycle)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Management Route Registration](#management-route-registration)
  - [Authentication and Management Permissions](#authentication-and-management-permissions)
  - [Request Validation Against the Shipped Schemas](#request-validation-against-the-shipped-schemas)
  - [Alias Derivation, Normalization, and Uniqueness](#alias-derivation-normalization-and-uniqueness)
  - [Tenant Scoping on Every Operation](#tenant-scoping-on-every-operation)
  - [Persisted Model Shape and Transactional Writes](#persisted-model-shape-and-transactional-writes)
  - [Full-Replacement Semantics](#full-replacement-semantics)
  - [Enable and Disable Semantics](#enable-and-disable-semantics)
  - [List and Query Parameters](#list-and-query-parameters)
  - [Colocated Tests](#colocated-tests)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-control-plane-config-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-control-plane-config`
## 1. Feature Context

### 1.1 Overview

This feature is the management half of the `oagw` gear. It registers the ten upstream and route endpoints at `/oagw/v1/...`, validates every write against the two shipped JSON Schemas, derives and enforces aliases, keeps every operation strictly tenant-scoped, and persists the result into the `oagw_*` upstream, route, tag, and match tables. It is the persisted state that `cpt-cf-oagw-feature-hierarchical-config` walks and `cpt-cf-oagw-feature-data-plane-proxy` serves.

### 1.2 Purpose

DECOMPOSITION §2.2 places this feature second in the feature graph, behind `cpt-cf-oagw-feature-gear-foundation` and ahead of every feature that reads configuration. The foundation supplies the gear registration, the domain types, the alias and hostname value objects, the `Scheme` admission predicate, and the `DomainError` catalogue; this feature turns them into a working write path. Without it there is no upstream and no route anywhere in the gear, so `hierarchical-config` has nothing to walk, `data-plane-proxy` has nothing to resolve, and `observability` has no configuration change to report.

Deliverables:

- The ten management endpoints of DECOMPOSITION §2.2, registered gear-relative under `/oagw/v1` with anonymous GTS identifiers in every path parameter.
- Request validation against `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`, including the endpoint shape, the protocol enum, the sharing enums, the match shape, the `rate_limit` sub-object, and the route-level `cors` object.
- Alias derivation by endpoint type, alias normalization, alias immutability across updates, and `(tenant_id, alias)` uniqueness.
- `enabled` semantics: default `true`, the transition rules, and the ancestor-disable guard.
- The persisted model this feature owns: the upstream, route, match, and tag tables with their keys, foreign keys, and single-transaction multi-table writes.
- OData list parameters `$filter`, `$select`, `$orderby`, `$top`, and `$skip`.

The feature is delivered in the three phases DECOMPOSITION §2.2 names: upstream CRUD, then route CRUD, then the list and query parameters. Nothing in the phase order changes the contract of any endpoint.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
- [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
- [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
- [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
- [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
- [ ] `p1` - `cpt-cf-oagw-interface-management-api`
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
- [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`

**Principles**:

- `cpt-cf-oagw-principle-tenant-scope`
- `cpt-cf-oagw-adr-request-routing`

**Constraints**:

- `cpt-cf-oagw-constraint-multi-sql`
- `cpt-cf-oagw-constraint-https-only`
- `cpt-cf-oagw-constraint-toolkit-deploy`

**Design Components**:

- `cpt-cf-oagw-component-model`
- `cpt-cf-oagw-interface-api`
- `cpt-cf-oagw-interface-management-api`
- `cpt-cf-oagw-tech-dependencies`

**Data**:

- `cpt-cf-oagw-db-schema`

`cpt-cf-oagw-interface-management-api` is a PRD §7.1 declaration and `cpt-cf-oagw-interface-api` is the DESIGN §3.3 contract section whose table carries the ten upstream and route rows this feature implements alongside the five plugin rows that belong to `cpt-cf-oagw-feature-plugin-system`: this feature restates that upstream contract in §2 and §5 and does not redesign a single endpoint. The `cpt-cf-oagw-db-schema` claim is shared — DECOMPOSITION §1.6 assigns this feature the upstream, route, tag, and match tables and leaves the plugin and plugin-binding tables to `cpt-cf-oagw-feature-plugin-system`.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-tenant-admin` | Creates, reads, replaces, enables, disables, and deletes the upstreams and routes of its own tenant through the management endpoints, and receives 404 for any resource owned by an ancestor. |
| `cpt-cf-oagw-actor-platform-operator` | Drives the same surface for configuration it owns and for the ancestor-side disable that must propagate to descendants; PRD §8 names it the actor of both use cases this feature implements. |

Both actors reach this feature through the same ten endpoints; the difference between them is which tenant the bearer token resolves to, not which handler runs. PRD §5.1 names both as the actors of `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-route-mgmt`, and `cpt-cf-oagw-fr-enable-disable`, and DECOMPOSITION §1.5 lists this feature against both.

The other four actors do not participate:

- `cpt-cf-oagw-actor-app-developer` has no management surface. Its endpoint is the proxy, delivered by `cpt-cf-oagw-feature-data-plane-proxy`.
- `cpt-cf-oagw-actor-cred-store` is not called. The credential reference inside the `auth` sub-configuration is validated for shape only, exactly as the foundation's value object does it; resolving the reference happens at proxy time. DECOMPOSITION §1.5 lists the credential store under `cpt-cf-oagw-feature-plugin-system` alone.
- `cpt-cf-oagw-actor-types-registry` is not called during a management operation. The GTS type catalogue was provisioned once by `cpt-cf-oagw-feature-gear-foundation`; no write here registers or re-registers a type.
- `cpt-cf-oagw-actor-upstream-service` is never contacted. No management operation opens an outbound connection.

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` — this feature persists and validates the domain types it declares, reuses its `DomainError` catalogue for the 400/404/409 answers below, calls its alias and hostname normalization routine on every value it stores, and registers its handlers on the router mount point it created (DECOMPOSITION §3).

Supporting sources this feature stays consistent with:

- [schemas/upstream.v1.schema.json](../schemas/upstream.v1.schema.json) and [schemas/route.v1.schema.json](../schemas/route.v1.schema.json) — the shapes every management write is validated against. Both are frozen inputs; the places this run overrides them are recorded in §1.5.
- [ADR/0001-request-routing.md](../ADR/0001-request-routing.md) (`cpt-cf-oagw-adr-request-routing`) — path-based routing. Every path this feature registers under `/oagw/v1/upstreams/*` and `/oagw/v1/routes/*` is routed to the Control Plane, and the management operation order below is the ADR's management flow.
- [ADR/0004-cors.md](../ADR/0004-cors.md) — CORS is a dedicated `cors` field on `Upstream` and `Route`, not a plugin; this feature validates that field.
- [ADR/0006-state-management.md](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management`) — the Control Plane writes to the database, flushes its own cache, and returns. This feature performs that write and that flush, and it owns the Control Plane L1 configuration cache the ADR assigns to the Control Plane: the cache is built here, and it is the only cache this feature owns. The Data Plane L1 cache and its post-write invalidation belong to `cpt-cf-oagw-feature-data-plane-proxy` (DECOMPOSITION §2.5, the explicit-invalidation alternative of ADR 0006), which also dispositions that ADR's periodic-sync alternative (DECOMPOSITION §1.3(10)).
- [ADR/0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md) — every gateway error this feature answers carries `X-OAGW-Error-Source: gateway` and an `application/problem+json` body through the foundation's mapping.
- [config/e2e-local.yaml](../../../../../config/e2e-local.yaml) — the graded configuration. Its `oagw.config` block sets `allow_http_upstream: true`, which is the input that admits the `http` endpoint scheme at write time.

**Run-level assumptions** — premises this feature relies on that come from the platform runtime rather than from PRD, DESIGN, the ADRs, or DECOMPOSITION:

- Assumption: the platform middleware stack authenticates the bearer token and resolves the caller's tenant before the request reaches this feature's handlers. `config/e2e-local.yaml` sets `api-gateway.auth_disabled: false` and `require_auth_by_default: true`, and DESIGN §3.3 names `toolkit-auth` as the inbound mechanism, but no supplied document states that the resolved tenant identifier is what this feature receives. If it is not, tenant scoping cannot be applied and every management operation must fail closed rather than answer with another tenant's data.
- Assumption: the platform tenant-resolver supplies the calling tenant's ancestor chain to the hierarchy walk. DECOMPOSITION §2.3 states that the tenant tree comes from the platform tenant-resolver and assigns the walk to `cpt-cf-oagw-feature-hierarchical-config`; this feature relies on that premise for the effective `enabled` state but performs no walk of its own. If the chain is unavailable, the disable propagation of `cpt-cf-oagw-fr-enable-disable` cannot be observed from a descendant tenant.
- Assumption: the `oagw` gear receives a database handle. The persisted model below cannot exist without one, and the graded configuration declares no `database:` section under `gears.oagw` in `config/e2e-local.yaml`. If the runtime provisions no handle for a gear that declares none, every management write fails with a storage error and the gear serves no configuration at all.
- Assumption: the brace notation in the permission family `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` denotes four distinct permission identifiers per resource type, matched literally by the platform authorization layer. If the runtime treats the brace form as a single literal string, authorization either always fails or always passes, and the fail direction must be the one that denies the write.

### 1.5 Feature-Local Deviations from Shared Baselines

| Deviation | Rationale | Review owner | Validation performed |
|-----------|-----------|--------------|----------------------|
| Routes are registered gear-relative at `/oagw/v1/...` with no `/api` prefix. | DECOMPOSITION §1.3(1) corrects the `/api/oagw/v1/...` tabulation in PRD §7.1 and DESIGN §3.3: `/api` is an operator gateway prefix, not a path this gear serves. Every path in this document is the gear-relative form, and the ten paths are the restatement of the upstream contract, not a new one. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| A route-level `cors` object is validated with the same shape as the upstream CORS configuration, although `schemas/route.v1.schema.json` declares no route-level `cors` property. | DECOMPOSITION §1.3(5): route-level CORS is configured through the `cors` field exactly as the Route class in DESIGN §3.1 specifies, and the shipped schema is a frozen input this run does not edit. The shape applied is the `definitions.cors` object both schemas already carry. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Write-time endpoint `scheme` validation accepts the `http` literal when `oagw.config.allow_http_upstream` is `true`, overriding the `scheme` enum of the frozen upstream schema. | DECOMPOSITION §1.3(2): which literals a configured endpoint may carry and whether a plaintext connection is opened are two questions, and only the second is governed by the flag. `cpt-cf-oagw-constraint-https-only` describes the default posture the flag lifts. The write-time check performed here and the dial-time decision in `cpt-cf-oagw-feature-data-plane-proxy` stay two separate checks against the same constraint. The graded configuration sets the flag to `true`. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| `oagw_route_grpc_match` and the gRPC protocol value are created and validated but consume nothing. | DECOMPOSITION §1.3(4) and DESIGN §3.1 defer gRPC proxying to Phase 3, and DECOMPOSITION §2.2 keeps the table and the protocol value in this feature's write path so the schema's `oneOf` stays decidable. No gRPC match is ever read by this run; a route whose `match` declares only `grpc` is stored and is unreachable at proxy time. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Route `priority` is a required field on the route DTO, and route `enabled` is accepted and persisted, although `schemas/route.v1.schema.json` declares neither. | DESIGN §3.1 declares `priority` and `enabled` as attributes of the Route class, and both the match-uniqueness invariant (DESIGN §3.6) and the enable/disable semantics of `cpt-cf-oagw-fr-enable-disable` are stated over them, so neither can be dropped. Requiring `priority` is the reading that keeps the `(path, priority, method)` uniqueness predicate decidable; a missing priority would make two otherwise-distinct routes collide on an undefined key. `enabled` keeps the `true` default that PRD §5.1 states for both resource types. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The alias is immutable across updates, including the DESIGN §3.3 clause that recomputes the alias when hostname endpoints change; a replacement whose endpoints would derive a different alias answers 409. | DECOMPOSITION §2.2 states alias immutability across updates and §1.3 makes the decomposition prevail over DESIGN where they conflict. Immutability also keeps the alias-to-upstream mapping stable for the cached resolutions ADR 0006 describes. The 409 status is this feature's resolution of the conflict: PRD §8 names 409 Conflict for an alias conflict, and an endpoint change that would move the derived alias is an attempt to change that identity rather than a validation failure. An endpoint change that derives the same alias is accepted, so pooling changes remain possible. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| A route whose `upstream_id` names an upstream the calling tenant does not own answers 400, not 404. | PRD §8 lists "Upstream not found: Return 400 ValidationError" for the Configure Route use case, and DESIGN §3.3 requires `upstream_id` to belong to the calling tenant because ancestor upstreams are not directly addressable. The 404 rule in DESIGN §3.3 Tenant Scoping governs path-addressed resources, so the two rules govern different cases and do not collide: 404 for a resource named in the path, 400 for a resource named in the body. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| An omitted `enabled` on a replacement leaves the stored flag unchanged instead of restoring the schema default `true`. | DESIGN §3.3 states that full replacement overwrites all fields, and the upstream schema gives `enabled` a default of `true`, so the literal reading silently re-enables a disabled upstream during an unrelated configuration edit. PRD §5.1 gives `cpt-cf-oagw-fr-enable-disable` the purpose of temporary maintenance and emergency circuit breaking, which makes silent re-enable the unsafe direction. The flag is therefore carried forward when the body omits it, and a body that states it explicitly still controls it. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The `plugins` sub-object of an upstream or route is validated here but no binding row is written by this feature. | The `plugins` field is part of both DTO shapes and both schemas, so it must be validated; the ordered binding rows live in `oagw_upstream_plugin` and `oagw_route_plugin`, which DECOMPOSITION §1.6 and §2.4 assign to `cpt-cf-oagw-feature-plugin-system` together with the plugin tables. Until that feature lands, the validated value is carried on the domain object and reaches no table this feature owns. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Match-rule uniqueness is evaluated against the enabled routes of the upstream, not against all of them. | DESIGN §3.6 states the invariant as "no two enabled routes under same upstream may share `(path_prefix, priority)` for same method", while DESIGN §3.3 phrases the same predicate as "same path + priority + method". Both sources describe one predicate; this feature implements the DESIGN §3.6 form because it is the persisted invariant, so two disabled routes with identical keys are stored without a conflict and a disable never has to be undone to store a duplicate. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The route replacement DTO is the create DTO minus `upstream_id`: a replacement is validated against `schemas/route.v1.schema.json` with `upstream_id` removed from the schema's `required` set. | DESIGN §3.3 states that a route's `upstream_id` is immutable and not present in the update DTO, while the shipped route schema lists `upstream_id` in `required` with no separate update schema. A replacement that omitted `upstream_id` would otherwise fail the required-property check before the immutability rule could be applied, so the required set is narrowed for the replacement method only, and the stored `upstream_id` is taken from the addressed row. A body that nevertheless carries `upstream_id` is still rejected 400 by the immutable-field rule below. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| Two management-conflict `DomainError` variants, `AliasConflict` and `MatchConflict`, are provisioned by `cpt-cf-oagw-feature-gear-foundation` and consumed here: `AliasConflict` answers 409 with `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1` and `MatchConflict` answers 409 with `gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`. | DESIGN §3.3 tabulates exactly one 409 row, `PluginInUse`, which is a plugin-lifecycle answer, so the 409s this feature answers for an alias conflict and for a match conflict have no catalogue row to map to. The catalogue is the foundation's to own, so the extension is recorded there and in DECOMPOSITION §1.3(9); this feature names the variant it returns in its alias and match-uniqueness answers instead of reusing the plugin variant or answering with an unplumbed type. Both are non-retriable. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| DESIGN §3.3's `created_at desc` ordering example is not orderable on either list endpoint, because the persisted model this feature declares carries no timestamp column. | The persisted model is the table set DECOMPOSITION §1.6 assigns to this feature, and no table in it carries a creation timestamp; `created_at` appears in DESIGN §3.3 only as an `$orderby` example. The example is replaced with an orderable field (`alias` for an upstream, `priority` for a route) rather than adding a column no supplied document declares. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The permission family this feature enforces is narrowed to the `upstream` and `route` arms; the `*_plugin` arms of the same family are enforced by `cpt-cf-oagw-feature-plugin-system`. | DECOMPOSITION §2.2 writes the family as `gts.cf.core.oagw.{upstream,route,*_plugin}.v1~:{create;override;read;delete}`, and DESIGN §3.2 grants the plugin arms `{create;read;delete}` only — no `override` arm exists for a plugin, whose immutability after creation is a PRD §5.3 declaration. This feature registers no plugin route (see `cpt-cf-oagw-dod-management-routes`), so it enforces the `upstream` and `route` arms on the ten paths it registers and leaves the plugin arms to the feature that owns them. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| This feature's tests are colocated at `gears/system/oagw/oagw/tests/` instead of `testing/e2e/gears/oagw/`. | DECOMPOSITION §1.3(3) reserves `testing/e2e/gears/oagw/` for the acceptance suite; every unit and integration test this decomposition produces lives with the crate. This is the same deviation `cpt-cf-oagw-feature-gear-foundation` records in its own §1.5, restated here because the tests it governs include this feature's. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| A successful upstream or route deletion notifies `cpt-cf-oagw-flow-rate-limit-cleanup` of `cpt-cf-oagw-feature-rate-limiting` in process and before the delete's response is produced, which is the same in-process post-write ordering the Control Plane cache flush already applies. | ADR 0003's prefix-based cleanup of a deleted resource is a rate-limit-registry act, and DECOMPOSITION §2.6 assigns that cleanup to `cpt-cf-oagw-feature-rate-limiting` while assigning the deletions that trigger it to this feature. No supplied document states who notifies whom, so the call-direction seam this feature already records for the Data Plane cache flush of `cpt-cf-oagw-feature-data-plane-proxy` is extended to the third interested owner: the write is this feature's, the notification is issued by this feature, and the cleanup is the notified feature's. A failed deletion notifies nothing, because the database it failed against is unchanged. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |
| The deletion-cascade and single-transaction criteria are exercised against this run's store — the in-memory row-table set DECOMPOSITION §1.6 assigns to this feature — rather than against each of the PostgreSQL, MySQL, and SQLite backends of `cpt-cf-oagw-constraint-multi-sql`, because the decomposition ships no SQL adapter for this gear. Portability across the three backends is carried the way §5 states it: the store holds no backend-specific feature, so the same table set, cascade, and transaction boundary apply on any backend that persists those rows. The cascade into routes, both match tables, the method rows, and both tag tables, the `204 No Content` with no body, and the no-partial-write rule are each exercised in `tests/store_tests.rs`, `tests/service_tests.rs`, and `tests/api_tests.rs`; the no-partial-write rule on a failing write is the store's batch boundary, which mutates a candidate copy of the tables and swaps it in only after the batch and the invariant check both succeed, so a batch that fails anywhere leaves the stored configuration exactly as it was. | The controller of this run. | Accepted by the semantic review of this run; `cfs validate --artifact` clean. |

### 1.6 Explicit Non-Applicability

The areas below apply to the gear as a whole but not to this feature. Each is stated here so the omission is a recorded decision rather than a silent gap.

- **Events**: no event is published or consumed here. A management write changes persisted state and flushes the Control Plane cache (ADR 0006); the audit log and the configuration-change reporting that describe those writes belong to `cpt-cf-oagw-feature-observability`, which DECOMPOSITION §3 makes dependent on this feature.
- **Rollout and rollback**: the gear is one configuration item and one release unit (DECOMPOSITION §1.4), so this feature ships no rollout of its own and no independent rollback path. A partially applied multi-table write is impossible by construction — every write below is a single transaction.
- **Versioning**: every GTS identifier this feature reads or writes is fixed at `.v1` by DESIGN §3.1, and the breaking-change policy of `cpt-cf-oagw-interface-management-api` (major version bump from v1 to v2) is a PRD-level declaration about the interface, not a mechanism this feature implements. No version negotiation, aliasing, or migration surface exists here.
- **Localization and accessibility**: the `title` and `detail` of every problem body this feature answers are English protocol strings produced by the foundation's error mapping, and there is no locale negotiation, no translated surface, and no actor-facing rendered UI to make accessible.
- **Plugin persistence**: the plugin and plugin-binding tables are deliberately not created, read, or written by this feature. The boundary is recorded as a §1.5 row rather than left to inference, because the DTO shapes carry a `plugins` field whose persistence belongs elsewhere.
- **Outbound connectivity**: no management operation opens a socket, resolves a credential, or contacts an upstream service. The SSRF policy carried in `OagwConfig` is evaluated by `cpt-cf-oagw-feature-data-plane-proxy` at dial time; this feature performs the write-time scheme check only.
- **Credential material and problem detail**: the only credential-bearing field this feature persists is the opaque `auth.secret_ref` reference, which is validated for `cred://` shape and never resolved here. `auth.config` is an unconstrained object per the shipped schema: this feature validates that it is an object, persists it verbatim, returns it verbatim in the 201 and 200 representations, and never writes it to a log or to a problem `detail`. No problem `detail` this feature answers ever echoes request body content — a `detail` carries the failing property names, the addressed resource, or the colliding identifier, and nothing copied from the body. Enforcement of `cpt-cf-oagw-nfr-credential-isolation` for what may legitimately appear inside `auth.config` belongs to `cpt-cf-oagw-feature-plugin-system`, which resolves the reference at proxy time and owns the auth plugin contract; this feature's obligation is the reference-only rule above, so it neither inspects nor polices the content of that object.
- **Compliance and privacy**: no persisted configuration family this feature owns carries personal data or regulated data — the families are endpoint sets, protocol and sharing values, match keys, rate limits, CORS, tags, and header rules, and the only credential-bearing field among them is the opaque reference described above. No retention policy is defined here: what is stored persists until the owning tenant deletes it, and retention of the configuration store is a platform obligation this feature inherits rather than sets.
- **Performance**: the only bounded path this feature owns is the list page, which `$top` caps at 100 results whatever the caller asks for; the read-assembly guidance that keeps a page from costing one query per parent row is given in §3 under `cpt-cf-oagw-algo-odata-list`. No latency or throughput target is set on the write path or on a single read here, because the proxy-path latency targets of `cpt-cf-oagw-nfr-low-latency` belong to `cpt-cf-oagw-feature-data-plane-proxy`, which owns the request hot path.
- **Observability and health**: this feature logs its failure outcomes — a rejected write (validation, authorization, or conflict, with the failing property names and the colliding identifier), and a storage failure, with the correlation identifier — and it logs no request body, no configuration value, and no credential material. It emits no audit record of a successful configuration change, no metric, and no readiness signal: the audit and metrics surface belongs to `cpt-cf-oagw-feature-observability`, and gear readiness is reported by `cpt-cf-oagw-feature-gear-foundation`, which owns the provisioning state machine.

## 2. Actor Flows (CDSL)

Every flow below follows the management operation order DESIGN §3.5 states — authenticate, validate the DTO, write, respond — and the path-based routing of `cpt-cf-oagw-adr-request-routing`, which sends `/oagw/v1/upstreams/*` and `/oagw/v1/routes/*` to the Control Plane. Path parameters carry anonymous GTS identifiers, and the ten paths are the gear-relative restatement of the DESIGN §3.3 contract.

**Use cases**: `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`

### Configure an Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-create`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:

- A hostname upstream with a single endpoint on its standard port is created; the alias is derived from the host, the row is persisted with a server-generated identifier, and the response carries the anonymous GTS identifier and the normalized alias.
- A caller-supplied alias that equals the derived value after normalization is accepted as an idempotent no-op rather than rejected.
- An endpoint set whose hosts share a suffix of at least two labels derives that suffix as the alias.
- An IP-based or otherwise non-derivable endpoint set with an explicit alias is created.
- An endpoint that omits `port` is stored with the schema default of 443; sub-configuration defaults declared by the shipped schema are applied where the body omits them.
- The Control Plane cache is flushed after the successful creation, before the response is produced (ADR 0006).

**Error Scenarios**:

- The body fails schema validation — missing `server` or `protocol`, an unknown property, an endpoint without `scheme` or `host`, a `port` outside 1 to 65535, a `protocol` outside the enum, a sharing value outside `private`/`inherit`/`enforce`, a `rate_limit` without `sustained`, or a `cors` object without `enabled` — and is answered 400 with `cpt-cf-oagw-fr-error-codes`' validation type.
- The endpoint set is not derivable — IP-only hosts, no common suffix of at least two labels, or a bare public suffix — and no explicit alias was supplied: 400.
- The caller supplied an alias that differs from the derived value: 400.
- Another upstream of the same tenant already holds the normalized alias: 409 with the `AliasConflict` variant.
- The bearer token is missing or invalid: 401; it lacks `gts.cf.core.oagw.upstream.v1~:create`: 403.

**Steps**:
1. [x] - `p1` - Actor issues the create request carrying the upstream DTO: the `server` endpoint set, the `protocol`, and any of `alias`, `tags`, `auth`, `headers`, `rate_limit`, `cors`, `plugins`, `enabled` - `inst-us-create-issue`
2. [x] - `p1` - API: POST /oagw/v1/upstreams — the platform middleware authenticates the bearer token and resolves the calling tenant, and the handler enforces `gts.cf.core.oagw.upstream.v1~:create` before any validation runs - `inst-us-create-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-request-validate` validates the body against `schemas/upstream.v1.schema.json` plus the §1.5 overrides, including the `auth` sub-configuration whose credential reference is checked for shape only - `inst-us-create-validate`
4. [x] - `p1` - `cpt-cf-oagw-algo-alias-derive` derives the alias from the endpoint set, or validates the caller-supplied one, and normalizes it - `inst-us-create-alias`
5. [x] - `p1` - **IF** the endpoint set is non-derivable and the caller supplied no alias - `inst-us-create-alias-if`
   1. [x] - `p1` - **RETURN** 400 naming the endpoint set as non-derivable; no row is written - `inst-us-create-alias-return`
6. [x] - `p1` - **ELSE** - `inst-us-create-alias-else`
   1. [x] - `p1` - Continue with the derived or caller-supplied alias in normalized form - `inst-us-create-alias-continue`
7. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` resolves the calling tenant and checks `(tenant_id, alias)` uniqueness - `inst-us-create-scope`
8. [x] - `p1` - **IF** another upstream of the same tenant already holds the normalized alias - `inst-us-create-conflict-if`
   1. [x] - `p1` - **RETURN** 409 with the `AliasConflict` variant (`gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`); an alias held by a different tenant is not a conflict at this layer, and the ancestor-bind decision for one that is belongs to `cpt-cf-oagw-feature-hierarchical-config` - `inst-us-create-conflict-return`
9. [x] - `p1` - **ELSE** - `inst-us-create-else`
   1. [x] - `p1` - DB: INSERT into `oagw_upstream` (server-generated `id`, `tenant_id`, `alias`, `protocol`, `enabled`, and the configuration families as document columns) and into `oagw_upstream_tag` for each tag, in one transaction, then flush the Control Plane cache before the response is produced; no plugin or plugin-binding row is written (§1.5) - `inst-us-create-insert`
10. [x] - `p1` - **RETURN** 201 with the created representation, the `id` as `gts.cf.core.oagw.upstream.v1~{uuid}`, and the normalized `alias` - `inst-us-create-return`

### Configure a Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-create`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:

- A route whose `upstream_id` names an upstream of the calling tenant and whose `match` declares exactly one of `http` or `grpc` is created with the supplied `priority`, and the route row together with its match, method, and tag rows lands in one transaction.
- A route whose match keys do not collide with any other enabled route of the same upstream is created.
- A route created for a disabled upstream is stored; storing it does not change the upstream's state.
- The Control Plane cache is flushed after the successful creation, before the response is produced (ADR 0006).

**Error Scenarios**:

- The `upstream_id` does not name an upstream owned by the calling tenant — including an ancestor-owned one — and is answered 400 (§1.5).
- The body fails schema validation: no `upstream_id` or `match`, a `match` declaring both or neither of `http` and `grpc`, an `http` match with no `methods` or no `path`, a method outside the enum, a `grpc` match missing `service` or `method`, or a missing `priority` (§1.5).
- The match keys collide with another enabled route of the same upstream: 409 with the `MatchConflict` variant.
- The bearer token is missing or invalid: 401; it lacks `gts.cf.core.oagw.route.v1~:create`: 403.

**Steps**:
1. [x] - `p1` - Actor issues the create request carrying the route DTO: `upstream_id`, `match`, `priority`, and any of `tags`, `rate_limit`, `cors`, `plugins`, `enabled` - `inst-rt-create-issue`
2. [x] - `p1` - API: POST /oagw/v1/routes — the platform middleware authenticates the bearer token and the handler enforces `gts.cf.core.oagw.route.v1~:create` - `inst-rt-create-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-request-validate` validates the body against `schemas/route.v1.schema.json` plus the §1.5 overrides, including the route-level `cors` object validated with the upstream CORS shape - `inst-rt-create-validate`
4. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` resolves the referenced upstream under the calling tenant's scope - `inst-rt-create-resolve`
5. [x] - `p1` - **IF** no upstream with that `upstream_id` is owned by the calling tenant - `inst-rt-create-resolve-if`
   1. [x] - `p1` - **RETURN** 400; an ancestor-owned upstream is not directly addressable as a route target, so a descendant route can only be created under a route target the caller owns - `inst-rt-create-resolve-return`
6. [x] - `p1` - **ELSE** - `inst-rt-create-resolve-else`
   1. [x] - `p1` - Continue with the resolved upstream - `inst-rt-create-resolve-continue`
7. [x] - `p1` - `cpt-cf-oagw-algo-match-uniqueness` checks the incoming `(path, priority, method)` keys against the other enabled routes of that upstream - `inst-rt-create-unique`
8. [x] - `p1` - **IF** a conflict exists - `inst-rt-create-unique-if`
   1. [x] - `p1` - **RETURN** 409 with the `MatchConflict` variant (`gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`), naming the colliding route; no row is written - `inst-rt-create-unique-return`
9. [x] - `p1` - **ELSE** - `inst-rt-create-unique-else`
   1. [x] - `p1` - DB: INSERT into `oagw_route` (server-generated `id`, `tenant_id`, the immutable `upstream_id`, `priority`, `enabled`, and the configuration families) and into `oagw_route_http_match`, `oagw_route_method`, and `oagw_route_tag` as the match keys require, in one transaction, then flush the Control Plane cache before the response is produced - `inst-rt-create-insert`
10. [x] - `p1` - **RETURN** 201 with the created representation and the `id` as `gts.cf.core.oagw.route.v1~{uuid}` - `inst-rt-create-return`

### Read and List Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-config-read-list`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

This flow is the read half of the management surface. It covers the four GET endpoints — `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `GET /oagw/v1/routes`, and `GET /oagw/v1/routes/{id}` — and it writes nothing: no cache is flushed and no row is touched, because a read never changes persisted state.

**Success Scenarios**:

- A single resource addressed by identifier and owned by the calling tenant is returned with its representation and its `id` as the anonymous GTS identifier of the resource.
- A list request with no parameters returns the first page of the calling tenant's rows, bounded by the default `$top` of 50.
- A list request carrying `$filter`, `$select`, `$orderby`, `$top`, and `$skip` returns the tenant-scoped page those parameters select, with `$top` capped at 100 whatever it asks for.

**Error Scenarios**:

- The `id` in the path names a nonexistent resource, or one owned by another tenant including an ancestor: 404, with the two causes indistinguishable.
- A query parameter is malformed, or an expression names a field the resource kind does not expose: 400.
- The bearer token is missing or invalid: 401; it lacks `gts.cf.core.oagw.upstream.v1~:read` or `gts.cf.core.oagw.route.v1~:read`: 403.

**Steps**:
1. [x] - `p1` - Actor issues a `GET` against one of the four read paths, with an `{id}` path parameter for a single read and OData query parameters for a list - `inst-read-issue`
2. [x] - `p1` - API: GET /oagw/v1/upstreams, GET /oagw/v1/upstreams/{id}, GET /oagw/v1/routes, or GET /oagw/v1/routes/{id} — the platform middleware authenticates the bearer token and the handler enforces the `read` permission of the resource kind before any query is built - `inst-read-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` builds the read predicate so it carries the tenant equality alongside every other key, and resolves 404 for a path-addressed resource the caller does not own - `inst-read-scope`
4. [x] - `p1` - **IF** the operation is a list - `inst-read-list-if`
   1. [x] - `p1` - `cpt-cf-oagw-algo-odata-list` parses and bounds the five parameters and assembles the page - `inst-read-list`
5. [x] - `p1` - **ELSE** - `inst-read-single-else`
   1. [x] - `p1` - Read the one row the predicate resolved, together with its dependent tag rows - `inst-read-single`
6. [x] - `p1` - **RETURN** 200 with the representation for a single read and with the bounded page for a list; a path-addressed resource that resolved to no row was already answered 404 in the scoping step - `inst-read-return`

### Replace or Delete an Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-upstream-replace-delete`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:

- A replacement overwrites every configuration family, clears the optional families the body omits, leaves the alias and the identifier untouched, and leaves the stored `enabled` flag untouched when the body omits it (§1.5).
- A replacement that adds an endpoint to the pool while still deriving the same alias is accepted.
- A deletion removes the upstream and, by cascade, its routes, match rows, method rows, and tag rows in one transaction.
- The Control Plane cache is flushed after a successful write, before the response is produced (ADR 0006), and a successful deletion additionally notifies `cpt-cf-oagw-feature-rate-limiting`'s cleanup in the same ordering.

**Error Scenarios**:

- The `id` in the path names a nonexistent upstream, or one owned by another tenant including an ancestor: 404, with the two causes indistinguishable.
- The replacement body fails schema validation, or carries `id` or `tenant_id` values that differ from the addressed row: 400.
- The replacement's endpoints would derive an alias different from the stored one: 409 with the `AliasConflict` variant (§1.5).
- The bearer token is missing or invalid: 401; it lacks `gts.cf.core.oagw.upstream.v1~:override` for the replacement or `gts.cf.core.oagw.upstream.v1~:delete` for the deletion: 403.

**Steps**:
1. [x] - `p1` - Actor issues the operation against `/oagw/v1/upstreams/{id}` with the replacement body for a replacement, or with no body for a deletion - `inst-us-rw-issue`
2. [x] - `p1` - API: PUT /oagw/v1/upstreams/{id} or DELETE /oagw/v1/upstreams/{id} — the platform middleware authenticates the bearer token and the handler enforces `gts.cf.core.oagw.upstream.v1~:override` or `...:delete` respectively - `inst-us-rw-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` resolves the row by identifier and calling tenant - `inst-us-rw-scope`
4. [x] - `p1` - **IF** no row matches, because the identifier does not exist or because it belongs to another tenant including an ancestor - `inst-us-rw-scope-if`
   1. [x] - `p1` - **RETURN** 404; the two causes are deliberately indistinguishable so the endpoint discloses nothing about other tenants' resources - `inst-us-rw-scope-return`
5. [x] - `p1` - **ELSE** - `inst-us-rw-scope-else`
   1. [x] - `p1` - DB: SELECT the stored upstream row and its dependent tag rows as the baseline for the diff - `inst-us-rw-load`
6. [x] - `p1` - **IF** the operation is a replacement - `inst-us-rw-put-if`
   1. [x] - `p1` - `cpt-cf-oagw-algo-request-validate` validates the replacement body against the upstream schema and the §1.5 overrides - `inst-us-rw-validate`
   2. [x] - `p1` - `cpt-cf-oagw-algo-put-replace-diff` recomputes the derived alias from the replacement endpoints, confirms the immutable fields, and builds the write set - `inst-us-rw-diff`
   3. [x] - `p1` - **IF** the recomputed alias differs from the stored alias - `inst-us-rw-alias-if`
      1. [x] - `p1` - **RETURN** 409 with the `AliasConflict` variant (`gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`); the alias is immutable across updates and the stored alias is left unchanged - `inst-us-rw-alias-return`
   4. [x] - `p1` - **ELSE** - `inst-us-rw-alias-else`
      1. [x] - `p1` - DB: UPDATE the `oagw_upstream` row and REPLACE its `oagw_upstream_tag` rows in one transaction, clearing every optional family the body omits, then flush the Control Plane cache - `inst-us-rw-put-write`
7. [x] - `p1` - **ELSE** - `inst-us-rw-delete-else`
   1. [x] - `p1` - DB: DELETE the `oagw_upstream` row by identifier; the foreign keys cascade the deletion into `oagw_route`, both match tables, `oagw_route_method`, and both tag tables within the same transaction, then flush the Control Plane cache and notify `cpt-cf-oagw-flow-rate-limit-cleanup` of `cpt-cf-oagw-feature-rate-limiting` of the successful upstream deletion, in process and before the response is produced - `inst-us-rw-delete-write`
8. [x] - `p1` - **RETURN** the replaced representation for a replacement, and `204 No Content` with no body for a deletion - `inst-us-rw-return`

### Delete a Route

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-route-delete`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Success Scenarios**:

- A route owned by the calling tenant is deleted with `204 No Content` and no body, and the route row and its dependent match, method, and tag rows disappear together in one transaction.
- No other route of the same upstream is disturbed; the deletion removes exactly the addressed route and its dependents.
- Deleting a route never touches the upstream row, so a route deletion cannot disable or remove the upstream it was created under.
- The Control Plane cache is flushed after the successful deletion, before the response is produced (ADR 0006), and the rate-limit registry of `cpt-cf-oagw-feature-rate-limiting` is notified in the same ordering.

**Error Scenarios**:

- The `id` in the path names a nonexistent route, or one owned by another tenant including an ancestor: 404, with the two causes indistinguishable.
- The bearer token is missing or invalid: 401; it lacks `gts.cf.core.oagw.route.v1~:delete`: 403.

**Steps**:
1. [x] - `p1` - Actor issues `DELETE /oagw/v1/routes/{id}` with no body - `inst-rt-del-issue`
2. [x] - `p1` - API: DELETE /oagw/v1/routes/{id} — the platform middleware authenticates the bearer token and the handler enforces `gts.cf.core.oagw.route.v1~:delete` before any query is built - `inst-rt-del-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` resolves the route row by identifier and calling tenant - `inst-rt-del-scope`
4. [x] - `p1` - **IF** no row matches, because the identifier does not exist or because it belongs to another tenant including an ancestor - `inst-rt-del-scope-if`
   1. [x] - `p1` - **RETURN** 404; the two causes are deliberately indistinguishable, so the endpoint discloses nothing about other tenants' routes - `inst-rt-del-scope-return`
5. [x] - `p1` - **ELSE** - `inst-rt-del-scope-else`
   1. [x] - `p1` - DB: DELETE the `oagw_route` row by identifier; the foreign keys cascade the deletion into `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, and `oagw_route_tag` within the same transaction, so the route and everything derived from its match disappear together or not at all, then flush the Control Plane cache and notify `cpt-cf-oagw-flow-rate-limit-cleanup` of `cpt-cf-oagw-feature-rate-limiting` of the successful route deletion, in process and before the response is produced - `inst-rt-del-write`
6. [x] - `p1` - **RETURN** `204 No Content` with no body; a deletion has no representation to return - `inst-rt-del-return`

### Enable or Disable Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-enable-disable`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:

- The owner sets `enabled` to `false` on its own upstream or route; the resource enters `Disabled` for its owner and, without any further write, for every descendant tenant.
- The owner sets `enabled` back to `true`; the resource returns to `Enabled` when no contributing ancestor row is disabled.
- A disabled upstream keeps its routes; disabling removes no row.
- A replacement that omits `enabled` does not disturb the stored flag (§1.5).

**Error Scenarios**:

- The `id` names a nonexistent resource or one owned by another tenant including an ancestor: 404, so no descendant can ever address an ancestor resource to re-enable it.
- The replacement body fails validation: 400.
- The bearer token is missing or invalid: 401; it lacks the `override` permission of the resource type: 403.

**Steps**:
1. [x] - `p1` - Actor issues a replacement carrying an explicit `enabled` value; the ten management paths of DECOMPOSITION §2.2 contain no dedicated enable or disable operation, so the flag is set through PUT - `inst-en-dis-issue`
2. [x] - `p1` - API: PUT /oagw/v1/upstreams/{id} or PUT /oagw/v1/routes/{id} — the platform middleware authenticates the bearer token and the handler enforces `gts.cf.core.oagw.upstream.v1~:override` or `gts.cf.core.oagw.route.v1~:override` - `inst-en-dis-authz`
3. [x] - `p1` - `cpt-cf-oagw-algo-tenant-scope` resolves the row by identifier and calling tenant - `inst-en-dis-scope`
4. [x] - `p1` - **IF** no row matches - `inst-en-dis-scope-if`
   1. [x] - `p1` - **RETURN** 404; this is the only reason a descendant cannot re-enable an ancestor-disabled resource through this API - `inst-en-dis-scope-return`
5. [x] - `p1` - **ELSE** - `inst-en-dis-scope-else`
   1. [x] - `p1` - `cpt-cf-oagw-algo-request-validate` validates the replacement, and the stored `enabled` flag is carried forward when the body omits it (§1.5) - `inst-en-dis-validate`
   2. [x] - `p1` - DB: UPDATE the `enabled` column of the owned row in the same transaction as the rest of the replacement, then flush the Control Plane cache - `inst-en-dis-write`
   3. [x] - `p1` - **IF** the written flag is `false` - `inst-en-dis-off-if`
      1. [x] - `p1` - The resource enters `Disabled` for its owner and for every descendant tenant without any further write; the proxy path answers 503 for a disabled upstream, which is `cpt-cf-oagw-feature-data-plane-proxy`'s obligation - `inst-en-dis-off`
   4. [x] - `p1` - **ELSE** - `inst-en-dis-on-else`
      1. [x] - `p1` - The resource returns to `Enabled` only where no contributing ancestor row is disabled; where one is, it stays effectively `Disabled`, which is the no-descendant-re-enable guard of `cpt-cf-oagw-state-config-lifecycle` - `inst-en-dis-on`
6. [x] - `p1` - **RETURN** the updated representation - `inst-en-dis-return`

## 3. Processes / Business Logic (CDSL)

The routines below are called by the flows in §2, and one of them is additionally called by another routine here: `cpt-cf-oagw-algo-put-replace-diff` re-runs `cpt-cf-oagw-algo-match-uniqueness` during a route replacement, so that routine is reached both from `cpt-cf-oagw-flow-route-create` and from the replacement diff. The routines touch the database only through the repository layer and answer every failure of their own with a `DomainError` from the foundation catalogue, so no flow builds a problem body of its own.

**Storage failure is not a `DomainError` variant.** A persistence-layer failure — no usable database handle, a failed statement, a transaction that cannot commit — has no row in the foundation catalogue, and inventing one would put a platform failure inside the gear's error contract. It is answered with the platform's RFC 9457 500 problem shape carrying `X-OAGW-Error-Source: gateway`, it is logged with the correlation identifier, and it fails the request without partial writes. The single-transaction rule of every multi-table write below is what makes that last claim true: a transaction that does not commit leaves no row behind, so a failed write leaves the stored configuration exactly as it was.

### Management Write Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-request-validate`

**Input**: the HTTP method, the resource kind (`upstream` or `route`), the request body, and `OagwConfig.allow_http_upstream`.

**Output**: a validated domain write, or a `DomainError::ValidationError` carrying every failing property.

The families checked, and the source of each check:

| Family | Check | Source |
|--------|-------|--------|
| Upstream required properties | `server` and `protocol` present | `schemas/upstream.v1.schema.json` `required` |
| Route required properties | on a create, `upstream_id` and `match` present; on a replacement, `match` present and `upstream_id` removed from the required set, because the replacement DTO is the create DTO minus `upstream_id` (§1.5) | `schemas/route.v1.schema.json` `required`, narrowed for the replacement method per §1.5 |
| Unknown properties | rejected at the root and in the `server`, `endpoints` item, `headers`, `rate_limit`, `cors`, and `match` objects | `additionalProperties: false` holds in the upstream schema at the root and in its sub-objects, and in the route schema's sub-objects (`match`, `http_match`, `grpc_match`, `rate_limit`, `cors`); the route schema's root object is open, and the root-level extras a route accepts are exactly the §1.5-added `priority` and `enabled`. The `auth` and `plugins` objects of the upstream schema and the `plugins` object of the route schema leave their own bodies open, which is why `auth.config` can be persisted verbatim |
| Endpoint shape | at least one endpoint; each carries `scheme` and `host`; `host` is a hostname, IPv4, or IPv6; `port` is an integer from 1 to 65535 with the schema default 443 | upstream schema `server.endpoints` |
| Endpoint-pool homogeneity | all endpoints in `server.endpoints` carry the same `protocol`, the same `scheme`, and the same `port`; a pool that mixes two `scheme` values or two `port` values is rejected | PRD §5.5 (`cpt-cf-oagw-fr-alias-resolution`) |
| Scheme admission | `https`, `wss`, `wt`, `grpc`, plus `http` exactly when `allow_http_upstream` is `true` | upstream schema `scheme` enum, overridden per DECOMPOSITION §1.3(2) |
| Protocol enum | one of the two GTS protocol identifiers for HTTP and gRPC | upstream schema `protocol.enum` |
| Sharing enums | `private`, `inherit`, or `enforce` on `auth.sharing`, `plugins.sharing`, `rate_limit.sharing`, and `cors.sharing` | both schemas |
| Match shape | exactly one of `http` or `grpc`; `http` requires `methods` with at least one of GET, POST, PUT, DELETE, PATCH and a non-empty `path`, with optional `query_allowlist` and `path_suffix_mode` of `disabled` or `append`; `grpc` requires non-empty `service` and `method` | route schema `match`, `http_match`, `grpc_match` |
| `rate_limit` sub-object | `sustained` required with `rate` of at least 1; `window` one of second, minute, hour, day; `burst.capacity` at least 1; `algorithm` token bucket or sliding window; `scope` one of the five values; `strategy` reject, queue, or degrade; `cost` at least 1 | `definitions.rate_limit` in both schemas |
| `cors` shape | `enabled` required; `allowed_origins` entries are `*` or a URI; `allowed_methods` from the seven-value enum; `allow_credentials: true` forbids `*` in `allowed_origins` | upstream schema `definitions.cors`, applied to the route-level object per §1.5 |
| Tags | every entry matches the schema's tag pattern | both schemas |
| Credential reference | shape only; never resolved | PRD §8 Configure Upstream; §1.3 |

**Steps**:
1. [x] - `p1` - Parse the body; a body that is not valid JSON for the resource kind fails here - `inst-val-parse`
2. [x] - `p1` - Reject every property the schema does not declare for the resource kind, with the one exception the §1.5 record of the open route root makes: a route body may carry the §1.5-added `priority` and `enabled` at its root, because the route schema's root object declares no `additionalProperties: false` - `inst-val-unknown`
3. [x] - `p1` - Check the required properties of the resource kind, branching on create versus replacement: a create requires the full set the resource kind's schema declares (`server` and `protocol` for an upstream, `upstream_id` and `match` for a route), while a replacement is checked against the same schema with `upstream_id` removed from the route's `required` set, because the replacement DTO is the create DTO minus `upstream_id` (§1.5) - `inst-val-required`
4. [x] - `p1` - **FOR EACH** endpoint in the `server.endpoints` array - `inst-val-endpoint-loop`
   1. [x] - `p1` - Check `scheme`, `host`, and `port` against the endpoint shape row above - `inst-val-endpoint`
5. [x] - `p1` - **IF** the `server.endpoints` array holds more than one endpoint and the endpoints do not share one `scheme` or one `port` - `inst-val-pool-if`
   1. [x] - `p1` - Reject the pool with a validation error naming the diverging property; a pool is homogeneous by PRD §5.5, so a mixed-`scheme` pool and a mixed-`port` pool are both rejected and neither is ever stored - `inst-val-pool`
6. [x] - `p1` - **IF** any endpoint carries the `http` literal - `inst-val-http-if`
   1. [x] - `p1` - Accept it exactly when `allow_http_upstream` is `true`, and reject it otherwise; the flag is the only input to this decision (§1.5) - `inst-val-http`
7. [x] - `p1` - Check the protocol enum, the sharing enums, and the tag pattern - `inst-val-enums`
8. [x] - `p1` - **IF** the resource kind is `route` - `inst-val-route-if`
   1. [x] - `p1` - Check that `match` declares exactly one of `http` or `grpc`, then check the declared branch against the match shape row above, and check that `priority` is present (§1.5) - `inst-val-match`
9. [x] - `p1` - Check the `rate_limit` sub-object and the `cors` object, the latter with the upstream CORS shape for both resource kinds (§1.5) - `inst-val-subs`
10. [x] - `p1` - **IF** any check failed - `inst-val-fail-if`
   1. [x] - `p1` - **RETURN** one validation error naming every failing property, so a caller is not made to retry once per defect - `inst-val-fail-return`
11. [x] - `p1` - **RETURN** the validated domain write - `inst-val-return`

### Alias Derivation and Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-alias-derive`

**Input**: the validated endpoint set, an optional caller-supplied alias, and whether the operation is a create or a replacement.

**Output**: the normalized alias to store, or a validation error.

**Precondition**: the endpoint set is homogeneous. `cpt-cf-oagw-algo-request-validate` has already established that every endpoint in `server.endpoints` carries the same `protocol`, the same `scheme`, and the same `port` (PRD §5.5, `cpt-cf-oagw-fr-alias-resolution`), so this routine never sees a pool it would have to describe with two ports. The precondition is what makes `suffix:port` derivation well-defined: one shared port yields exactly one `hostname:port` or `suffix:port` form for the whole pool, so the derived value is a property of the pool rather than of one endpoint in it, and the comparison against a stored alias during a replacement compares like with like.

**Steps**:
1. [x] - `p1` - Normalize every endpoint host through `cpt-cf-oagw-algo-alias-normalize`, so derivation and later resolution can never disagree about the shape of a value - `inst-alias-derive-normalize`
2. [x] - `p1` - Classify each host as a hostname or an IP address - `inst-alias-derive-classify`
3. [x] - `p1` - **IF** the endpoint set holds a single hostname - `inst-alias-derive-single-if`
   1. [x] - `p1` - Derive the hostname itself on a standard port and `hostname:port` otherwise; the standard ports are 80 for HTTP and 443 for HTTPS, WSS, WT, and gRPC - `inst-alias-derive-single`
4. [x] - `p1` - **IF** the endpoint set holds several hostnames - `inst-alias-derive-multi-if`
   1. [x] - `p1` - Compute the longest common suffix of at least two labels and derive the suffix, or `suffix:port` when the port is non-standard - `inst-alias-derive-suffix`
   2. [x] - `p1` - **IF** the candidate suffix is itself a bare public suffix, such as `co.uk` - `inst-alias-derive-suffix-if`
      1. [x] - `p1` - Treat the set as non-derivable; a bare public suffix is never an alias - `inst-alias-derive-suffix-reject`
5. [x] - `p1` - **IF** any host is an IP address, or the set has no common suffix of at least two labels, or the candidate was rejected as a bare public suffix - `inst-alias-derive-nd-if`
   1. [x] - `p1` - Require an explicit alias; absent one, return a validation error naming the endpoint set as non-derivable - `inst-alias-derive-nd`
6. [x] - `p1` - **IF** the caller supplied an alias - `inst-alias-derive-supplied-if`
   1. [x] - `p1` - **IF** the supplied alias equals the derived value after normalization, accept it as an idempotent no-op - `inst-alias-derive-supplied-eq`
   2. [x] - `p1` - **ELSE** reject it with a validation error; for a derivable endpoint set the alias is not a free name - `inst-alias-derive-supplied-ne`
7. [x] - `p1` - **IF** the operation is a replacement - `inst-alias-derive-put-if`
   1. [x] - `p1` - Compare the derived alias with the stored one and report an `AliasConflict` (409, `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`) when they differ; the alias is immutable across updates (§1.5) - `inst-alias-derive-put`
8. [x] - `p1` - **RETURN** the normalized alias - `inst-alias-derive-return`

Every alias this routine returns satisfies the schema's alias pattern, is ASCII lowercase, and carries no trailing dot; the port suffix participates in identity, so a hostname on its standard port and the same hostname on 8443 are two different aliases.

### Tenant Scoping and Ancestor Non-Addressability

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-tenant-scope`

**Input**: the calling tenant resolved from the SecurityContext, the operation, and the resource selector — a path identifier, a body `upstream_id`, or nothing for a list.

**Output**: the owned row or rows, or a `DomainError` answering 404 or 400.

**Steps**:
1. [x] - `p1` - Read the calling tenant from the SecurityContext; a request without one is never executed against the database - `inst-scope-tenant`
2. [x] - `p1` - Build the read or write predicate so that it carries the tenant equality alongside every other key, through the secure ORM and with no raw SQL (`cpt-cf-oagw-principle-tenant-scope`) - `inst-scope-predicate`
3. [x] - `p1` - **IF** the operation addresses a resource by identifier in the path - `inst-scope-path-if`
   1. [x] - `p1` - Match on the identifier and the calling tenant only; an ancestor row can never satisfy the predicate, so a descendant's read, replacement, or deletion of an ancestor resource returns 404 - `inst-scope-path`
   2. [x] - `p1` - **IF** no row matched - `inst-scope-path-empty-if`
      1. [x] - `p1` - **RETURN** 404, with a nonexistent identifier and a foreign-owned one deliberately indistinguishable - `inst-scope-path-empty`
4. [x] - `p1` - **IF** the operation is a route create or route replacement - `inst-scope-upstream-if`
   1. [x] - `p1` - Resolve the referenced upstream on the identifier and the calling tenant; empty resolution answers 400, because a body reference that is not owned by the caller is a validation failure and not a disclosure about another tenant (§1.5) - `inst-scope-upstream`
5. [x] - `p1` - **IF** the operation is a list - `inst-scope-list-if`
   1. [x] - `p1` - Apply the tenant equality as the outermost predicate of the query, before the OData parameters of `cpt-cf-oagw-algo-odata-list` are applied, so no page can contain another tenant's row - `inst-scope-list`
6. [x] - `p1` - **RETURN** the owned row or rows - `inst-scope-return`

This routine is the only place that decides which rows an operation may see. It never walks the tenant hierarchy: the walk that resolves an ancestor's configuration for a descendant belongs to `cpt-cf-oagw-feature-hierarchical-config`, and the management API's view of an ancestor resource stays empty by construction.

### OData List Parameter Parsing and Bounding

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-odata-list`

**Input**: the raw query string and the resource kind.

**Output**: one bounded, tenant-scoped page, plus the projection, ordering, and filter it was built from, or a validation error.

The five parameters, their types, and their bounds, as DESIGN §3.3 tabulates them for both resource kinds:

| Parameter | Type | Default | Bound |
|-----------|------|---------|-------|
| `$filter` | string | none | An OData filter expression over the fields the resource exposes for filtering; the DESIGN examples are `alias eq 'api.openai.com'` for an upstream and `upstream_id eq '{uuid}'` for a route |
| `$select` | string | none | A comma-separated field list limiting the returned representation |
| `$orderby` | string | none | A field with an optional direction, drawn from the orderable fields of the resource kind in the table below; DESIGN §3.3's `created_at desc` example is not orderable in this run because the persisted model carries no timestamp column (§1.5), so the ordering examples are `alias` for an upstream and `priority` for a route |
| `$top` | integer | 50 | At most 100 results are returned, whatever the parameter asks for |
| `$skip` | integer | 0 | A non-negative offset into the tenant-scoped result set |

The surface each resource kind exposes to those parameters, drawn from the persisted model this feature declares under `cpt-cf-oagw-dod-persisted-model`. `name` appears in neither row because neither resource kind declares one; `path` and `method` for a route live in the route's match and method rows rather than in the route row, and a tag field is satisfied by a parent that holds the tag in its tag table:

| Resource kind | Filter fields | Orderable fields | Selectable properties |
|---|---|---|---|
| `upstream` | `id`, `alias`, `enabled`, and the upstream tags | `id`, `alias`, `enabled` | `id`, `alias`, `protocol`, `enabled`, `server`, `auth`, `headers`, `rate_limit`, `cors`, `plugins`, `tags` |
| `route` | `id`, `upstream_id`, `path`, `method`, `priority`, `enabled`, and the route tags | `id`, `upstream_id`, `priority`, `enabled` | `id`, `upstream_id`, `priority`, `enabled`, `match`, `rate_limit`, `cors`, `plugins`, `tags` |

Every orderable field is single-valued per parent row, so the tag fields and the route's `path` and `method` are filterable but not orderable: a parent can hold several of each, and an ordering over a multi-valued field is not defined.

**Steps**:
1. [x] - `p1` - Parse the five parameter names and ignore their absence; every absent parameter takes the default in the table above - `inst-odata-parse`
2. [x] - `p1` - Validate `$top` and `$skip` as non-negative integers - `inst-odata-paging`
3. [x] - `p1` - **IF** `$top` exceeds 100 - `inst-odata-top-if`
   1. [x] - `p1` - Bound the page to 100 results; the ceiling is a hard bound on the response, so an oversized page can never be served by asking for it - `inst-odata-top-cap`
4. [x] - `p1` - Validate `$filter` and `$orderby` against the fields the resource kind exposes for filtering and ordering, and `$select` against its declared properties - `inst-odata-expressions`
5. [x] - `p1` - **IF** any expression cannot be parsed or names a field the resource kind does not expose - `inst-odata-fail-if`
   1. [x] - `p1` - **RETURN** a validation error naming the offending parameter; a malformed expression is never interpreted as an empty filter - `inst-odata-fail-return`
6. [x] - `p1` - Apply the tenant equality from `cpt-cf-oagw-algo-tenant-scope` first, then the filter, then the ordering, then the offset, and finally the bound - `inst-odata-apply`
7. [x] - `p1` - Assemble the page in one query set rather than one query per parent: the tag rows of the page's parent rows, and for a route page also its match and method rows, are read for the whole page in a single query set, so a page bounded to 100 parents costs a bounded number of queries whatever it holds - `inst-odata-assemble`
8. [x] - `p1` - **RETURN** the page and the projection it was built with - `inst-odata-return`

### Full-Replacement Diff

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-put-replace-diff`

**Input**: the stored row with its dependent rows, the validated replacement, and the resource kind.

**Output**: the write set to apply in one transaction, or a `DomainError` answering 409.

**Steps**:
1. [x] - `p1` - Confirm the immutable fields: `id` and `tenant_id` are taken from the addressed row and never from the body, and a body that states either with a different value is a validation failure - `inst-diff-immutable`
2. [x] - `p1` - **IF** the resource kind is `route` - `inst-diff-route-if`
   1. [x] - `p1` - Take `upstream_id` from the stored row; the update DTO does not carry it, and a body that supplies one is a validation failure rather than a silent override - `inst-diff-route-upstream`
   2. [x] - `p1` - Re-run `cpt-cf-oagw-algo-match-uniqueness` against the other enabled routes of the same upstream, excluding the row being replaced - `inst-diff-route-unique`
3. [x] - `p1` - **IF** the resource kind is `upstream` - `inst-diff-upstream-if`
   1. [x] - `p1` - Recompute the derived alias from the replacement endpoints and compare it with the stored alias; a difference is a conflict (§1.5) - `inst-diff-upstream-alias`
4. [x] - `p1` - Build the write set by overwriting every configuration family with the body's value and clearing the optional families the body omits, with the single exception of `enabled`, which is carried forward when the body omits it (§1.5) - `inst-diff-clear`
5. [x] - `p1` - Compute the tag replacement set as the full set of tags in the body, so a body with fewer tags removes the difference - `inst-diff-tags`
6. [x] - `p1` - **IF** the write set is empty because nothing differs - `inst-diff-empty-if`
   1. [x] - `p1` - Apply nothing and return the stored representation; the write path is not skipped, so the cache flush of ADR 0006 still runs - `inst-diff-empty`
7. [x] - `p1` - **RETURN** the write set - `inst-diff-return`

### Route Match Uniqueness

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-match-uniqueness`

**Input**: the owning upstream identifier, the incoming match keys, the other routes of that upstream with their enabled flags, and whether the incoming route is being created or replaced.

**Output**: confirmation, or a `DomainError` answering 409 with the `MatchConflict` variant (`gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`) and naming the colliding route.

**Steps**:
1. [x] - `p1` - Expand the incoming route into one key per declared method, each carrying the path and the priority; a route that declares three methods contributes three keys - `inst-match-expand`
2. [x] - `p1` - Restrict the comparison set to the enabled routes of the same upstream, excluding the row being replaced (§1.5) - `inst-match-set`
3. [x] - `p1` - **FOR EACH** incoming key - `inst-match-loop`
   1. [x] - `p1` - **IF** an enabled route of the same upstream holds the same path, the same priority, and the same method - `inst-match-collide-if`
      1. [x] - `p1` - **RETURN** 409 with the `MatchConflict` variant, naming the colliding route - `inst-match-collide`
4. [x] - `p1` - **RETURN** confirmation - `inst-match-return`

The predicate is the DESIGN §3.6 invariant — no two enabled routes under the same upstream may share a path and priority for the same method — with the DESIGN §3.3 wording of the same predicate. Two routes that differ in any one of the three components never collide, and two disabled routes with identical keys are stored without a conflict.

## 4. States (CDSL)

### Upstream and Route Effective Lifecycle

- [x] `p2` - **ID**: `cpt-cf-oagw-state-config-lifecycle`

**States**: `Enabled`, `Disabled`

**Initial State**: `Enabled`

The machine is the effective lifecycle of one upstream or route as a calling tenant observes it, not the history of the stored boolean. `Enabled` is the initial state because PRD §5.1 gives `enabled` the default `true` on both resource types and the upstream schema repeats that default. There is no third state: no supplied document defines a created-but-inert condition, and the postcondition of PRD §8's Configure Upstream use case is that the resource is created and available for proxy routing, so a successfully persisted row is immediately meaningful. Deletion is not a state either — it removes the row, the cascade removes its dependents, and nothing of the resource remains to be in a state.

**Transitions**:
1. [x] - `p1` - **FROM** `Enabled` **TO** `Disabled` **WHEN** the owning tenant replaces the resource with `enabled` set to `false` - `inst-state-disable`
2. [x] - `p1` - **FROM** `Enabled` **TO** `Disabled` **WHEN** a contributing ancestor row is disabled — no write reaches this row, the change is observed only from the descendant's side, and the hierarchy walk that detects it belongs to `cpt-cf-oagw-feature-hierarchical-config` - `inst-state-ancestor-disable`
3. [x] - `p1` - **FROM** `Disabled` **TO** `Enabled` **WHEN** the owning tenant replaces the resource with `enabled` set to `true` and no contributing ancestor row is disabled - `inst-state-enable`
4. [x] - `p1` - **FROM** `Disabled` **TO** `Enabled` is refused, and the resource stays `Disabled`, **WHEN** a contributing ancestor row is disabled: this is the no-descendant-re-enable guard of `cpt-cf-oagw-fr-enable-disable`, and the ancestor non-addressability of `cpt-cf-oagw-algo-tenant-scope` is what keeps the guard reachable from this API - `inst-state-ancestor-guard`

A disabled upstream keeps its routes and its configuration. PRD §5.1 makes a disabled upstream answer 503 at proxy time and a disabled route absent from matching, and both of those are evaluated by the features that own the proxy path; this feature's obligation is that the stored flag is authoritative for its owner, that no management write by a descendant can alter an ancestor's row, and that the flag survives a replacement that does not mention it (§1.5). Because the state is effective rather than stored, transition 2 requires no write and produces no audit record of its own; the audit surface for configuration changes belongs to `cpt-cf-oagw-feature-observability`.

## 5. Definitions of Done

### Management Route Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-management-routes`

The system **MUST** register exactly the ten management endpoints of DECOMPOSITION §2.2 — `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}` — on the gear-relative router mount point the foundation created, with `{id}` accepted as the anonymous GTS identifier of the resource (`gts.cf.core.oagw.upstream.v1~{uuid}` or `gts.cf.core.oagw.route.v1~{uuid}`), and **MUST** register no plugin route: the five plugin paths of DESIGN §3.3 belong to `cpt-cf-oagw-feature-plugin-system` (`cpt-cf-oagw-adr-request-routing`).

**Implements**:

- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-config-read-list`
- `cpt-cf-oagw-flow-upstream-replace-delete`
- `cpt-cf-oagw-flow-route-delete`
- `cpt-cf-oagw-flow-enable-disable`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: `POST /oagw/v1/upstreams`, `GET /oagw/v1/upstreams`, `GET /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/upstreams/{id}`, `DELETE /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `GET /oagw/v1/routes`, `GET /oagw/v1/routes/{id}`, `PUT /oagw/v1/routes/{id}`, `DELETE /oagw/v1/routes/{id}`
- DB: none — registration only; the tables are claimed by `cpt-cf-oagw-dod-persisted-model`
- DB Table: none
- Entities: none — the domain types were declared by `cpt-cf-oagw-feature-gear-foundation`

### Authentication and Management Permissions

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-authz-permissions`

The system **MUST** require bearer-token authentication through `toolkit-auth` on every management endpoint and **MUST** enforce `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}` on the upstream endpoints and `gts.cf.core.oagw.route.v1~:{create;override;read;delete}` on the route endpoints, so that a create is answered only with `create`, a replacement and any `enabled` change only with `override`, a deletion only with `delete`, and a read or list only with `read`. A request without a valid token **MUST** be answered 401 and a valid token without the required permission **MUST** be answered 403, in both cases before any validation or database access. The `*_plugin` arm of the permission family named in DECOMPOSITION §2.2 is **NOT** enforced here; `cpt-cf-oagw-feature-plugin-system` owns it.

**Implements**:

- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-upstream-replace-delete`
- `cpt-cf-oagw-flow-enable-disable`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: all ten paths listed under `cpt-cf-oagw-dod-management-routes`
- DB: none — authorization precedes every database access
- DB Table: none
- Entities: none

### Request Validation Against the Shipped Schemas

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-request-validation`

The system **MUST** validate every create and replacement body against `schemas/upstream.v1.schema.json` or `schemas/route.v1.schema.json` before any write, covering every family tabulated under `cpt-cf-oagw-algo-request-validate`, and **MUST** answer a failing body with a single validation error naming every failing property. Validation **MUST** branch on create versus replacement, and it does so in exactly one place: the required-property step. A create **MUST** satisfy the full set the resource kind's schema declares, and a replacement **MUST** satisfy the same set minus `upstream_id` on a route, because the replacement DTO is the create DTO minus `upstream_id`; a replacement body that nevertheless carries `upstream_id` **MUST** still be rejected 400 by the immutable-field rule rather than silently ignored. The §1.5 deviations this DoD enforces **MUST** hold, enumerated by their deviation text so that no count in this DoD can disagree with the §1.5 table: the write-time `scheme` admission that accepts the `http` literal exactly when `allow_http_upstream` is `true`; the route-level `cors` object validated with the upstream CORS shape although the shipped route schema declares no route-level `cors` property; route `priority` required although the shipped route schema declares no `priority`; and the route replacement DTO validated against the schema with `upstream_id` removed from its `required` set. The `auth` sub-configuration's credential reference **MUST** be validated for shape only and **MUST NOT** be resolved.

**Implements**:

- `cpt-cf-oagw-algo-request-validate`
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`

**Constraints**: `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`, `POST /oagw/v1/routes`, `PUT /oagw/v1/routes/{id}`
- DB: none — validation precedes every write
- DB Table: none
- Entities: `Upstream`, `Route`, `Endpoint`, `ServerConfig`, `RateLimitConfig`, `CorsConfig`

### Alias Derivation, Normalization, and Uniqueness

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-alias-derivation`

The system **MUST** derive the alias of every upstream from its endpoint set — hostname on a standard port, `hostname:port` on a non-standard port, and the longest common suffix of at least two labels for several hostnames — **MUST** require an explicit alias for IP-based and otherwise non-derivable endpoint sets, **MUST** treat a bare public suffix as never derivable, **MUST** accept a caller-supplied alias only when it equals the derived value after normalization, **MUST** store every alias ASCII-lowercase with trailing dots stripped and resolve it case-insensitively, **MUST** keep the alias immutable across replacements, and **MUST** enforce `(tenant_id, alias)` uniqueness so that a duplicate within the calling tenant answers 409 and the same alias in another tenant does not. The uniqueness key **MUST** be enforced in the write transaction as well as by the pre-write check, and a unique-constraint violation on `(tenant_id, alias)` surfaced by that transaction **MUST** map to the same 409 `AliasConflict` answer the pre-write check returns, so two concurrent creates of the same alias produce the same conflict rather than a 500.

**Implements**:

- `cpt-cf-oagw-algo-alias-derive`
- `cpt-cf-oagw-algo-put-replace-diff`
- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-upstream-replace-delete`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: `POST /oagw/v1/upstreams`, `PUT /oagw/v1/upstreams/{id}`
- DB: `cpt-cf-oagw-db-schema` — the `(tenant_id, alias)` uniqueness key on the upstream table
- DB Table: `oagw_upstream`
- Entities: `Upstream`, `Endpoint`, `ServerConfig`, `Alias`

### Tenant Scoping on Every Operation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-tenant-scoping`

The system **MUST** scope every management operation to the calling tenant at the data layer, by carrying the tenant equality in the same predicate as every other key through the secure ORM and with no raw SQL, and **MUST** answer a path-addressed resource owned by another tenant — including an ancestor — with 404 that is indistinguishable from a missing resource. A route body referencing an upstream the calling tenant does not own **MUST** be answered 400 per §1.5. No management operation **MUST** read or write a row whose `tenant_id` differs from the caller's, which is the zero-cross-tenant threshold of `cpt-cf-oagw-nfr-multi-tenancy` (`cpt-cf-oagw-principle-tenant-scope`).

**Implements**:

- `cpt-cf-oagw-algo-tenant-scope`
- `cpt-cf-oagw-flow-upstream-replace-delete`
- `cpt-cf-oagw-flow-enable-disable`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: all ten paths listed under `cpt-cf-oagw-dod-management-routes`
- DB: `cpt-cf-oagw-db-schema` — tenant-scoped reads and writes on every table this feature owns
- DB Table: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`

### Persisted Model Shape and Transactional Writes

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-persisted-model`

The system **MUST** persist upstreams and routes in the table set DECOMPOSITION §1.6 assigns to this feature — `oagw_upstream` keyed on `id` and unique on `(tenant_id, alias)`, `oagw_route` keyed on `id` with a cascading foreign key to `upstream_id`, `oagw_route_http_match` keyed on `route_id`, `oagw_route_grpc_match` keyed on `route_id`, `oagw_route_method` keyed on `(route_id, method)`, and `oagw_upstream_tag` with `oagw_route_tag` keyed on `(parent_id, tag)` — and **MUST** apply every multi-table write inside a single transaction so that a failure leaves no partial rows. It **MUST** cascade a deletion in both directions the model defines: an upstream deletion into its routes, match rows, method rows, and tag rows, and a route deletion into its own match, method, and tag rows, the route-side cascade being the one `cpt-cf-oagw-flow-route-delete` relies on. It **MUST** carry the two access paths those reads and checks need, named here in prose rather than declared as DDL: the index that serves the tenant-scoped scan of each table this feature owns, because every read predicate leads with the tenant equality, and the index that serves the `(upstream_id, path, priority, method)` lookup `cpt-cf-oagw-algo-match-uniqueness` performs against the enabled routes of an upstream. It **MUST** stay portable across the PostgreSQL, MySQL, and SQLite backends of `cpt-cf-oagw-constraint-multi-sql` by avoiding backend-specific features, and **MUST NOT** create or write the plugin and plugin-binding tables (§1.5, §1.6). The two `auth_plugin_ref` and `auth_plugin_uuid` columns DESIGN §3.1 declares on the upstream row are the exception to that rule: this feature declares the upstream row that carries them, and `cpt-cf-oagw-feature-plugin-system` writes exactly those two columns inside the single transaction this feature's parent write opens, so that column set has one declaring feature and one writing feature and no third writer.

A persistence-layer failure is **NOT** a `DomainError` catalogue variant. It **MUST** be answered by the platform's RFC 9457 500 problem shape carrying `X-OAGW-Error-Source: gateway`, **MUST** be logged with the correlation identifier, and **MUST** fail the request without partial writes, which the single-transaction rule above is what guarantees; a successful deletion **MUST** be answered `204 No Content` with no body.

**Implements**:

- `cpt-cf-oagw-flow-upstream-create`
- `cpt-cf-oagw-flow-route-create`
- `cpt-cf-oagw-flow-upstream-replace-delete`
- `cpt-cf-oagw-flow-route-delete`
- `cpt-cf-oagw-algo-put-replace-diff`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`, `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: none — the tables are written by the handlers registered under `cpt-cf-oagw-dod-management-routes`
- DB: `cpt-cf-oagw-db-schema` — this feature's share of the shared schema
- DB Table: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`, `Endpoint`, `ServerConfig`, `MatchConfig`, upstream and route tag rows, the `(tenant_id, alias)` uniqueness key

### Full-Replacement Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-full-replacement-put`

The system **MUST** treat PUT as full replacement: every configuration family is overwritten, every optional family the body omits is cleared, and the tag set is replaced in full. `id`, `tenant_id`, and the route's `upstream_id` **MUST** be immutable, and an update DTO that carries a route `upstream_id` **MUST** be rejected rather than silently ignored. The alias **MUST** be immutable across replacements, so a replacement whose endpoints would derive a different alias answers 409 with the `AliasConflict` variant and leaves the stored alias unchanged, while one that derives the same alias is accepted (§1.5). Match-rule uniqueness **MUST** be revalidated against the other enabled routes of the same upstream, excluding the row being replaced, and a unique-constraint violation on the match keys surfaced by the write transaction **MUST** map to the same 409 `MatchConflict` answer the pre-write check returns.

**Implements**:

- `cpt-cf-oagw-algo-put-replace-diff`
- `cpt-cf-oagw-algo-match-uniqueness`
- `cpt-cf-oagw-flow-upstream-replace-delete`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: `PUT /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema` — replacement writes and tag replacement
- DB Table: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`, `MatchConfig`

### Enable and Disable Semantics

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-enable-disable`

The system **MUST** store `enabled` on both resource types with the default `true` that PRD §5.1 states, **MUST** accept a transition only from the owning tenant, **MUST** leave the stored flag unchanged when a replacement body omits it (§1.5), **MUST** keep a disabled upstream's routes in place, **MUST** present the effective state as the conjunction of the resource's own flag with the flags of the contributing ancestor rows so that an ancestor disable reaches every descendant without a write, and **MUST NOT** allow any management operation to raise the effective state of a resource while a contributing ancestor row is disabled, which is the no-descendant-re-enable guard of `cpt-cf-oagw-state-config-lifecycle`. The 503 answer for a disabled upstream and the exclusion of a disabled route from matching are evaluated by the features that own the proxy path; the flag persisted here is the signal they consume (`cpt-cf-oagw-fr-enable-disable`).

**Implements**:

- `cpt-cf-oagw-state-config-lifecycle`
- `cpt-cf-oagw-flow-enable-disable`
- `cpt-cf-oagw-algo-put-replace-diff`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: `PUT /oagw/v1/upstreams/{id}`, `PUT /oagw/v1/routes/{id}`
- DB: `cpt-cf-oagw-db-schema` — the `enabled` column on both resource rows
- DB Table: `oagw_upstream`, `oagw_route`
- Entities: `Upstream`, `Route`

### List and Query Parameters

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-list-query-parameters`

The system **MUST** support `$filter`, `$select`, `$orderby`, `$top`, and `$skip` on both list endpoints, **MUST** default `$top` to 50 and bound every page to 100 results whatever the parameter asks for, **MUST** apply the tenant equality before any of the five parameters so that no page can contain another tenant's row, **MUST** answer a malformed parameter or an expression naming a field the resource kind does not expose with a validation error, and **MUST** leave the response to a list request with no parameters bounded by the default (`cpt-cf-oagw-interface-management-api`).

**Implements**:

- `cpt-cf-oagw-algo-odata-list`
- `cpt-cf-oagw-algo-tenant-scope`
- `cpt-cf-oagw-flow-config-read-list`

**Constraints**: `cpt-cf-oagw-constraint-multi-sql`

**Touches**:

- API: `GET /oagw/v1/upstreams`, `GET /oagw/v1/routes`
- DB: `cpt-cf-oagw-db-schema` — bounded, tenant-scoped list reads
- DB Table: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_method`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: `Upstream`, `Route`

### Colocated Tests

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-colocated-tests`

The system **MUST** deliver this feature's unit and integration tests colocated under `gears/system/oagw/oagw/tests/`, covering schema validation and every override of §1.5, alias derivation by endpoint type including the bare-public-suffix case, tenant scoping with the 404-for-ancestor behaviour, full-replacement clearing and alias immutability, match uniqueness, the enable and disable transitions, and the OData parameters, and **MUST NOT** add any test under `testing/e2e/gears/oagw/` (DECOMPOSITION §1.3(3)). The coverage **MUST** include an endpoint-pool homogeneity case in which a pool whose endpoints declare two different `scheme` values is rejected with a validation error and a case in which a pool whose endpoints declare two different `port` values is rejected with a validation error, and a credential-isolation case asserting that no credential material is resolved or logged — the `auth.secret_ref` reference stays opaque and `auth.config` is never written to a log — and that a problem `detail` carries no echo of the request body.

**Implements**:

- `cpt-cf-oagw-algo-request-validate`
- `cpt-cf-oagw-algo-alias-derive`
- `cpt-cf-oagw-algo-tenant-scope`
- `cpt-cf-oagw-algo-odata-list`
- `cpt-cf-oagw-algo-put-replace-diff`
- `cpt-cf-oagw-algo-match-uniqueness`
- `cpt-cf-oagw-state-config-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: all ten paths listed under `cpt-cf-oagw-dod-management-routes`
- DB: `cpt-cf-oagw-db-schema` — the tables the integration tests exercise
- DB Table: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag`, `oagw_route_tag`
- Entities: none — tests only

## 6. Acceptance Criteria

- [x] Exactly the ten management paths of DECOMPOSITION §2.2 are registered, all gear-relative, a request to the `/api/oagw/v1/upstreams` form is answered by no OAGW handler, and `GET /oagw/v1/upstreams/{id}` resolves a resource addressed as `gts.cf.core.oagw.upstream.v1~{uuid}` while a path parameter that is not the anonymous GTS identifier of one of the caller's resources resolves to no resource.
- [x] A management request without a bearer token is answered 401 and reaches no handler and no database access, a valid token without `gts.cf.core.oagw.upstream.v1~:create` is answered 403 and writes no row, and a token holding only `read` succeeds on `GET /oagw/v1/upstreams` while failing on `POST /oagw/v1/upstreams` and on the `PUT` and `DELETE` of an upstream it owns.
- [x] A create body omitting `server` or `protocol`, carrying an unknown property, holding an endpoint without `scheme` or `host`, or holding a `port` of 0 or 65536 is answered 400 with a validation error naming every failing property; the same holds for a route body whose root carries a property outside the route schema's declared set plus the §1.5-added `priority` and `enabled`, while a route body that states `priority` or `enabled` at the root is accepted because the route schema's root object is open.
- [x] A route body whose `match` declares both `http` and `grpc`, neither of them, an `http` branch with no `methods`, or a `grpc` branch without `service` is answered 400.
- [x] A `rate_limit` without `sustained`, a `sustained` with `rate` below 1, a `window` outside the four-value enum, a `cors` object without `enabled`, or a `cors` object with `allow_credentials` set to `true` and `allowed_origins` containing `*` is answered 400, and the route-level `cors` object is accepted when it satisfies the same shape as the upstream one.
- [x] An upstream endpoint with the `http` scheme is accepted exactly when `allow_http_upstream` is `true` and rejected with 400 when it is `false`, with no other input changing the outcome.
- [x] A single hostname endpoint on 443 derives `api.openai.com`, the same hostname on 8443 derives `api.openai.com:8443`, and the two are distinct aliases.
- [x] Endpoint sets of `us.vendor.com` and `eu.vendor.com` on 443 derive `vendor.com`, and an IP-only endpoint set derives nothing and requires an explicit alias, its absence being answered 400.
- [x] Endpoint sets whose only common suffix is a bare public suffix such as `co.uk` are answered 400 as non-derivable and are stored only with an explicit alias.
- [x] A caller-supplied alias equal to the derived value after normalization is accepted and one that differs is answered 400, and aliases are stored ASCII-lowercase with trailing dots stripped so that an upstream created as `API.OpenAI.com.` is resolved by a lookup for `api.openai.com`.
- [x] A second upstream with the same normalized alias in the same tenant is answered 409, and the same alias created by a different tenant is not.
- [x] `POST /oagw/v1/routes` with an `upstream_id` owned by another tenant, including an ancestor, is answered 400 and writes no row.
- [x] Two routes under the same upstream with the same path, priority, and method are answered 409 on the second, while a route differing in any one of the three is accepted; two disabled routes with identical keys are both stored.
- [x] A `PUT /oagw/v1/routes/{id}` body that omits `upstream_id` is a conforming replacement: it is not rejected for the missing `upstream_id`, it clears every optional family it omits, and the stored `upstream_id` stays unchanged, while a replacement body that nevertheless carries `upstream_id` is answered 400.
- [x] `PUT /oagw/v1/upstreams/{id}` whose endpoints would derive an alias different from the stored one is answered 409, the stored alias is unchanged, and a replacement that adds a pooled endpoint while deriving the same alias succeeds.
- [x] A descendant tenant's `GET`, `PUT`, and `DELETE` of an upstream owned by an ancestor tenant are each answered 404, while the ancestor's own requests for the same identifier succeed.
- [x] A list request with no `$top` returns at most 50 rows and one with `$top` above 100 returns at most 100, `$filter`, `$orderby`, `$select`, and `$skip` narrow, order, project, and offset the tenant-scoped result set respectively, a malformed `$top` or `$skip` or a `$filter` naming a field the resource kind does not expose is answered 400 rather than interpreted as an absent parameter, and an `$orderby` naming `created_at` is answered 400 because the persisted model carries no timestamp column.
- [x] Setting `enabled` to `false` on an upstream leaves its routes present, and restoring it to `true` returns the resource to `Enabled` unless a contributing ancestor row is disabled, in which case it stays effectively `Disabled`; a replacement body that omits `enabled` leaves the stored flag unchanged rather than restoring the default `true`.
- [x] Deleting an upstream removes its routes, both match tables' rows, its method rows, and both tag tables' rows in one transaction on each of the PostgreSQL, MySQL, and SQLite backends, the deletion succeeds with `204 No Content` and no body, and a write that fails partway leaves no partial rows.
- [x] Every test for this feature lives under `gears/system/oagw/oagw/tests/`, passes there, and no test is added under `testing/e2e/gears/oagw/`.
