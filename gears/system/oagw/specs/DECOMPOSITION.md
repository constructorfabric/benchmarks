# Decomposition: OAGW — Outbound API Gateway


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation & Registration - MEDIUM](#21-gear-foundation--registration---medium)
  - [2.2 Domain Model & Repositories - HIGH](#22-domain-model--repositories---high)
  - [2.3 Control-Plane Management API - HIGH](#23-control-plane-management-api---high)
  - [2.4 Plugin System - HIGH](#24-plugin-system---high)
  - [2.5 Error Semantics & RFC 9457 Framework - MEDIUM](#25-error-semantics--rfc-9457-framework---medium)
  - [2.6 Rate Limiting & Caching Policies - MEDIUM](#26-rate-limiting--caching-policies---medium)
  - [2.7 CORS Handling - LOW](#27-cors-handling---low)
  - [2.8 Data-Plane Proxy Service - HIGH](#28-data-plane-proxy-service---high)
  - [2.9 Observability & Audit - MEDIUM](#29-observability--audit---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

## 1. Overview

The OAGW design (`cpt-cf-oagw-design-overview`) is decomposed into nine implementable features organized around the Control Plane / Data Plane separation that defines the gear: configuration management (rare, correctness-sensitive, tenant-scoped) is cleanly isolated from request proxying (high-frequency, latency-sensitive), while both planes share one domain layer and one deployment unit.

**Decomposition Strategy**:

- **Foundation first, then semantics**: the gear registration and crate skeleton build first, followed by the domain model and repositories that every other feature consumes, then management and plugin surfaces, then the proxy hot path and its policy layers.
- **High cohesion, loose coupling**: each feature groups design elements that change together — e.g., all repository/data concerns in one feature, all plugin-trait concerns in another, all error-contract concerns in one framework feature.
- **Mutual exclusivity**: every DESIGN element (7 components, 3 sequences, 1 schema, 10 tables, 7 principles, 6 constraints) and every PRD FR/NFR (14 FR + 8 NFR) is assigned to exactly one feature (see coverage statement below). Cross-cutting behavior is delivered by the owning feature and consumed by dependents through explicit `Depends On` links, never by duplicate scope.
- **Acyclic dependency ordering**: priorities `p1`–`p5` are assigned in dependency order; the foundation feature has no dependencies; all dependency edges form a valid DAG (Section 3).

**Coverage statement**: 100% of DESIGN definition IDs and 100% of PRD FR/NFR IDs are assigned to features. All reference checkboxes use EXISTING IDs from the pipeline PRD (`cpt-cf-oagw-fr-*`, `cpt-cf-oagw-nfr-*`) and DESIGN (`cpt-cf-oagw-principle-*`, `cpt-cf-oagw-constraint-*`, `cpt-cf-oagw-component-*`, `cpt-cf-oagw-seq-*`, `cpt-cf-oagw-db-*`, `cpt-cf-oagw-dbtable-*`), cross-checked with `cfs list-ids`.

**Refinements to the suggested breakdown (with reasoning)**:

- **Error Semantics & RFC 9457 Framework is its own feature (p4) dependent on the Control-Plane Management API (F3) rather than on the Data-Plane Proxy (F8)**, and the Data-Plane Proxy instead depends on it. Rationale: the proxy hot path (F8) emits the framework's gateway errors, so the framework must exist before the proxy — this yields a valid DAG (no cycle) and keeps the error catalog, retriability classification, and `X-OAGW-Error-Source` contract in exactly one feature (`cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-fr-error-codes`).
- **Deliberate coverage handling (DOC-001)**: (a) `oagw_route_grpc_match` (Phase 3) is assigned to Domain Model & Repositories with an explicit gate — its in-memory shape is reserved but no gRPC code path is implemented or reachable; (b) WebSocket/WebTransport tunneling named in `cpt-cf-oagw-fr-streaming` is not elaborated in DESIGN — the Data-Plane Proxy covers HTTP/SSE streaming passthrough and WebSocket/WebTransport is explicitly omitted; (c) CORS has no dedicated PRD FR — the CORS feature documents this explicitly rather than forcing a phantom requirement link; (d) the Gear Foundation feature covers no PRD FR/NFR directly (scaffolding is enabling work, covered transitively) and says so explicitly.

## 2. Entries

**Overall implementation status:**

- [ ] `p1` - **ID**: `cpt-cf-oagw-status-overall`

### 2.1 [Gear Foundation & Registration](./FEATURE.md#feature-oagw-gear-foundation--registration) - MEDIUM

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Register the `oagw` gear with the ToolKit host and establish the crate skeleton, configuration module, plane separation, and REST/OpenAPI wiring that every other feature builds on.

- **Depends On**: None

- **Scope**:
  - `#[toolkit::gear(name = "oagw", ...)]` registration: dependencies (`credstore`, `types_registry`, `tenant_resolver`, `authz_resolver`), capabilities `[rest, stateful]`, lifecycle entry (`init`/`serve`) per `cpt-cf-oagw-component-model`
  - Gear configuration module (`gears.oagw.config` YAML via `config_or_default`) with defaults: `proxy_timeout_secs` (2), `allow_http_upstream` (false), `ssrf_policy.enabled` (true), `token_cache_ttl_secs` (300), `token_cache_capacity` (10000)
  - DDD-Light module skeleton (`api/rest`, `domain`, `infra`) and the Control Plane / Data Plane service split within the single `oagw` crate
  - `RestApiCapability::register_rest` wiring and utoipa/OpenAPI registration of unprefixed paths (the platform `api-gateway` applies the `/api/oagw/v1/...` prefix)
  - Request routing between planes: `/upstreams/*`, `/routes/*`, `/plugins/*` → Control Plane; `/proxy/*` → Data Plane
  - Single-gear deployment inside the ToolKit host per `cpt-cf-oagw-topology-deployment`
  - Technology/dependency baseline from `cpt-cf-oagw-tech-dependencies` (axum 0.8, toolkit-http, credstore/`types_registry`/tenant-resolver/authz-resolver SDK clients, opentelemetry)

- **Out of scope**:
  - No management or proxy behaviors yet (handlers are wired by dependent features F3/F8)
  - No persistence, plugin registries, or proxy-client implementations (F2, F4, F8)
  - gRPC support (Phase 3)

- **Requirements Covered**:

  - None — gear registration/config scaffolding has no directly-traceable PRD FR/NFR; requirement coverage is provided transitively by every dependent feature that this foundation enables.

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - `OagwConfig` (gear-level configuration with defaults)

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-model`

- **API**:
  - None (gear registration, configuration module, and OpenAPI scaffolding only; REST endpoints are added by the dependent features)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.2 [Domain Model & Repositories](./FEATURE.md#feature-oagw-domain-model--repositories) - HIGH

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-domain-model-repositories`

- **Purpose**: Establish the domain entities, repository contracts, in-memory storage, tenant scoping, and hierarchical configuration-merge semantics that both planes consume.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - Domain entities: `Upstream`, `Route`, `RouteMatch`, `RouteMethod`, `ServerConfig`/`Endpoint` pool, `HostEntry`/`TargetHost`, `Plugin`, vault-aware `cred://` secret references, sharing-mode configuration
  - Repository traits (`UpstreamRepository`, `RouteRepository`, `PluginRepository`) and in-memory DashMap implementations whose shapes mirror the §3.7 relational tables (`cpt-cf-oagw-db-schema`)
  - Tenant-scoped reads/writes, server-generated UUIDs, tenant-chain resolution invariants
  - Hierarchical configuration merge: sharing modes `private`/`inherit`/`enforce`, `min()` rate inheritance, plugin concatenation, add-only tag union (`cpt-cf-oagw-component-domain-layer`)
  - SSRF guard on upstream endpoint URLs (scheme allowlist, RFC 1123 hostname validation) at the configuration boundary
  - `infra/storage` implementations of the repository traits; `infra/proxy` and `infra/plugin` modules are delivered with the Data-Plane Proxy (F8) and Plugin System (F4) under the same `cpt-cf-oagw-component-infra-layer` contracts

- **Out of scope**:
  - SeaORM/`toolkit-db` persistence swap (future, behind the same repository traits)
  - `oagw_route_grpc_match`: the in-memory table shape is reserved here, but gRPC proxying is Phase 3 — no gRPC code path is implemented or reachable in this or any dependent feature
  - Distributed/Redis-backed rate-limit state
  - DNS resolution / IP pinning rule implementation details

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [ ] `p2` - `cpt-cf-oagw-fr-hierarchical-config`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-multi-sql`
  - [ ] `p2` - `cpt-cf-oagw-constraint-in-memory-storage`

- **Domain Model Entities**:
  - Upstream
  - Route, RouteMatch, RouteMethod
  - ServerConfig, Endpoint, HostEntry / TargetHost
  - PluginConfig
  - Secret reference (`cred://` URI)

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-domain-layer`
  - [ ] `p2` - `cpt-cf-oagw-component-infra-layer`

- **API**:
  - None (internal domain/repository layer; no REST surface)

- **Sequences**:

  - None

- **Data**:

  - [ ] `p3` - `cpt-cf-oagw-db-schema`
  - `p2` - `cpt-cf-oagw-dbtable-upstream`
  - `p2` - `cpt-cf-oagw-dbtable-route`
  - `p2` - `cpt-cf-oagw-dbtable-route-http-match`
  - `p2` - `cpt-cf-oagw-dbtable-route-grpc-match` (shape reserved; Phase 3 gRPC — out of scope)
  - `p2` - `cpt-cf-oagw-dbtable-route-method`
  - `p2` - `cpt-cf-oagw-dbtable-upstream-tag`
  - `p2` - `cpt-cf-oagw-dbtable-route-tag`

### 2.3 [Control-Plane Management API](./FEATURE.md#feature-oagw-control-plane-management-api) - HIGH

- [ ] `p3` - **ID**: `cpt-cf-oagw-feature-control-plane-api`

- **Purpose**: Expose the tenant-scoped REST management surface for upstreams, routes, and plugins with validation, alias derivation, permissions, and OData-style list semantics.

- **Depends On**: `cpt-cf-oagw-feature-domain-model-repositories`

- **Scope**:
  - REST CRUD for upstreams (`/api/oagw/v1/upstreams`) and routes (`/api/oagw/v1/routes`, including HTTP match/methods and plugin sub-resources); plugin CRUD (`/api/oagw/v1/plugins` plus `GET /plugins/{id}/source`)
  - Alias auto-derivation and validation (public-suffix-list common-suffix for hostname endpoints, immutable, unique per `(tenant_id, alias)`; IP/non-derivable endpoints require an explicit alias); target-host decision input matrix for multi-endpoint upstreams
  - DTO validation, `SecurityContext` extraction, `toolkit-auth` bearer authentication, and GTS permission checks via `authz_resolver`
  - Tenant-scoped visibility (ancestor resources return 404), `(tenant_id, alias)` and match-rule uniqueness validation, `enabled` flag support on upstreams and routes
  - OData-ish list query parameters (`$filter`, `$select`, `$orderby`, `$top` default 50 max 100, `$skip`) and utoipa OpenAPI documentation per `cpt-cf-oagw-interface-management-api` / `cpt-cf-oagw-interface-api`
  - Management operation orchestration per `cpt-cf-oagw-seq-management-crud-flow` (`cpt-cf-oagw-component-control-plane`, `cpt-cf-oagw-component-api-layer`)

- **Out of scope**:
  - Proxy data-path execution (F8)
  - Plugin trait execution, registries, and custom plugin runtime (F4)
  - Rate-limit and CORS enforcement at request time (F6, F7)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - UpstreamConfig / RouteConfig / PluginConfig management DTOs

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-api-layer`
  - [ ] `p2` - `cpt-cf-oagw-component-control-plane`

- **API**:
  - POST /api/oagw/v1/upstreams, GET /api/oagw/v1/upstreams, GET/PUT/DELETE /api/oagw/v1/upstreams/{id}
  - POST /api/oagw/v1/routes, GET /api/oagw/v1/routes, GET/PUT/DELETE /api/oagw/v1/routes/{id}
  - POST /api/oagw/v1/plugins, GET /api/oagw/v1/plugins, GET/DELETE /api/oagw/v1/plugins/{id}, GET /api/oagw/v1/plugins/{id}/source

- **Sequences**:

  - `p3` - `cpt-cf-oagw-seq-management-crud-flow`

- **Data**:

  - None

### 2.4 [Plugin System](./FEATURE.md#feature-oagw-plugin-system) - HIGH

- [ ] `p3` - **ID**: `cpt-cf-oagw-feature-plugin-system`

- **Purpose**: Deliver the three plugin trait families, GTS identifier resolution, built-ins, catalog-only entries, custom plugin lifecycle, and sandboxed Starlark execution.

- **Depends On**: `cpt-cf-oagw-feature-domain-model-repositories`

- **Scope**:
  - `AuthPlugin` / `GuardPlugin` / `TransformPlugin` traits and plugin registries (`infra/plugin`) with GTS-identifier resolution; `plugin_ref`/`plugin_uuid` identification model and binding conflict detection
  - Built-ins: `noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic` (auth); `required_headers` (guard); `request_id` (transform)
  - Catalog-only identifiers registered in `types_registry` but not resolvable via a plugin registry: `basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`
  - Deterministic execution order semantics: Auth → Guards → Transform(request) → upstream → Transform(response/error); upstream plugins before route plugins
  - OAuth2 token cache (`pingora-memory-cache` with key re-verification, TTL `min(config_ttl, expires_in − 30s)`)
  - `cred://` secret resolution via the CredStore SDK with zeroizing secret types
  - Custom Starlark plugin lifecycle: immutable after creation, garbage collection after TTL; sandboxed execution with no network/file I/O and enforced timeout/memory limits

- **Out of scope**:
  - Rate limiting, CORS, timeout enforcement, request/response logging, and metrics — core Data Plane instrumentation delivered in F6, F7, and F9 (catalog identifiers only here)
  - `basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics` implementations (catalog-only, not bindable)
  - Plugin versioning/lifecycle management beyond immutable custom plugins
  - Management API surface for plugin CRUD (owned by F3; registries are consumed in-process)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-cred-isolation`
  - [ ] `p2` - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Plugin
  - PluginConfig
  - AuthPlugin / GuardPlugin / TransformPlugin trait contracts

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-plugin-system`

- **API**:
  - None (plugin CRUD surface owned by F3; registries and execution are in-process)

- **Sequences**:

  - None

- **Data**:

  - `p3` - `cpt-cf-oagw-dbtable-plugin`
  - `p3` - `cpt-cf-oagw-dbtable-upstream-plugin`
  - `p3` - `cpt-cf-oagw-dbtable-route-plugin`

### 2.5 [Error Semantics & RFC 9457 Framework](./FEATURE.md#feature-oagw-error-semantics--rfc-9457-framework) - MEDIUM

- [ ] `p4` - **ID**: `cpt-cf-oagw-feature-error-semantics`

- **Purpose**: Provide the single RFC 9457 problem+json error framework with the full GTS instance catalog, retriability classification, and gateway/upstream error-source signaling used by every plane.

- **Depends On**: `cpt-cf-oagw-feature-control-plane-api`

- **Scope**:
  - Central `application/problem+json` error type with standard fields (`type`, `title`, `status`, `detail`, `instance`) and OAGW extension fields (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`)
  - Full GTS instance catalog from the DESIGN error table: validation, missing/invalid/unknown target-host, authentication failed, route not found, plugin in use, payload too large, rate limit exceeded, secret not found, protocol/downstream/stream-aborted, link unavailable, circuit-breaker open, plugin not found, connection/request/idle timeout
  - Retriable vs. non-retriable classification per instance (rate-limit, link, circuit-breaker, and timeouts retriable; validation/auth/route-not-found/payload/secret not)
  - `X-OAGW-Error-Source: gateway|upstream` response header contract (`cpt-cf-oagw-principle-error-source`); upstream bodies pass through unchanged
  - `request_id` propagation into error responses

- **Out of scope**:
  - Proxy mechanics that trigger the errors at runtime (F8 consumes this framework)
  - Upstream error passthrough byte handling (F8)
  - Error taxonomy only — no routing, plugin execution, or policy behavior

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-rfc9457`
  - [ ] `p2` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - GatewayError (problem+json envelope)
  - Error instance catalog (GTS `type` identifiers)

- **Design Components**:

  - None

- **API**:
  - None (shared serialization contract consumed by management and proxy handlers)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.6 [Rate Limiting & Caching Policies](./FEATURE.md#feature-oagw-rate-limiting--caching-policies) - MEDIUM

- [ ] `p4` - **ID**: `cpt-cf-oagw-feature-rate-limiting`

- **Purpose**: Enforce dual-rate token-bucket rate limiting at upstream and route levels with hierarchical inheritance, standard 429 semantics, and control-plane configuration caching.

- **Depends On**: `cpt-cf-oagw-feature-plugin-system`, `cpt-cf-oagw-feature-error-semantics`

- **Scope**:
  - Token-bucket dual-rate (sustained/burst) configuration per upstream and per route with hierarchical `min()` effective-rate inheritance
  - Strategy behavior `reject` / `queue` / `degrade`; the reject strategy emits the 429 `RateLimitExceeded` gateway error via the RFC 9457 framework with `Retry-After` and `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` headers
  - Per-instance in-process rate limiters (no distributed coordination in this delivery); Control Plane `L1` configuration cache feeding proxy-time lookup
  - Rate-limit exceeded sequence per `cpt-cf-oagw-seq-rate-limit-flow`
  - Caching policy per `cpt-cf-oagw-principle-no-cache`: no upstream response caching — only control-plane configuration caching

- **Out of scope**:
  - Redis-backed distributed rate limiting (future, behind the local mode)
  - Upstream response caching (never — client/upstream responsibility)
  - Rate-limit counter persistence across restarts (future)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - RateLimitConfig (dual-rate token bucket)
  - Token-bucket state keyed by (upstream, route)

- **Design Components**:

  - None

- **API**:
  - None (in-process policy executed on the proxy hot path)

- **Sequences**:

  - `p4` - `cpt-cf-oagw-seq-rate-limit-flow`

- **Data**:

  - None

### 2.7 [CORS Handling](./FEATURE.md#feature-oagw-cors-handling) - LOW

- [ ] `p4` - **ID**: `cpt-cf-oagw-feature-cors-handling`

- **Purpose**: Provide per-upstream/route CORS handling: a permissive preflight fast path and strict validation of actual cross-origin requests.

- **Depends On**: `cpt-cf-oagw-feature-domain-model-repositories`

- **Scope**:
  - Permissive preflight fast path (204) for OPTIONS without tenant resolution at the handler level
  - Exact-match origin validation and method + header validation on actual cross-origin requests
  - 403 rejection with `Vary: Origin` for disallowed origins/methods
  - `CorsConfig` read from upstream/route effective configuration; secure default (CORS disabled unless configured)

- **Out of scope**:
  - No dedicated PRD FR exists for CORS (verified against the PRD FR/NFR inventory). CORS behavior is governed by the `cors` catalog identifier registered in `cpt-cf-oagw-fr-builtin-plugins` (Plugin System, F4) and by upstream/route configuration CRUD (F3). This feature intentionally carries no requirement checkbox — documented explicitly so the omission is not silent.
  - No CORS plugin implementation (catalog identifier only)

- **Requirements Covered**:

  - None (explicitly — see out-of-scope note above)

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - CorsConfig

- **Design Components**:

  - None

- **API**:
  - None (in-process behavior on the proxy path; preflight handled in the API handler layer)

- **Sequences**:

  - None

- **Data**:

  - None

### 2.8 [Data-Plane Proxy Service](./FEATURE.md#feature-oagw-data-plane-proxy-service) - HIGH

- [ ] `p5` - **ID**: `cpt-cf-oagw-feature-data-plane-proxy`

- **Purpose**: Orchestrate the proxy hot path: alias resolution, route matching, effective-configuration application, plugin chain execution, upstream forwarding with streaming passthrough, header handling, circuit breaking, and error-source tagging.

- **Depends On**: `cpt-cf-oagw-feature-control-plane-api`, `cpt-cf-oagw-feature-plugin-system`, `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-cors-handling`, `cpt-cf-oagw-feature-error-semantics`

- **Scope**:
  - `DataPlaneService` (`infra/proxy/service.rs`): alias resolution (descendant → root tenant-chain walk with shadowing), route matching (HTTP method allowlist + longest path prefix), and effective-configuration merge application at request time
  - Plugin chain execution per the deterministic ordering (Auth → Guards → Transform(request) → upstream → Transform(response/error))
  - `X-OAGW-Target-Host` / target-host header matrix application, header transforms, and hop-by-hop header stripping
  - Streaming/SSE-compatible passthrough bodies via the toolkit HTTP client; timeout policy from `proxy_timeout_secs`; no automatic retries; circuit breaker
  - SSRF enforcement on the outbound surface: HTTPS-only upstream connections, hostname validation, 100MB payload-size cap, `Transfer-Encoding`/`Content-Length` validation
  - `enabled` flag semantics: disabled upstream → 503 service-unavailable; disabled route excluded from matching; ancestor-disabled resources stay disabled for descendants
  - Gateway vs upstream error-source mapping via the RFC 9457 framework (F5): gateway errors as problem+json with `X-OAGW-Error-Source: gateway`; upstream responses pass through unchanged with `X-OAGW-Error-Source: upstream`
  - Integration hooks on the hot path for rate-limit checks (F6) and CORS actual-request enforcement (F7)
  - Proxy flow orchestration per `cpt-cf-oagw-seq-proxy-flow` (`cpt-cf-oagw-component-data-plane`)
  - Proxy API surface per `cpt-cf-oagw-interface-proxy-api`

- **Out of scope**:
  - Error framework definition, GTS instance catalog, and retriability classification (F5 — consumed, not redefined)
  - Metrics and audit log emission implementation (F9 — hooks only)
  - Automatic retries (never — client responsibility) and upstream response caching (client/upstream responsibility)
  - gRPC proxying (Phase 3)
  - WebSocket/WebTransport tunneling: named in `cpt-cf-oagw-fr-streaming` but not elaborated in DESIGN — this delivery covers HTTP/SSE streaming passthrough; the WS/WT session flows are explicitly omitted (DOC-001)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-no-retry`

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-no-direct-internet`
  - [ ] `p2` - `cpt-cf-oagw-constraint-body-limit`
  - [ ] `p2` - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - ProxyContext
  - ProxyResponse
  - HeadersConfig (routing / hop-by-hop / passthrough categories)
  - TargetHost selection

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-data-plane`

- **API**:
  - `*` /api/oagw/v1/proxy/{alias}[/{path_suffix}][?{query}] (method allowlist + longest path prefix)

- **Sequences**:

  - `p5` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - None

### 2.9 [Observability & Audit](./FEATURE.md#feature-oagw-observability-and-audit) - MEDIUM

- [ ] `p5` - **ID**: `cpt-cf-oagw-feature-observability-audit`

- **Purpose**: Emit the Prometheus metrics vocabulary, structured JSON audit logs, and request/error logging with correlation IDs across management and proxy operations.

- **Depends On**: `cpt-cf-oagw-feature-data-plane-proxy`, `cpt-cf-oagw-feature-rate-limiting`, `cpt-cf-oagw-feature-error-semantics`

- **Scope**:
  - Prometheus metrics vocabulary per DESIGN §4 with the defined label set and cardinality controls: `oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_requests_in_flight`, `oagw_errors_total`, `oagw_circuit_breaker_state` / `oagw_circuit_breaker_transitions_total`, `oagw_rate_limit_exceeded_total` / `oagw_rate_limit_usage_ratio`, `oagw_routing_target_host_used` / `oagw_routing_endpoint_selected`, `oagw_upstream_available` / `oagw_upstream_connections`; `/metrics` admin-only endpoint
  - Structured JSON audit logging to stdout with fields per DESIGN §4 (`timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, `error_type`); no PII and no secrets; high-volume log rate limiting
  - `request_id` / `trace_id` correlation propagation into metrics, logs, and gateway error responses

- **Out of scope**:
  - Distributed tracing exporter integration (future)
  - Tenant labels in metrics (prohibited by DESIGN cardinality rules)
  - Audit log retention policy and plugin GC scheduling (deferred platform concerns)

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`

- **Design Principles Covered**:

  - None

- **Design Constraints Covered**:

  - None

- **Domain Model Entities**:
  - Audit log record
  - Metrics registry

- **Design Components**:

  - None

- **API**:
  - GET /metrics (admin-only)

- **Sequences**:

  - None

- **Data**:

  - None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    └─→ cpt-cf-oagw-feature-domain-model-repositories
        ├─→ cpt-cf-oagw-feature-control-plane-api ──→ cpt-cf-oagw-feature-error-semantics
        │       cpt-cf-oagw-feature-error-semantics ──→ cpt-cf-oagw-feature-data-plane-proxy
        ├─→ cpt-cf-oagw-feature-plugin-system ──→ cpt-cf-oagw-feature-rate-limiting
        │       cpt-cf-oagw-feature-error-semantics ──→ cpt-cf-oagw-feature-rate-limiting
        │       cpt-cf-oagw-feature-rate-limiting ──→ cpt-cf-oagw-feature-data-plane-proxy
        ├─→ cpt-cf-oagw-feature-cors-handling ──→ cpt-cf-oagw-feature-data-plane-proxy
        ├─→ cpt-cf-oagw-feature-error-semantics ──→ cpt-cf-oagw-feature-observability-audit
        ├─→ cpt-cf-oagw-feature-rate-limiting ──→ cpt-cf-oagw-feature-observability-audit
        └─→ cpt-cf-oagw-feature-data-plane-proxy ──→ cpt-cf-oagw-feature-observability-audit
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-domain-model-repositories` requires `cpt-cf-oagw-feature-gear-foundation`: the crate skeleton, configuration module, and gear registration must exist before domain entities and repository implementations can be placed and wired.
- `cpt-cf-oagw-feature-control-plane-api` requires `cpt-cf-oagw-feature-domain-model-repositories`: management CRUD operates on the domain entities and repository traits established by the domain layer.
- `cpt-cf-oagw-feature-plugin-system` requires `cpt-cf-oagw-feature-domain-model-repositories`: plugin trait contracts and custom-plugin persistence depend on the domain model, plugin entities, and binding-table shapes.
- `cpt-cf-oagw-feature-error-semantics` requires `cpt-cf-oagw-feature-control-plane-api`: the RFC 9457 framework is defined alongside the API layer that serializes gateway errors and exposes the GTS instance catalog.
- `cpt-cf-oagw-feature-rate-limiting` requires `cpt-cf-oagw-feature-plugin-system`: rate-limit policy refines the request-policy contract the plugin/guard layer establishes; and requires `cpt-cf-oagw-feature-error-semantics`: the 429 `RateLimitExceeded` response is emitted through the framework.
- `cpt-cf-oagw-feature-cors-handling` requires `cpt-cf-oagw-feature-domain-model-repositories`: `CorsConfig` is part of the upstream/route effective configuration produced by the domain layer.
- `cpt-cf-oagw-feature-data-plane-proxy` requires the control-plane API, plugin system, rate limiting, CORS handling, and error semantics features: the proxy orchestrates configuration resolution from the control plane, executes plugin chains, enforces rate limits and CORS, and emits framework gateway errors — all built as dependencies, never re-implemented.
- `cpt-cf-oagw-feature-observability-audit` requires `cpt-cf-oagw-feature-data-plane-proxy`, `cpt-cf-oagw-feature-rate-limiting`, and `cpt-cf-oagw-feature-error-semantics`: metrics and audit emission instrument the proxy hot path and management operations (which must exist first) and observe the rate-limiting usage ratio and the error-source attribution produced by the error-semantics framework.

**DAG note**: The dependency graph is acyclic; `cpt-cf-oagw-feature-gear-foundation` is the single foundation feature with no dependencies. Features at the same priority (`p3`: control-plane-api + plugin-system; `p4`: error-semantics + rate-limiting + cors-handling; `p5`: data-plane-proxy + observability-audit) are independent tracks and can be developed in parallel once their listed dependencies are complete.
