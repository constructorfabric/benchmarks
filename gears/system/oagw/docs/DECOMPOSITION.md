# Decomposition: OAGW (Outbound API Gateway) gear


<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Decomposition Strategy](#11-decomposition-strategy)
  - [1.2 Dependency Rationale (Summary)](#12-dependency-rationale-summary)
  - [1.3 Task Override Notes (Corrections to the Supplied Documents)](#13-task-override-notes-corrections-to-the-supplied-documents)
  - [1.4 Deployment and State Posture](#14-deployment-and-state-posture)
  - [1.5 Actor Coverage](#15-actor-coverage)
  - [1.6 Coverage Accounting](#16-coverage-accounting)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
  - [2.2 Control Plane Configuration API - HIGH](#22-control-plane-configuration-api---high)
  - [2.3 Hierarchical Configuration - MEDIUM](#23-hierarchical-configuration---medium)
  - [2.4 Plugin System - HIGH](#24-plugin-system---high)
  - [2.5 Data Plane Proxy - HIGH](#25-data-plane-proxy---high)
  - [2.6 Rate Limiting - HIGH](#26-rate-limiting---high)
  - [2.7 CORS - HIGH](#27-cors---high)
  - [2.8 Streaming - HIGH](#28-streaming---high)
  - [2.9 Observability - MEDIUM](#29-observability---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-oagw-gear`
## 1. Overview

This document decomposes the existing OAGW design into implementable features. It changes nothing in `PRD.md`, `DESIGN.md`, the ADRs, or the JSON Schemas; §1.3 prevails over `PRD.md` and `DESIGN.md` wherever they conflict with each other or with the grading configuration, and every such correction is listed there and applied in the feature entries. The FEATURE artifacts that this pipeline authors next live at `gears/system/oagw/docs/features/<slug>.md`, one per entry in Section 2, and the link in each `2.N` heading below points at that path.

The source design is one gear crate with two logical services inside it. The Control Plane owns configuration data (upstreams, routes, plugins) and the Data Plane orchestrates proxy requests to external services. Both live behind one Axum router inside a single executable, layered DDD-Light style as `api/rest`, `domain`, and `infra`.

### 1.1 Decomposition Strategy

The design was split bottom-up along ownership boundaries rather than along API endpoints. Shared vocabulary comes first, then the owner of persisted state, then the request path, and finally the policies that hang off that path.

1. **Foundation first.** Nothing else can be written or tested until the gear registers with ToolKit, the domain types exist, and every failure has one canonical shape.
2. **Configuration before traffic.** Upstreams, routes, and plugins are the inputs the data plane consumes, so the Control Plane lands before any proxy code.
3. **Hierarchy before resolution.** Effective configuration is a hierarchy walk, so the walk is its own feature and is finished before the proxy path calls it.
4. **One proxy spine.** The data plane is a single feature that resolves, merges, transforms, and forwards. Rate limiting, CORS, and streaming attach to that spine instead of being folded into it.
5. **Cross-cutting concerns last.** Observability reads the spine and the policies, so it is built once they are stable.

Each feature is independently implementable and testable once its dependencies exist. Tests are colocated with the crate under `gears/system/oagw/oagw/tests/` (see the override in Section 1.3), so a feature is "done" when its tests pass in that tree and its behaviour is observable through the gear's public surface.

**Priority rule used below.** A feature carries the highest priority of the requirements it primarily delivers. Requirements a feature merely consumes again (for example `cpt-cf-oagw-nfr-input-validation`, which the foundation models and the proxy path enforces) keep their own source priority inside the feature's coverage list. No feature is unprioritized.

### 1.2 Dependency Rationale (Summary)

The chain has one trunk and one fan-out. `gear-foundation` is the root. `control-plane-config` extends it and hands `hierarchical-config` the persisted model to walk. `plugin-system` branches off the root in parallel because it touches no persisted upstream or route state. `data-plane-proxy` is the junction where the two branches meet, the three policy tails (`rate-limiting`, `cors`, `streaming`) all hang off it, and `observability` reads it together with the two features whose state it reports. Section 3 gives the full graph and the per-edge rationale.

The useful parallel seam is between the Control Plane branch (`control-plane-config` then `hierarchical-config`) and `plugin-system`; both only need `gear-foundation`. The second seam is the three parallel tail features — `cors`, `streaming`, and the `observability` slice that only reads the proxy — which can be built concurrently once `data-plane-proxy` exists. The rest of `observability`, the rate-limit and configuration-change reporting, is completed after `rate-limiting` and `control-plane-config` exist, because it reads circuit-breaker state, rate-limit state, and configuration-change events.

### 1.3 Task Override Notes (Corrections to the Supplied Documents)

§1.3 prevails over `PRD.md` and `DESIGN.md` wherever they conflict with each other or with the grading configuration, and every such correction is listed here. The corrections are recorded here and in the feature entries; no supplied document was edited.

1. **Gear-relative routes, no `/api` prefix.** Routes in this configuration are registered gear-relative as `/oagw/v1/...`. `PRD.md` and `DESIGN.md` tabulate `/api/oagw/v1/...`, which is the absolute path behind an operator gateway prefix `/api` and is not what this gear serves. Every `API` bullet in this document uses the gear-relative form, for example `POST /oagw/v1/upstreams` and `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`.
2. **`http` is a legal endpoint scheme in the graded configuration.**
   - **The rule.** Which scheme literals a configured endpoint may carry is one question, and whether a plaintext connection is actually opened is the other. Only the second is governed by `allow_http_upstream`.
   - **The configuration evidence.** `config/e2e-local.yaml` sets `oagw.config.allow_http_upstream: true`, so an upstream may be declared `{"scheme": "http", "port": 80}`. `cpt-cf-oagw-constraint-https-only` in `DESIGN.md` describes the *default* posture, which this flag lifts.
   - **The per-layer decision.** When `allow_http_upstream` is `true`, the write-time validation in `control-plane-config` ACCEPTS the `http` literal, which overrides the `scheme` enum of `schemas/upstream.v1.schema.json` for this deployment. That schema is a frozen input this run does not edit, so the override is recorded here and applied by the implementation, and the outbound dial layer in `data-plane-proxy` may then open the plaintext connection. The write-time scheme check and the outbound dial decision stay two separate checks against the same constraint.
   - **`wt` scope reduction.** A `wt`-scheme upstream validates and is stored per the schema, but no feature carries WebTransport behaviour (see item 4 of this section). `data-plane-proxy` answers a proxy attempt against such an upstream with a gateway error carrying `X-OAGW-Error-Source: gateway`, recorded here as an explicit scope reduction.
3. **Automated tests are colocated with the crate.** They live in `gears/system/oagw/oagw/tests/`. `testing/e2e/gears/oagw/` is reserved for the acceptance suite and receives no unit or integration tests from this decomposition.
4. **gRPC proxying and WebTransport are out of scope.** `DESIGN.md` §3.1 defers gRPC proxying to Phase 3, while `PRD.md` §4.2 places it in phase 4; both exclude it from the current build, and this decomposition follows the DESIGN §3.1 wording ("no gRPC proxy code path is currently implemented or reachable"), so no gRPC match or forwarding work is planned here. For WebTransport the premise is the opposite of a silent gap: `DESIGN.md` specifies no WebTransport transport behaviour, while `PRD.md` §4.1 lists WebTransport in scope, `cpt-cf-oagw-fr-streaming` MUSTs WebSocket and WebTransport session flows, and `schemas/upstream.v1.schema.json` admits the `wt` scheme. This decomposition therefore records an explicit scope reduction of `cpt-cf-oagw-fr-streaming`: the HTTP request/response, SSE, and WebSocket clauses are delivered, and the WebTransport clause is not. Both exclusions are recorded in the affected features rather than left as silent gaps.
5. **Route-level CORS is configured through a `cors` field that the shipped route schema does not declare.** Route-level CORS is configured through the `cors` field exactly as the Route class in `DESIGN.md` §3.1 specifies, and `cpt-cf-oagw-feature-control-plane-config` validates it; the shipped `schemas/route.v1.schema.json` omits the property. That schema is a frozen input this run does not edit, so the divergence is recorded here and resolved by validating a route-level `cors` object with the same shape as the upstream CORS configuration.
6. **Metrics are exposed at `/oagw/v1/metrics`, not `/metrics`.** `DESIGN.md` §4.2 places the Prometheus endpoint at `/metrics` and qualifies it as admin-only. This run exposes it gear-relative at `/oagw/v1/metrics`, consistent with the gear-relative routing in item 1, and drops the admin-only gating qualifier because the graded configuration exposes no admin-gating surface for gear-relative gear routes.
7. **Configuration caching is in scope.** ADR 0005's single-exec branch authorizes the L1 configuration cache with explicit invalidation, overriding `DESIGN.md` §4.1's "future consideration" posture for config caching.
8. **The DESIGN ADR table is stale and is left as-is.** The authoritative ADR set for this run is `gears/system/oagw/docs/ADR/0001`–`0009`; `DESIGN.md` §5.2's table predates ADRs 0008 and 0009 and §1.2 omits `cpt-cf-oagw-adr-required-headers-guard-plugin`. `DESIGN.md` is a frozen input to this run, so its table is not corrected and this note is the baseline of record.
9. **The error catalogue gains two management-conflict 409 variants.** DESIGN §3.3 tabulates exactly one 409 row, `PluginInUse`, which is a plugin-lifecycle answer, so the 409s the management write path answers for an alias conflict and for a route-match conflict have no catalogue variant to map to. This run extends the catalogue that `cpt-cf-oagw-feature-gear-foundation` owns with `AliasConflict` (409, non-retriable, `gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1`) and `MatchConflict` (409, non-retriable, `gts.cf.core.errors.err.v1~cf.oagw.match.conflict.v1`), both spelled on the same `gts.cf.core.errors.err.v1~cf.oagw.{slug}.v1` pattern as the DESIGN rows. The catalogue is therefore 22 variants over 21 distinct error identifiers rather than the 20 rows over 19 identifiers DESIGN §3.3 tabulates, and `cpt-cf-oagw-feature-control-plane-config` consumes both variants in its alias and match-uniqueness answers.
10. **Cache ownership is split along the CP/DP boundary.** ADR 0006 assigns an L1 cache to each of the Control Plane and the Data Plane, and ADR 0005's single-exec branch (item 7) authorizes both. This run assigns the Control Plane L1 configuration cache to `cpt-cf-oagw-feature-control-plane-config`, which builds it and flushes it on every successful write before the response is produced; the Data Plane L1 cache and its post-write invalidation belong to `cpt-cf-oagw-feature-data-plane-proxy` (§2.5, the explicit-invalidation alternative of ADR 0006), which also dispositions the periodic-sync alternative of that ADR.
11. **`enabled` is carried forward by a replacement that omits it.** DESIGN §3.3 states that full replacement overwrites all fields, and the upstream schema gives `enabled` a default of `true`, so the literal reading silently re-enables a disabled upstream during an unrelated configuration edit. A replacement therefore carries the stored `enabled` flag forward when the body omits it, and a body that states the flag explicitly still controls it. The same item records the property set behind it: `priority` and `enabled` are attributes of the Route class in DESIGN §3.1 that the shipped `schemas/route.v1.schema.json` omits, so both are deviations recorded in the feature artifacts rather than properties the schema declares.
12. **Match-rule uniqueness is evaluated over the upstream's enabled routes.** DESIGN §3.6 states the invariant as "no two enabled routes under same upstream may share `(path_prefix, priority)` for same method", while DESIGN §3.3 phrases the same predicate as "same path + priority + method". This run implements the DESIGN §3.6 form because it is the persisted invariant: two disabled routes with identical keys are stored without a conflict, and a disable never has to be undone to store a duplicate.

### 1.4 Deployment and State Posture

The gear deploys as one executable inside the ToolKit monolith (`cpt-cf-oagw-constraint-toolkit-deploy`), and the grading configuration runs it in that single-exec mode. ADR 0005 (Control Plane Caching) and ADR 0006 (State Management) therefore reduce to their single-exec branch: **L1 caches only, no Redis, no L2 layer**.

Concretely, the Data Plane owns a small in-process LRU of resolved upstream and route configurations, a shared outbound HTTP client, and per-instance token buckets. The Control Plane owns its own in-process cache and flushes it on writes. There is no cross-instance state and no distributed rate-limit sync. Rate-limit counters are per-instance by design; a restart accepts a short burst window rather than introducing a Redis dependency.

`config/e2e-local.yaml` confirms this posture. The `oagw:` block carries only `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy.enabled` — there is no cache-backend or sync-backend key to configure.

**Configuration-item posture.** All nine features are work packages within the single `oagw` crate. The gear is one configuration item and one release unit, so the features are baselined together and tracked by feature ID rather than by independent version; a feature is not separately releasable, and its completion is recorded against the feature entry in Section 2.

### 1.5 Actor Coverage

The six actors from `PRD.md` §2 are all served by this decomposition. Each is named here and is exercised by the features listed against it.

| Actor | ID | Served by |
|---|---|---|
| Platform Operator | `cpt-cf-oagw-actor-platform-operator` | `gear-foundation`, `control-plane-config`, `plugin-system`, `observability` |
| Tenant Administrator | `cpt-cf-oagw-actor-tenant-admin` | `control-plane-config`, `hierarchical-config`, `plugin-system` |
| Application Developer | `cpt-cf-oagw-actor-app-developer` | `data-plane-proxy`, `rate-limiting`, `streaming` |
| Credential Store | `cpt-cf-oagw-actor-cred-store` | `plugin-system` |
| Types Registry | `cpt-cf-oagw-actor-types-registry` | `gear-foundation`, `plugin-system` |
| Upstream Service | `cpt-cf-oagw-actor-upstream-service` | `data-plane-proxy`, `streaming` |

The three human actors reach the gear through the management and proxy APIs; the three system actors are reached through in-process SDK calls (`cred_store`, `types_registry`) and through outbound HTTP.

### 1.6 Coverage Accounting

Every identifier named in the traceability inputs is accounted for: the PRD-declared requirements, interfaces, contracts, and use cases each appear at least once under a feature's checkbox lists, and the DESIGN-declared elements each appear at least once in a feature reference list. The counts are: 14 functional requirements, 8 non-functional requirements, 2 PRD public API interfaces plus 2 PRD external integration contracts plus 1 design-scoped API contract section (`cpt-cf-oagw-interface-api`), 5 use cases, 9 ADRs, 10 design structure identifiers, 7 design principles, and 5 design constraints. The six `cpt-cf-oagw-actor-*` identifiers are not repeated under feature checkbox lists; they are mapped to features in Section 1.5, which is their coverage record. `cpt-cf-oagw-seq-proxy-flow` sits under `data-plane-proxy` and is the only identified sequence in `DESIGN.md` §3.5; the §3.5 management operation flow is carried as scope in `control-plane-config` rather than as a separate sequence identifier. `cpt-cf-oagw-db-schema` is shared: `control-plane-config` owns the upstream, route, tag, and match tables, and `plugin-system` owns the plugin and plugin-binding tables.

---

## 2. Entries

### 2.1 [Gear Foundation](features/gear-foundation.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establishes `oagw` as a registered ToolKit gear and lays down the shared vocabulary every later feature compiles against. It delivers the crate skeleton, the `OagwConfig` surface, the domain model types, the GTS identifier catalogue with its types-registry provisioning, and one canonical error shape. Without it, no other feature can be written or tested.

- **Depends On**: None

- **Scope**:
  - ToolKit gear skeleton and registration (`gear.rs`, `lib.rs`) with the Axum router mount point that later features populate.
  - `OagwConfig` surface: `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`, `token_cache_ttl_secs`, `token_cache_capacity`.
  - DDD-Light crate layering (`api/rest`, `domain`, `infra`) with domain code free of infrastructure types.
  - Domain model types for `Upstream`, `Route`, `Plugin`, and their `server`, `auth`, `headers`, `rate_limit`, `cors`, and `plugins` sub-configurations.
  - GTS identifier constants for the upstream, route, protocol, error, and plugin base types, plus `post_init()` type catalog provisioning through the `types_registry` SDK.
  - `DomainError` covering the full error catalogue, tagged with its gateway-versus-upstream source, and mapped to RFC 9457 `application/problem+json` responses carrying GTS `type` identifiers.
  - Colocated tests under `gears/system/oagw/oagw/tests/`.

- **Out of scope**:
  - Any HTTP endpoint, handler, or route registration.
  - Persistence, plugin execution, and outbound forwarding.
  - New requirements or architecture decisions; this feature only materializes the existing design.

- **Phases**: single phase

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - `Upstream`, `Route`, `Plugin` (aggregate shapes from the design domain model)
  - `Endpoint`, `ServerConfig`, `AuthConfig`, `HeadersConfig`, `RateLimitConfig`, `CorsConfig`, `PluginsConfig`
  - Alias and hostname value objects, normalized to ASCII lowercase with trailing dots stripped
  - `Scheme`, `SsrfPolicy`, `OagwGear`, `ErrorSource`, and `ErrorContext` (declared once here as the gear's shared vocabulary; consumed by later features)
  - `DomainError` and the GTS error-type catalogue with gateway/upstream source tags

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-design-drivers`
  - `p1` - `cpt-cf-oagw-design-overview`
  - `p1` - `cpt-cf-oagw-design-domain-model`
  - `p1` - `cpt-cf-oagw-design-dependencies`
  - `p1` - `cpt-cf-oagw-tech-dependencies`

- **API**:
  - None (no public HTTP surface; this feature supplies the router mount point and the `OagwConfig` consumed by later features)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.2 [Control Plane Configuration API](features/control-plane-config.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-control-plane-config`

- **Purpose**: Implements the management half of the gear. It owns create, read, update, and delete for upstreams and routes, validates every request against the shipped JSON Schemas, derives and enforces aliases, and keeps all of it strictly tenant-scoped. This is the persisted state the proxy path later reads.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - Upstream and route CRUD with server-generated UUIDs and anonymous GTS identifiers in path parameters.
  - The management operation flow in the order DESIGN §3.5 states it — authenticate, validate the DTO, write, respond.
  - Bearer-token authentication via `toolkit-auth` and enforcement of the `gts.cf.core.oagw.{upstream,route,*_plugin}.v1~:{create;override;read;delete}` management permissions on every management endpoint (the `*_plugin` arms are enforced by `cpt-cf-oagw-feature-plugin-system`).
  - Request validation against `schemas/upstream.v1.schema.json` and `schemas/route.v1.schema.json`, including endpoint shape, protocol enum, sharing enums, match shape, the `rate_limit` sub-object, and the route-level `cors` object validated with the same shape as the upstream CORS configuration (see the override in Section 1.3, item 5).
  - Alias derivation and enforcement by endpoint type: hostname endpoints always auto-derive, IP-based or non-derivable endpoints require an explicit alias, and a bare public suffix (for example `co.uk`) is never derivable.
  - Alias normalization to ASCII lowercase, trailing-dot stripping, case-insensitive resolution, RFC 1123 hostname validation, and alias immutability across updates.
  - `enabled` flag semantics (default `true`), including propagation of an ancestor disable to all descendants and the ban on descendant re-enabling.
  - Tenant scoping on every operation; ancestor resources return 404 to descendants through this API.
  - Route match-rule uniqueness within an upstream, evaluated over that upstream's enabled routes (same path, priority, and method conflicts with 409), and immutable `upstream_id` on PUT.
  - Full-replacement PUT semantics where omitted optional fields are cleared, except `enabled`, which a replacement carries forward from the stored row when the body omits it (Section 1.3, item 11).
  - OData list parameters `$filter`, `$select`, `$orderby`, `$top`, and `$skip`, with `$top` defaulting to 50 and capped at 100.
  - Persisted model shape and invariants: the `oagw_*` table set, `(tenant_id, alias)` uniqueness, cascade deletes, and single-transaction multi-table writes.
  - The Control Plane L1 configuration cache ADR 0006 assigns to the Control Plane, built and flushed here on every successful write; the Data Plane L1 cache and its invalidation belong to `cpt-cf-oagw-feature-data-plane-proxy` (Section 1.3, item 10).

- **Out of scope**:
  - Effective-config merge across the tenant hierarchy (see `cpt-cf-oagw-feature-hierarchical-config`).
  - Plugin CRUD, binding, and garbage collection (see `cpt-cf-oagw-feature-plugin-system`).
  - Proxy-time alias resolution and shadowing behaviour (owned by `cpt-cf-oagw-feature-hierarchical-config`, consumed by `cpt-cf-oagw-feature-data-plane-proxy`).
  - Plugin and plugin-binding tables: this feature's claim on `cpt-cf-oagw-db-schema` covers the upstream, route, tag, and match tables only.
  - `oagw_route_grpc_match` and gRPC protocol values are created and validated but unused, deferred to Phase 3 per §1.3(4).

- **Phases**: upstream CRUD, then route CRUD, then list/query parameters

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`
  - `p1` - `cpt-cf-oagw-adr-request-routing`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`
  - `p1` - `cpt-cf-oagw-constraint-https-only`
  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - `Upstream`, `Route`, `Endpoint`, `ServerConfig`, `MatchConfig` (`http_match`)
  - Upstream and route tag rows, and the per-tenant uniqueness key `(tenant_id, alias)`
  - REST DTOs mirroring the two JSON Schemas

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-interface-api`
  - `p1` - `cpt-cf-oagw-interface-management-api`
  - `p1` - `cpt-cf-oagw-tech-dependencies`

  This feature delivers the DESIGN §3.2 Request Routing and Internal Services subsections for the management half, plus the Management API contract in DESIGN §3.3.

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.3 [Hierarchical Configuration](features/hierarchical-config.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-hierarchical-config`

- **Purpose**: Makes configuration work across a tenant tree. It walks the hierarchy from a descendant to the root, resolves ancestor aliases with shadowing, applies the three sharing modes, and produces one effective configuration per resolution. Partner and customer hierarchies depend on this behaviour.

- **Depends On**: `cpt-cf-oagw-feature-control-plane-config`

- **Scope**:
  - Sole owner of the tenant hierarchy walk from descendant to root using the tenant chain supplied by the platform.
  - Sole owner of ancestor alias resolution and shadowing, where the closest match wins and enforced ancestor limits still apply.
  - Sole owner of the per-field effective-config merge strategies: auth overrides when `inherit` and is forced when `enforce`; rate limits take `min(ancestor, descendant)`; plugin chains concatenate as ancestor then descendant; CORS unions origins when `inherit` and is forced when `enforce`; tags always union add-only.
  - Sharing modes `private` (owner only), `inherit` (descendants may override), and `enforce` (descendants cannot override), applied per configuration field.
  - Descendant override permissions `oagw:upstream:bind`, `oagw:upstream:override_auth`, `oagw:upstream:override_rate`, and `oagw:upstream:add_plugins`.
  - Binding-style upstream creation where a descendant alias matches an ancestor upstream, including the `private`-blocks-visibility and `enforce`-blocks-override rules.
  - Request tags treated as tenant-local additions during binding-style creation, never mutating ancestor tags.

- **Out of scope**:
  - Persisting hierarchy data; the tenant tree comes from the platform tenant-resolver.
  - Proxy-time consumption of the effective configuration (see `cpt-cf-oagw-feature-data-plane-proxy`).
  - Rate-limit token bucket mechanics, which live in `cpt-cf-oagw-feature-rate-limiting`.

- **Phases**: single phase

- **Requirements Covered**:

  - [x] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - `SharingMode` (`private` / `inherit` / `enforce`)
  - `EffectiveUpstreamConfig`, `EffectiveRouteConfig`
  - Tenant chain, ancestor binding, and the merge result for each field family

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`

  This feature delivers the DESIGN §3.2 Hierarchical Configuration subsection and the plugin-free share of Permissions and Access Control (the descendant override permissions).

- **API**:
  - None (the hierarchy is internal; it is exercised through the existing management endpoints and at proxy time)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.4 [Plugin System](features/plugin-system.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-plugin-system`

- **Purpose**: Supplies the extensibility model for authentication, validation, and mutation. It defines the three plugin contracts and their registries, ships the built-in catalogue, exposes plugin management over REST, and resolves secret material through the credential store. Authentication injection is a p1 capability and lives here.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` contracts with separate registries, exposing the sandbox limits that execution-time enforcement applies, and the deterministic order Auth, then Guards, then Transform on request, then the upstream call, then Transform on response or error.
  - Chain composition where upstream plugins run before route plugins (`[U1, U2] + [R1, R2]` yields `[U1, U2, R1, R2]`).
  - Built-in catalogue: auth `noop`, `apikey`, `oauth2_client_cred`, and `oauth2_client_cred_basic`; guard `required_headers`; transform `request_id`.
  - Catalog-only identifiers with no backing implementation: auth `basic` and `bearer`, guard `timeout` and `cors`, transform `logging` and `metrics`. They are registered in the types-registry only and are rejected when used as a bindable plugin reference.
  - Plugin management API: create, list, get, delete, and `GET /oagw/v1/plugins/{id}/source` for Starlark source; these plugin endpoints inherit the same authentication and permission middleware from the shared router mount delivered by gear-foundation, with the `*_plugin.v1~` permission literals enforced by the same mechanism as the upstream and route endpoints.
  - `plugin_ref` and `plugin_uuid` binding model: each binding carries its chain position, the plugin reference, the optional plugin UUID, and its plugin configuration, positions are contiguous from 0, and the application validates that `plugin_uuid` matches `plugin_ref` when present.
  - Resolution of `plugin_ref` values across the persisted plugin store and the in-process named registry.
  - Immutability after creation, in-use protection returning 409 `PluginInUse`, and garbage-collection eligibility for unlinked custom plugins.
  - Auth plugin identity stored as scalar columns to keep in-use checks off JSON scanning.
  - Credential resolution for auth plugins through `cred://` references, with OAuth2 Client Credentials using an internal token cache.
  - This feature's share of `cpt-cf-oagw-db-schema` is the plugin and plugin-binding tables; the upstream, route, tag, and match tables belong to `cpt-cf-oagw-feature-control-plane-config`.

- **Out of scope**:
  - Plugin execution on a live request, which belongs to `cpt-cf-oagw-feature-data-plane-proxy`.
  - Circuit breaking, which is core policy and not a plugin.
  - gRPC proxying; no gRPC proxy code path is currently implemented or reachable.
  - Plugin versioning and lifecycle management as a separate concern.

- **Phases**: plugin contracts and registries, then built-in catalogue and management API, then bindings and lifecycle

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-cred-isolation`
  - `p2` - `cpt-cf-oagw-principle-plugin-immutable`
  - `p1` - `cpt-cf-oagw-adr-plugin-system`
  - `p1` - `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`
  - `p1` - `cpt-cf-oagw-adr-required-headers-guard-plugin`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`

- **Domain Model Entities**:
  - `Plugin` (UUID-backed custom plugin row), plugin binding rows, and the named-plugin registry entry
  - `AuthContext`, `RequestContext`, `ResponseContext`, `ErrorContext` (consumed from `gear-foundation`), `GuardDecision`
  - Token cache entry for the OAuth2 Client Credentials variants

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-interface-api`

  This feature delivers the DESIGN §3.2 Plugin System and Plugin Lifecycle Management subsections, plus the plugin share of Permissions and Access Control.

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source
  - DELETE /oagw/v1/plugins/{id}

- **Sequences**:

  - None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.5 [Data Plane Proxy](features/data-plane-proxy.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-data-plane-proxy`

- **Purpose**: The request path and the reason the gear exists. It resolves the upstream by alias, matches the route, applies the effective configuration resolved by `cpt-cf-oagw-feature-hierarchical-config`, runs the plugin chain, rewrites headers, and forwards the call. It also tags every response with its error source so callers can tell a gateway failure from an upstream failure.

- **Depends On**: `cpt-cf-oagw-feature-hierarchical-config`, `cpt-cf-oagw-feature-plugin-system`

- **Scope**:
  - Proxy handler for `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`.
  - Invokes the effective-config resolution delivered by `cpt-cf-oagw-feature-hierarchical-config` at proxy time: proxy-time alias resolution against the tenant chain that feature walks, with shadowing and with ancestor `enforce` limits still applied.
  - Route matching by method allowlist, longest path prefix, and priority, honouring `path_suffix_mode`.
  - Consumes the effective configuration in the order upstream, then route, then tenant; the per-field merge strategies themselves are owned by `cpt-cf-oagw-feature-hierarchical-config`.
  - Request-time plugin chain execution in the order Auth, Guards, Transform(request), upstream call, Transform(response/error), including credential injection into the outbound request.
  - Starlark custom-plugin sandbox: no network or file I/O, no imports, enforced per-invocation timeout and memory limits.
  - Header transformation: routing headers consumed, hop-by-hop headers stripped, passthrough rules applied, `Host` or `:authority` replaced with the upstream value.
  - `X-OAGW-Target-Host` endpoint selection with the full behaviour matrix, including the required-header case for common-suffix aliases, and round-robin otherwise.
  - Body validation: `Content-Length` consistency, the 100MB hard limit rejected before buffering, `chunked`-only `Transfer-Encoding`, and rejection of CR/LF injection and conflicting CL/TE combinations.
  - Inbound validation of path, query parameters, and headers against the matched route, returning 400 on failure.
  - Outbound forwarding with no gateway-level re-issue of the original client request; connector-level endpoint or connection attempts stay inside the upstream connector.
  - `X-OAGW-Error-Source: gateway|upstream` on every response and RFC 9457 bodies for every gateway error.
  - Bearer-token authorization requiring `gts.cf.core.oagw.proxy.v1~:invoke` and upstream ownership or ancestor sharing.
  - Data Plane L1 configuration cache with explicit invalidation, plus the shared outbound HTTP client and adaptive per-host HTTP version detection.

- **Out of scope**:
  - The tenant hierarchy walk, alias shadowing, and the per-field merge strategies, which `cpt-cf-oagw-feature-hierarchical-config` owns; this feature only consumes their result at proxy time.
  - Token bucket mechanics and 429 responses (see `cpt-cf-oagw-feature-rate-limiting`).
  - CORS preflight and origin enforcement (see `cpt-cf-oagw-feature-cors`).
  - SSE and WebSocket connection lifecycles (see `cpt-cf-oagw-feature-streaming`).
  - Metrics emission and audit log formatting (see `cpt-cf-oagw-feature-observability`).
  - gRPC proxying and WebTransport, per the overrides in Section 1.3.
  - Response caching, automatic request retries, and DNS/IP-pinning rule implementation details.

- **Phases**: route matching and effective-config invocation, then outbound proxying and error semantics, then streaming and body handling

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-no-cache`
  - `p1` - `cpt-cf-oagw-principle-error-source`
  - `p1` - `cpt-cf-oagw-adr-request-routing`
  - `p1` - `cpt-cf-oagw-adr-error-source-distinction`
  - `p1` - `cpt-cf-oagw-adr-data-plane-caching`
  - `p1` - `cpt-cf-oagw-adr-state-management`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`
  - `p1` - `cpt-cf-oagw-constraint-https-only`
  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - `ProxyContext`, `ProxyResponse`, `OutboundRequest`
  - `ResolvedUpstream`, `SelectedEndpoint`, `MatchedRoute`
  - Gateway error variants carrying `upstream_id`, `host`, `path`, `retry_after_seconds`, and `trace_id`

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-tech-dependencies`
  - `p1` - `cpt-cf-oagw-interface-api`

  This feature delivers the DESIGN §3.2 Alias Resolution, Headers Transformation, Guard Rules, Body Validation Rules, and Transformation Rules subsections.

- **API**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
  - POST /oagw/v1/proxy/api.openai.com/v1/chat/completions (single-endpoint upstream, no target header needed)
  - GET /oagw/v1/proxy/my-service/v1/status with `X-OAGW-Target-Host` selecting one endpoint in a pool

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None

### 2.6 [Rate Limiting](features/rate-limiting.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-rate-limiting`

- **Purpose**: Protects external service agreements and the platform from cost overruns. It enforces token-bucket limits on the proxy path, folds the tenant hierarchy into a single effective limit, and answers rejected callers with standard headers so they can retry correctly. It also owns the circuit breaker that keeps an unhealthy upstream from cascading.

- **Depends On**: `cpt-cf-oagw-feature-data-plane-proxy`

- **Scope**:
  - Token bucket as the default algorithm with sliding window as the optional alternative.
  - Dual-rate configuration: `sustained.rate` with `sustained.window` (`second`/`minute`/`hour`/`day`), `burst.capacity`, and `cost` per request.
  - Counter scopes `global`, `tenant`, `user`, `ip`, and `route`.
  - Strategies `reject` (429), `queue`, and `degrade`.
  - Hierarchical enforcement with `effective = min(selected_rate, route_rate, all_ancestor_enforced_rates)`, the canonical merge formula in DESIGN §3.2 (Hierarchical Configuration); the full per-field merge table is owned by `cpt-cf-oagw-feature-hierarchical-config` and is not restated here. Includes limits inherited across alias shadowing, plus budget modes `unlimited`, `allocated`, and `shared` with overcommit validation.
  - 429 responses carrying `X-RateLimit-*` headers and `Retry-After`, gated by `response_headers`.
  - Circuit breaker with the closed, open, and half-open state machine, tripping within the configured failure window and answering 503 `CircuitBreakerOpen`; circuit-breaker configuration parameters are deferred per DESIGN §4.7(1), so only the state machine and the 503 answer are in scope.
  - Per-instance in-memory buckets owned by the Data Plane, with prefix-based cleanup when an upstream or route is deleted.

- **Out of scope**:
  - Circuit-breaker configuration parameters and fallback strategies, deferred per DESIGN §4.7(1).
  - Redis-backed distributed counters; the graded single-exec posture uses L1 state only.
  - The `rate_limit` configuration schema itself, which is validated by `cpt-cf-oagw-feature-control-plane-config`.
  - Backpressure queueing strategies beyond the `queue` strategy's bounded behaviour.

- **Phases**: single phase

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-error-source`
  - `p1` - `cpt-cf-oagw-adr-rate-limiting`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - `RateLimitConfig` (sustained, burst, scope, strategy, cost, sharing)
  - `TokenBucket`, budget allocation, and `CircuitBreakerState`

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-tech-dependencies`

  This feature delivers the rate-limit merge row of the DESIGN §3.2 Hierarchical Configuration subsection.

- **API**:
  - None (rejections are returned on the existing `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` path)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.7 [CORS](features/cors.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-cors`

- **Purpose**: Lets browser clients call the proxy without opening it to every origin. Preflight is answered locally and cheaply, while the actual cross-origin request is checked against the resolved upstream's configuration before anything is forwarded. The check is built in rather than delegated to a plugin, so it works even when the upstream is unreachable.

- **Depends On**: `cpt-cf-oagw-feature-hierarchical-config`, `cpt-cf-oagw-feature-data-plane-proxy`

- **Scope**:
  - Built-in CORS handler configured per upstream and per route through the `cors` field — the same object shape on both resources, per the Route class in DESIGN §3.1 and the override in Section 1.3, item 5 — disabled unless explicitly enabled.
  - Preflight `OPTIONS` answered with a permissive 204 at the handler level, echoing the requested origin, method, and headers, with no upstream resolution and no tenant context.
  - Actual-request enforcement after upstream resolution: origin not in `allowed_origins` or method not in `allowed_methods` returns 403.
  - Validation that `allow_credentials` is never combined with a wildcard origin.
  - Exact origin matching only — port-sensitive and protocol-sensitive, with no regex patterns that could be bypassed.
  - Response decoration with `Access-Control-Allow-Origin`, `Access-Control-Allow-Methods`, `Access-Control-Expose-Headers`, `Access-Control-Max-Age`, and an always-present `Vary: Origin`.
  - Hierarchical CORS merge following the `inherit` and `enforce` sharing modes.

- **Out of scope**:
  - Proxying CORS to the upstream, and CORS as a guard plugin; both were rejected by the ADR.
  - Preflight authentication, which browsers do not send.

- **Phases**: single phase

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-adr-cors`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - `CorsConfig` (enabled, allowed_origins, allowed_methods, expose_headers, allow_credentials, sharing)
  - `CorsDecision` and the preflight response shape

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`

  This feature delivers the CORS merge row of the DESIGN §3.2 Hierarchical Configuration subsection, plus the CORS part of the §3.2 Security Considerations subsection.

- **API**:
  - OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}] returning 204 with CORS preflight headers

- **Sequences**:

  - None

- **Data**:

  - None

### 2.8 [Streaming](features/streaming.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-streaming`

- **Purpose**: Keeps long-lived responses working through the gateway. Streaming APIs such as chat completions send server-sent events, and bidirectional clients upgrade to WebSocket. The gateway must forward bytes as they arrive, tear connections down cleanly on either side, and still report a clear error when a stream dies.

- **Depends On**: `cpt-cf-oagw-feature-data-plane-proxy`

- **Scope**:
  - Server-sent event forwarding from the upstream to the caller as events arrive, without buffering the whole body.
  - Connection lifecycle handling for open, close, and error on both the client and upstream sides.
  - Client disconnect closes the upstream connection; upstream close closes the client connection and logs the event.
  - WebSocket upgrade proxying, including the `Upgrade` and `Connection` handshake headers and the `wss` endpoint scheme. The `Upgrade` and `Connection` strip rule from DESIGN's hop-by-hop header table is suspended for upgrade requests, so the handshake headers reach the upstream and the 101 response can complete.
  - Idle and request timeout handling for streams, returning 504 gateway errors when a stream stalls.
  - 502 `StreamAborted` with `X-OAGW-Error-Source: gateway` when a stream is terminated mid-flight.

- **Out of scope**:
  - WebTransport. `cpt-cf-oagw-fr-streaming` MUSTs the WebSocket and WebTransport session flows, but DESIGN specifies no WebTransport transport behaviour, so this feature delivers that requirement's HTTP request/response, SSE, and WebSocket clauses and does not deliver its WebTransport clause. This is an explicit scope reduction recorded in Section 1.3, item 4; a proxy attempt against a `wt`-scheme upstream is answered with a gateway error.
  - gRPC streaming; no gRPC proxy code path is currently implemented or reachable.
  - HTTP/3 (QUIC).

- **Phases**: single phase

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

  `cpt-cf-oagw-fr-streaming` is delivered in part by this feature: the HTTP request/response, SSE, and WebSocket clauses are in scope, and its WebTransport clause is not (Section 1.3, item 4).

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-error-source`
  - `p1` - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - `StreamSession`, stream lifecycle state, and the upgrade handshake result

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
  - `p1` - `cpt-cf-oagw-tech-dependencies`

  This feature delivers the DESIGN §3.2 Headers Transformation upgrade exception (the `Upgrade` and `Connection` strip rule suspended for handshakes) and the body-passthrough row of the §3.2 Transformation Rules subsection; the stream lifecycle itself has no DESIGN §3.2 subsection, so `cpt-cf-oagw-component-model` stays an umbrella reference here.

- **API**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` with server-sent event responses forwarded as received
  - GET /oagw/v1/proxy/{alias}[/{path_suffix}] with `Upgrade: websocket` for upgrade proxying

- **Sequences**:

  - None

- **Data**:

  - None

### 2.9 [Observability](features/observability.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-observability`

- **Purpose**: Makes the outbound path legible to operators. Every proxy request carries a correlation ID, configuration changes and failures are logged as structured JSON, and Prometheus metrics expose traffic, latency, errors, and rate-limit state. This is the feature an operator uses first when an upstream starts misbehaving.

- **Depends On**: `cpt-cf-oagw-feature-data-plane-proxy`, `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-control-plane-config`

- **Scope**:
  - Correlation identifiers propagated on every request and echoed in error bodies as `trace_id`.
  - Structured JSON audit logs to stdout with the field set `timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, and `error_type`.
  - Logging of successful requests, failed requests, configuration changes, authentication failures, and circuit-breaker state transitions.
  - Sampling for high-volume routes and rate-limited logging of authentication failures to prevent log flooding.
  - No PII: request and response bodies, query parameters, and headers are never logged except from an allowlist.
  - No secrets: API keys, tokens, and credential material are never logged, returned, or placed in error messages.
  - Prometheus metrics at `/oagw/v1/metrics` covering request counts, request duration, in-flight requests, errors, circuit-breaker state, rate-limit state, routing target selection, and upstream health.
  - Cardinality control: no tenant labels, `http.route` is the normalized match pattern rather than the raw path, and methods are normalized to a standard verb or `_OTHER`.

- **Out of scope**:
  - Distributed tracing backends and dashboard provisioning.
  - Log retention policy, which is an open question in the PRD.
  - Metric scraping infrastructure outside the gear.

- **Phases**: single phase

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - `AuditEvent`, `CorrelationContext`, and the metric label sets

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
  - `p2` - `cpt-cf-oagw-tech-dependencies`

  This feature delivers the DESIGN §4.2 metrics catalogue and the §4.3 audit-log catalogue; it has no DESIGN §3.2 subsection, so `cpt-cf-oagw-component-model` stays an umbrella reference here.

- **API**:
  - GET /oagw/v1/metrics

- **Sequences**:

  - None

- **Data**:

  - None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    ├─→ cpt-cf-oagw-feature-control-plane-config
    │         ↓
    │         └─→ cpt-cf-oagw-feature-hierarchical-config
    │                    ↓
    ├─→ cpt-cf-oagw-feature-plugin-system
    │                    ↓
    │        cpt-cf-oagw-feature-data-plane-proxy
    │        (converges hierarchical-config and plugin-system)
    │                    ↓
    │        ├─→ cpt-cf-oagw-feature-rate-limiting
    │        ├─→ cpt-cf-oagw-feature-cors
    │        │        second parent: cpt-cf-oagw-feature-hierarchical-config
    │        ├─→ cpt-cf-oagw-feature-streaming
    │        └─→ cpt-cf-oagw-feature-observability
                 │        second parent: cpt-cf-oagw-feature-rate-limiting
                 │        third parent: cpt-cf-oagw-feature-control-plane-config
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-control-plane-config` requires `cpt-cf-oagw-feature-gear-foundation`: it persists and validates the domain types, reuses the `DomainError` catalogue for 400/404/409 responses, and registers its handlers on the router the foundation creates.
- `cpt-cf-oagw-feature-hierarchical-config` requires `cpt-cf-oagw-feature-control-plane-config`: a hierarchy walk is meaningless without persisted upstreams and routes, and the sharing modes are fields on the objects that feature owns.
- `cpt-cf-oagw-feature-plugin-system` requires `cpt-cf-oagw-feature-gear-foundation`: it needs the plugin base-type identifiers, the domain contracts, and the types-registry provisioning, but it does not need upstream or route persistence, so it branches off the root directly.
- `cpt-cf-oagw-feature-data-plane-proxy` requires `cpt-cf-oagw-feature-hierarchical-config`: proxy-time alias resolution and effective-config merge are the hierarchy walk consumed at request time.
- `cpt-cf-oagw-feature-data-plane-proxy` requires `cpt-cf-oagw-feature-plugin-system`: the proxy path executes the auth, guard, and transform chains and resolves secret material through the credential store contract.
- `cpt-cf-oagw-feature-rate-limiting` requires `cpt-cf-oagw-feature-data-plane-proxy`: the check runs inside the resolved proxy context, and 429 and 503 answers replace the response the proxy would have produced.
- `cpt-cf-oagw-feature-cors` requires `cpt-cf-oagw-feature-hierarchical-config`: the effective CORS configuration is the merged per-tenant result, and preflight responses must not depend on tenant resolution.
- `cpt-cf-oagw-feature-cors` requires `cpt-cf-oagw-feature-data-plane-proxy`: origin and method enforcement happens after upstream resolution and before forwarding, on the proxy handler's own path.
- `cpt-cf-oagw-feature-streaming` requires `cpt-cf-oagw-feature-data-plane-proxy`: it changes how the proxy response body is transferred, not what is resolved.
- `cpt-cf-oagw-feature-observability` requires `cpt-cf-oagw-feature-data-plane-proxy`: correlation, audit fields, and the proxy-path metrics are all derived from the proxy request and response lifecycle.
- `cpt-cf-oagw-feature-observability` requires `cpt-cf-oagw-feature-rate-limiting`: it reports rate-limit state and 429 outcomes, which only exist once that feature owns them.
- `cpt-cf-oagw-feature-observability` requires `cpt-cf-oagw-feature-control-plane-config`: it logs configuration changes and reads configuration state, so it cannot be completed before that feature's write path exists.
- `cpt-cf-oagw-feature-control-plane-config` and `cpt-cf-oagw-feature-plugin-system` are independent of each other and can be developed in parallel once `cpt-cf-oagw-feature-gear-foundation` exists.
- `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-cors`, `cpt-cf-oagw-feature-streaming`, and the proxy-reading slice of `cpt-cf-oagw-feature-observability` are mutually independent and can be developed in parallel once `cpt-cf-oagw-feature-data-plane-proxy` exists; `cors` additionally waits on `cpt-cf-oagw-feature-hierarchical-config`, and `cpt-cf-oagw-feature-observability` additionally waits on `cpt-cf-oagw-feature-rate-limiting` and `cpt-cf-oagw-feature-control-plane-config`.
