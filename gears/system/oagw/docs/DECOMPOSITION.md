# Decomposition: OAGW (Outbound API Gateway)


<!-- toc -->

- [1. Overview](#1-overview)
  - [Spec corrections applied (task-mandated, recorded here and in the affected FEATURE docs)](#spec-corrections-applied-task-mandated-recorded-here-and-in-the-affected-feature-docs)
- [2. Entries](#2-entries)
  - [2.1 Gear Wiring and Error Contract - HIGH](#21-gear-wiring-and-error-contract---high)
  - [2.2 Domain Model and Storage - HIGH](#22-domain-model-and-storage---high)
  - [2.3 Alias Resolution Rules - MEDIUM](#23-alias-resolution-rules---medium)
  - [2.4 Management API (Control Plane) - HIGH](#24-management-api-control-plane---high)
  - [2.5 Plugin System and Built-in Plugins - MEDIUM](#25-plugin-system-and-built-in-plugins---medium)
  - [2.6 Hierarchical Configuration Resolution - MEDIUM](#26-hierarchical-configuration-resolution---medium)
  - [2.7 Rate Limiting - HIGH](#27-rate-limiting---high)
  - [2.8 Proxy Pipeline (Data Plane) - HIGH](#28-proxy-pipeline-data-plane---high)
  - [2.9 Streaming Proxy (SSE and WebSocket) - HIGH](#29-streaming-proxy-sse-and-websocket---high)
  - [2.10 Observability - MEDIUM](#210-observability---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [ ] `p1` - **ID**: `cpt-cf-oagw-status-oagw`
## 1. Overview

The approved DESIGN for OAGW is decomposed into 10 cohesive features, ordered by
dependency. The decomposition strategy is platform-first: the gear skeleton and its
error contract come first, then the domain model, then the alias rules that the
management API builds on, then the control plane, then the plugin chain, then
hierarchical configuration resolution, rate limiting, the proxy pipeline (data
plane), streaming, and finally observability, which instruments everything above it.

Each feature is independently implementable and testable against the PRD/DESIGN/ADR
identifiers it covers; the traceability section of every FEATURE doc maps back to
the same IDs listed here. Four spec corrections mandated by the deployment task are
recorded in the "Spec corrections applied" subsection below and are carried into the
affected FEATURE docs only — PRD, DESIGN and ADR are read-only upstream inputs and
are never edited.

**Artifact conventions (declared)**:

- Each feature is delivered as a **single phase**; there is no subordinate
  phase decomposition below feature level (ATTR-005/LEV-003 disposition).
- Feature IDs `cpt-cf-oagw-feature-{slug}` are stable configuration-item
  identifiers: the slug never changes across revisions, and revision history is
  carried by the repository baseline rather than a version suffix (CFG-002).
- Checkbox semantics: `[x]` on a "Requirements Covered" row mirrors the
  upstream PRD definition state (that requirement is already delivered by
  platform components outside oagw) and does **not** indicate oagw
  implementation progress; oagw progress is tracked only by the overall status
  line and each feature's own `cpt-cf-oagw-feature-*` block. Design-element
  rows (Principles / Constraints / Components / Sequences / Data) are emitted
  without task checkboxes because their DESIGN definitions do not use task
  tracking (deterministic rule `ref-task-def-no-task`); only Requirements
  Covered rows carry checkboxes (FMT-003 deviation, declared).
- Shared design-element coverage is intentional and scoped per feature; the
  owning boundary for each shared ID is stated in the feature's Scope or
  Out-of-scope bullets (EXC-001). `cpt-cf-oagw-seq-proxy-flow` is owned
  stage-wise: feature-plugin-chain owns the plugin stages, feature-rate-limiting
  owns the rate-limit stage, feature-proxy-pipeline owns orchestration and the
  endpoint-selection stages, feature-hierarchical-config owns config resolution
  feeding those stages, feature-streaming-proxy owns the streaming stages.
- Entities listed under "Domain Model Entities" that are not part of the
  DESIGN §3.1 aggregate set (`TenantChain`, `EffectiveConfig`, `TokenBucket`,
  `Alias`, `ProxyContext`, `ProxyResponse`, `OagwConfig`, `OagwError`) are
  derived implementation value types owned by the named feature, not new
  canonical aggregates (DEP-DOC-002 precedence note).

FEATURE artifacts under `features/` are authored as the next chain stage
immediately after this decomposition; the `features/{slug}.md` links below are
the canonical filenames for those artifacts.

### Spec corrections applied (task-mandated, recorded here and in the affected FEATURE docs)

1. **Route registration base path** — all oagw routes are registered at `/oagw/v1/...` WITHOUT a leading `/api` (task mandate; gear-relative paths are also how sibling gears register routes — e.g. account-management mounts `/account-management/v1/...`). The paths written in PRD/DESIGN as `/api/oagw/v1/...` are absolute paths behind an operator-facing gateway and are not the paths this deployment serves. FEATURE docs and their API sections use `/oagw/v1/...`; the `/api/oagw/v1/...` form is documented as the operator-gateway-prefixed alias.
2. **`http` is a legal endpoint scheme** — the management API must accept `"scheme": "http"` for upstream endpoints (`gears.oagw.config.allow_http_upstream: true` in the graded config lifts the `cpt-cf-oagw-constraint-https-only` default posture). Two distinct concerns stay apart: (a) which schemes the `Upstream.server.endpoints[].scheme` field accepts — `http`, `https`, `wss`, `grpc`, `wt`; (b) whether a plaintext connection is actually established — governed only by `allow_http_upstream` (default `false`). This extends the scheme enum of `schemas/upstream.v1.schema.json` (`https`, `wss`, `wt`, `grpc`) with `http`; `wt` remains valid configuration but is not proxied (deferral 7).
3. **Storage is in-memory for this release** — `config/e2e-local.yaml` provisions no `database` section for `oagw`, so the repository traits are implemented by in-memory stores. The repository traits (`domain/repo.rs`) keep persistence swappable; `cpt-cf-oagw-constraint-multi-sql` is honoured at the trait boundary in this release (no backend-specific code), with SQL persistence deferred. `cpt-cf-oagw-db-schema` is covered as the **schema contract** (field names, `(tenant_id, alias)` uniqueness, cascade semantics, route-match determinism invariants) that the in-memory stores enforce; materialising the nine SQL tables is deferred with SQL persistence, and no feature in this release creates tables.
4. **Proxy engine bridge** — the Data Plane is implemented as a streaming HTTP bridge (`DataPlaneServiceImpl` on hyper/hyper-util) rather than the Pingora engine in this release; the `pingora-*` crates remain declared dependencies for the planned engine swap. Also deferred and recorded per FEATURE: Starlark custom-plugin *execution* (custom plugins remain first-class CRUD resources), fine-grained permission enforcement (delegated to platform authz middleware; tenant scoping is enforced by oagw), and the circuit breaker mechanism (the `CircuitBreakerOpen` 503 error type is reserved per the DESIGN error table, mechanism listed in DESIGN 4.7 future work). Control-plane and data-plane config caching (ADR 0005 L1/L2, ADR 0006 DP L1) is superseded by the in-memory stores of correction 3 — lookups hit the in-memory repositories directly. Plugin garbage collection (`gc_eligible_at`, TTL job, DESIGN 3.2/3.6) is deferred together with plugin versioning (DESIGN 4.5 future work).
5. **Metrics exposure** — oagw registers its metric instruments on the platform telemetry stack (the toolkit OpenTelemetry SDK configured by the host); it does not mount its own `/metrics` route. The PRD's "metrics scraped at /metrics endpoint" acceptance is therefore satisfied at the platform telemetry surface (the graded config exports metrics via `opentelemetry.exporter.kind: otlp_grpc`), not by an oagw-owned scrape endpoint.
6. **Rate-limit strategy** — only the `reject` strategy (429 + `Retry-After`) is implemented this release; `queue` and `degrade` remain accepted configuration values that resolve to `reject` behaviour, and any other value fails validation. This is a declared narrowing of PRD `cpt-cf-oagw-fr-rate-limiting`'s strategy list.
7. **WebTransport** — `wt` endpoints are valid configuration (scheme enum, correction 2) but WebTransport proxying is not implemented this release; a proxy request routed to a `wt` endpoint is answered with the gateway `RouteError`/`ProtocolError` problem+json semantics rather than proxied. This is a declared residual of `cpt-cf-oagw-fr-streaming` (HTTP, SSE and WebSocket are fully proxied).
8. **Sliding-window approximation** — the rate-limit `algorithm` config value `sliding_window` is accepted (per ADR 0003) but is enforced this release as a fixed-window approximation of the sliding window, documented in the FEATURE; `token_bucket` is the exact default algorithm.

**Phase numbering precedence**: where DESIGN says gRPC service/method matching is "planned for Phase 3" and the PRD says gRPC proxying is "planned for phase 4", the PRD's Phase 4 governs (the PRD is the scope-of-record for phasing); no gRPC proxy code path is implemented in this release either way.

## 2. Entries

### 2.1 [Gear Wiring and Error Contract](features/gear-wiring.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-wiring`

- **Purpose**: Stand up the `oagw` ToolKit gear inside `gears/system/oagw/oagw/`: crate module layout, `OagwConfig`, gear registration with the ToolKit runtime, REST route mounting under `/oagw/v1`, GTS type provisioning in the types-registry, and the RFC 9457 problem+json error mapping with `X-OAGW-Error-Source: gateway|upstream`.

- **Depends On**: None

- **Scope**:
  - Crate module layout and gear registration: `src/lib.rs`, `src/gear.rs`, `src/config.rs` stand up the `oagw` gear with `RestApiCapability::register_rest` wiring per the toolkit gear contract (`cpt-cf-oagw-constraint-toolkit-deploy`)
  - `OagwConfig` with the DESIGN/ADR defaults (proxy timeout, `allow_http_upstream`, `ssrf_policy.enabled`, body limit, per-ADR 0008 token-cache settings) read via the gear config surface
  - Platform dependency wiring: the `types_registry` SDK client (gear dependency crate) and the platform-provided `cred_store`, `toolkit-auth` and `tenant-resolver` clients (reached through the toolkit client hub, not gear-level deps) are resolved in `init()` per the PRD dependency table — `cred_store` is what auth plugins use to resolve `cred://` secret references at request time
  - GTS type provisioning (`infra/type_provisioning.rs`) for upstream/route/plugin/protocol/error identifier families via `types_registry` (`cpt-cf-oagw-contract-types-registry`)
  - Error mapping (`api/rest/error.rs`, `domain/error.rs`): the DESIGN 20-row error table as `OagwError` → `application/problem+json` with exact GTS `type` ids, standard + extension fields, and `X-OAGW-Error-Source: gateway`, plus the two ADR 0004 CORS error types (`cors.origin_not_allowed.v1`, `cors.method_not_allowed.v1`, HTTP 403)
  - Route registration shell: management + proxy paths under `/oagw/v1/...` (correction 1), returning placeholder problem+json responses for endpoints implemented by later features

- **Out of scope**:
  - Handler bodies for management/proxy endpoints (later features)
  - Metrics/audit emission (feature-observability)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`

- **Domain Model Entities**:
  - OagwConfig
  - OagwError

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`

- **API**:
  - GET /oagw/v1/upstreams (registered; implemented by feature-management-api)
  - POST /oagw/v1/proxy/{alias}/{path} (registered; implemented by feature-proxy-pipeline)

- **Sequences**: None

- **Data**: None

### 2.2 [Domain Model and Storage](features/domain-model.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-domain-model`

- **Purpose**: Define the domain layer: `Upstream`, `Route`, `Plugin` aggregates with their configuration value objects (server endpoints, auth, headers, rate limit, cors, plugins, tags), request validation, `DomainError`, repository traits, and in-memory repository implementations.

- **Depends On**: `cpt-cf-oagw-feature-gear-wiring`

- **Scope**:
  - Domain aggregates and value objects matching `cpt-cf-oagw-design-domain-model` and `./schemas/upstream.v1.schema.json` / `./schemas/route.v1.schema.json` (with correction 2: `http` accepted as endpoint scheme)
  - Validation rules: hostname RFC 1123, alias pattern, tags pattern, endpoint homogeneity (same protocol/scheme/port), rate-limit/cors/plugin shape validation, `cors.allow_credentials` + wildcard rejection
  - `domain/repo.rs` traits (`UpstreamRepository`, `RouteRepository`, `PluginRepository`) with tenant-scoped lookups and `infra/storage/` in-memory implementations (correction 3)
  - Plugin identification model: `plugin_ref` always stored, `plugin_uuid` derived when UUID-backed

- **Out of scope**:
  - HTTP transport, service orchestration, alias derivation rules (feature-alias-resolution)
  - SQL/SeaORM persistence

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Upstream
  - Route
  - Plugin
  - Endpoint
  - RateLimitConfig
  - CorsConfig
  - HeadersConfig

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-domain-model`

- **API**: None

- **Sequences**: None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.3 [Alias Resolution Rules](features/alias-resolution.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-alias-resolution`

- **Purpose**: Implement the alias contract that turns upstream endpoints into routing keys: auto-derivation (single host, host:port, PSL common suffix), explicit-alias requirement for IPs/non-derivable pools, normalization, and the immutable-alias update transition table.

- **Depends On**: `cpt-cf-oagw-feature-domain-model`

- **Scope**:
  - Derivation rules incl. standard ports (HTTP 80, HTTPS/WSS/gRPC 443), `hostname:port` for non-standard ports, registrable common suffix via `psl` (suffix must be ≥2 labels and not a bare public suffix), port-preserving suffix form `suffix:port`
  - Normalization (ASCII lowercase, trailing dot stripped), case-insensitive resolution, alias uniqueness per `(tenant_id, alias)`
  - Update transition table enforcement (derivable→derivable, derivable→non-derivable, non-derivable→non-derivable, non-derivable→derivable, no endpoint change)

- **Out of scope**:
  - Tenant-hierarchy shadowing walk (feature-hierarchical-config)
  - Management API surface (feature-management-api)

- **Requirements Covered**:

  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Upstream
  - Alias
  - Endpoint

- **Design Components**:

  - `p1` - `cpt-cf-oagw-design-domain-model`

- **API**: None

- **Sequences**: None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.4 [Management API (Control Plane)](features/management-api.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-management-api`

- **Purpose**: Implement `ControlPlaneService` and the 15 management endpoints for upstream/route/plugin CRUD with create/replace semantics, immutable fields, tenant scoping (ancestor resources 404), enable/disable, plugin immutability and in-use protection, and OData list parameters.

- **Depends On**: `cpt-cf-oagw-feature-alias-resolution`

- **Scope**:
  - REST endpoints (correction 1 paths): `POST|GET /oagw/v1/upstreams`, `GET|PUT|DELETE /oagw/v1/upstreams/{id}`, `POST|GET /oagw/v1/routes`, `GET|PUT|DELETE /oagw/v1/routes/{id}`, `POST|GET /oagw/v1/plugins`, `GET|DELETE /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`
  - CRUD semantics: server-generated UUIDs; IDs as `gts.cf.core.oagw.{type}.v1~{uuid}`; PUT = full replacement; immutable `id`, `tenant_id`, route `upstream_id`; alias update rules from feature-alias-resolution; route match uniqueness `(path, priority, method)` → 409; upstream alias conflict → 409
  - `enabled` flag semantics: disabled upstream rejects proxy traffic; disable propagates to descendants
  - Plugin lifecycle: no PUT on plugins (a `PUT /oagw/v1/plugins/{id}` returns 405 — decomposition-level decision consistent with the immutability principle; `DELETE` returns `409 PluginInUse` per DESIGN) with `plugin_id` + `referenced_by.{upstreams,routes}` body, GTS source endpoint for Starlark plugins
  - OData list params `$filter`, `$select`, `$orderby`, `$top` (default 50, max 100), `$skip`
  - Tenant scoping: all CRUD reads/writes scoped to `SecurityContext::subject_tenant_id()`; ancestor resources 404 to descendants

- **Out of scope**:
  - Hierarchical effective-config merge (feature-hierarchical-config)
  - Proxy-time resolution (feature-proxy-pipeline)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-plugin-immutable`
  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - Upstream
  - Route
  - Plugin

- **Design Components**:

  - `p1` - `cpt-cf-oagw-interface-api`

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
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**: None

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.5 [Plugin System and Built-in Plugins](features/plugin-chain.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-plugin-chain`

- **Purpose**: Implement the three plugin traits with deterministic execution order and the built-in plugins: auth (`noop`, `apikey`, `oauth2_client_cred`, `oauth2_client_cred_basic` with internal token cache), guard (`required_headers`), transform (`request_id`).

- **Depends On**: `cpt-cf-oagw-feature-domain-model`, `cpt-cf-oagw-feature-gear-wiring`

- **Scope**:
  - `domain/plugin/` traits `AuthPlugin`, `GuardPlugin`, `TransformPlugin` with the ADR 0002 signatures; registries (`AuthPluginRegistry::with_builtins`, `GuardPluginRegistry::with_builtins`, `TransformPluginRegistry::with_builtins`) resolving named GTS plugin ids; catalog-only ids (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) are NOT resolvable
  - Execution order Auth → Guards → Transform(on_request) → upstream → Transform(on_response/on_error); upstream plugins before route plugins
  - `ApiKeyAuthPlugin` (header injection from `cred://` secret ref resolved through the `cred_store` client per `cpt-cf-oagw-contract-cred-store`), `NoopAuthPlugin`
  - `OAuth2ClientCredAuthPlugin` per ADR 0008: credential fetch via `fetch_token` (no background watcher) with an internal token cache honouring the ADR's key, verification and TTL rules; failed fetches not cached; Form and Basic variants
  - `RequiredHeadersGuardPlugin` per ADR 0009 (fail-open when unconfigured; first missing header reported; 400 request phase / 502 response phase)
  - `RequestIdTransformPlugin` (X-Request-ID injection/propagation)
  - Credential isolation: `SecretString` handling, no secret logging

- **Out of scope**:
  - Starlark custom plugin execution engine (correction 4)
  - Circuit breaker, CORS, timeout (core data plane logic)

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - Plugin
  - RequestContext
  - ResponseContext
  - ErrorContext

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`

- **API**: None

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.6 [Hierarchical Configuration Resolution](features/hierarchical-config.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-hierarchical-config`

- **Purpose**: Resolve effective configuration across the tenant hierarchy: sharing modes per field, merge strategies, ancestor-to-descendant alias shadowing, and enforced-ancestor persistence.

- **Depends On**: `cpt-cf-oagw-feature-management-api`

- **Scope**:
  - Sharing modes (`private`, `inherit`, `enforce`) per config field (auth, rate limit, plugins, CORS)
  - Merge strategies: auth override if `inherit`, forced if `enforce`; rate limits `min(ancestor, descendant)`; plugins concatenated ancestor→descendant; CORS origins union if `inherit`, forced if `enforce`; tags always add-only union. This feature **computes** the merged effective limit and hands it to the data plane; it does not enforce it
  - Alias resolution walk descendant→root with closest-match-wins shadowing; enforced ancestor constraints never bypassed by shadowing
  - Tenant chain walk via the tenant-resolver client; disabled ancestors disable descendants

- **Out of scope**:
  - Rate-limit token bucket mechanics and 429 enforcement (feature-rate-limiting, which consumes the computed effective limit)
  - Management CRUD (feature-management-api)

- **Requirements Covered**:

  - [x] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [x] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

- **Domain Model Entities**:
  - Upstream
  - Route
  - TenantChain
  - EffectiveConfig

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`

- **API**: None

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

### 2.7 [Rate Limiting](features/rate-limiting.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-rate-limiting`

- **Purpose**: Token-bucket rate limiting with dual-rate configuration, hierarchical budget enforcement, and standard response headers on the proxy path.

- **Depends On**: `cpt-cf-oagw-feature-hierarchical-config`

- **Scope**:
  - Token bucket (default) with `sustained.rate`/`sustained.window`, `burst.capacity` (default = sustained rate), `cost`, `scope`, `strategy: reject` (queue/degrade accepted in config, `reject` behaviour implemented per correction 6)
  - Effective limit = `min(own, enforced ancestors, route)` per ADR 0003 inheritance rules, **consumed** from the effective config computed by feature-hierarchical-config
  - `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset` / `Retry-After` response headers; 429 with `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`
  - Per-instance limiter registry owned by the data plane (ADR 0006); sliding-window algorithm accepted in config and enforced as a fixed-window approximation per correction 8, documented in the FEATURE

- **Out of scope**:
  - Redis-backed distributed sync (ADR 0003 future work)
  - Budget allocation validation across tenant creation (ADR 0003 schema extension)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [x] `p2` - `cpt-cf-oagw-fr-hierarchical-config`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - RateLimitConfig
  - TokenBucket

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`

- **API**:
  - POST /oagw/v1/proxy/{alias}/{path} (429 enforcement)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.8 [Proxy Pipeline (Data Plane)](features/proxy-pipeline.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-proxy-pipeline`

- **Purpose**: Implement `DataPlaneService` and the proxy endpoint: request classification, route matching, endpoint selection with `X-OAGW-Target-Host`, header processing, body validation, plugin chain execution, the streaming outbound call, response passthrough, and gateway error semantics with `X-OAGW-Error-Source`.

- **Depends On**: `cpt-cf-oagw-feature-rate-limiting`

- **Scope**:
  - Proxy endpoint `{METHOD} /oagw/v1/proxy/{alias}/{path}` (correction 1); built-in CORS handler per ADR 0004 (permissive 204 preflight, origin/method 403 on actual requests, `Vary: Origin`, credentials+wildcard rejected at validation)
  - Route matching: protocol-scoped HTTP match, method allowlist, longest path prefix, priority, `query_allowlist`, `path_suffix_mode`
  - Endpoint selection per ADR 0001 matrix: single endpoint (header optional), multi-endpoint explicit alias (optional → round-robin), multi-endpoint common-suffix alias (header REQUIRED, else `missing_target_host`); invalid/unknown values → 400 `invalid_target_host`/`unknown_target_host`
  - Header processing: routing headers consumed (`X-OAGW-Target-Host`), hop-by-hop strip list (with the WebSocket carve-out of feature-streaming-proxy: `Upgrade`/`Connection`/`Sec-WebSocket-*` are preserved for upgrade requests only), `Host` replaced with upstream host, `upstream.headers` set/add/remove/passthrough rules
  - Body validation: Content-Length match → 400, >100 MB → 413, unsupported Transfer-Encoding → 400; HTTP request-smuggling defences (CR/LF rejected in header values, CL/TE combinations rejected)
  - HTTP/2 support: adaptive per-host version negotiation with a cached per-host capability, per DESIGN 4.4
  - Plugin chain execution in order; response passthrough with `X-OAGW-Error-Source: upstream` on upstream errors and `gateway` on gateway errors; full DESIGN error table mapping (including the two ADR 0004 CORS 403 types)
  - Scheme policy: `https`/`wss`/`grpc` always allowed; `http` endpoints require `allow_http_upstream: true` (correction 2); SSRF policy hook (`ssrf_policy.enabled`)
  - No full client-request retries and no response caching — connector-level endpoint failover / connection-retry attempts by the upstream connector remain permitted per `cpt-cf-oagw-fr-request-proxy`; request timeout from `proxy_timeout_secs` → 504

- **Out of scope**:
  - WebSocket/WebTransport specifics (feature-streaming-proxy)
  - gRPC proxying (PRD out of scope, Phase 4)
  - Circuit breaker mechanism (correction 4)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-no-cache`
  - `p1` - `cpt-cf-oagw-principle-error-source`
  - `p1` - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`
  - `p1` - `cpt-cf-oagw-constraint-https-only`

- **Domain Model Entities**:
  - Upstream
  - Route
  - ProxyContext
  - ProxyResponse
  - Endpoint

- **Design Components**:

  - `p1` - `cpt-cf-oagw-interface-api`

- **API**:
  - GET /oagw/v1/proxy/{alias}
  - POST /oagw/v1/proxy/{alias}/{path}
  - PUT /oagw/v1/proxy/{alias}/{path}
  - PATCH /oagw/v1/proxy/{alias}/{path}
  - DELETE /oagw/v1/proxy/{alias}/{path}

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.9 [Streaming Proxy (SSE and WebSocket)](features/streaming-proxy.md) - HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-streaming-proxy`

- **Purpose**: Pass through streamed responses without buffering: `text/event-stream` SSE bodies frame-by-frame, and WebSocket upgrade proxying (client upgrade → upstream upgrade → bidirectional frame pump), all carrying `X-OAGW-Error-Source`.

- **Depends On**: `cpt-cf-oagw-feature-proxy-pipeline`

- **Scope**:
  - SSE: detect upstream `text/event-stream` (or any non-buffered stream) and forward body frames incrementally with correct `Content-Type`, no retry, stream abort → 502 `stream.aborted`
  - WebSocket: detect client `Upgrade: websocket`, establish the upstream WebSocket (RFC 6455 handshake against the resolved endpoint), then pump frames bidirectionally until either side closes; gateway-side errors before the upgrade keep problem+json semantics
  - Header carve-out owned by this feature: `Upgrade`, `Connection` and `Sec-WebSocket-*` are exempt from the hop-by-hop strip list of feature-proxy-pipeline for WebSocket upgrade requests only
  - Timeout semantics for streams: `proxy_timeout_secs` applies to connection/establishment, idle-read timeout applies to the stream (504 `timeout.idle`)

- **Out of scope**:
  - WebTransport (catalogued, not proxied in this release)
  - gRPC streaming (Phase 4)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `2` - `cpt-cf-oagw-nfr-observability`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`

- **Domain Model Entities**:
  - ProxyContext
  - Upstream

- **Design Components**:

  - `p1` - `cpt-cf-oagw-interface-api`

- **API**:
  - GET /oagw/v1/proxy/{alias}/{path} (SSE)
  - GET /oagw/v1/proxy/{alias}/{path} (WebSocket upgrade)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**: None

### 2.10 [Observability](features/observability.md) - MEDIUM

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-observability`

- **Purpose**: Emit the DESIGN's metrics and audit logs for control-plane and data-plane activity.

- **Depends On**: `cpt-cf-oagw-feature-proxy-pipeline`

- **Scope**:
  - Prometheus metric instruments with the DESIGN 4.2 names and the request-duration histogram buckets `[0.001,0.005,0.01,0.025,0.05,0.1,0.25,0.5,1.0,2.5,5.0,10.0]`, OTel-aligned label keys, no tenant labels
  - Structured JSON audit log fields (timestamp, level, event, request_id, tenant_id, principal_id, host, path, method, status, duration_ms, request_size, response_size, error_type); no PII, no secrets, no bodies/headers
  - Audit events for management configuration changes and authentication failures (DESIGN 4.3 "What is Logged"), emitted from the control-plane and proxy paths
  - Rate-limit and error counters wired into the proxy path

- **Out of scope**:
  - `/metrics` endpoint exposure and scrape auth (platform telemetry surface, correction 5)
  - Distributed tracing export configuration

- **Requirements Covered**:

  - [ ] `2` - `cpt-cf-oagw-nfr-observability`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - MetricInstruments (derived implementation type)
  - AuditEvent (derived implementation type)

- **Design Components**:

  - `p1` - `cpt-cf-oagw-tech-dependencies`

- **API**: None

- **Sequences**: None

- **Data**: None

---

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-wiring
    ↓
cpt-cf-oagw-feature-domain-model
    ↓
    ├─→ cpt-cf-oagw-feature-plugin-chain ─┐
    ↓                                     │
cpt-cf-oagw-feature-alias-resolution      │
    ↓                                     │
cpt-cf-oagw-feature-management-api        │
    ↓                                     │
cpt-cf-oagw-feature-hierarchical-config   │
    ↓                                     │
cpt-cf-oagw-feature-rate-limiting         │
    ↓                                     │
cpt-cf-oagw-feature-proxy-pipeline ←──────┘
    ↓
    ├─→ cpt-cf-oagw-feature-streaming-proxy
    └─→ cpt-cf-oagw-feature-observability
```

Note: `cpt-cf-oagw-feature-plugin-chain` depends on `cpt-cf-oagw-feature-domain-model` and `cpt-cf-oagw-feature-gear-wiring` only, so it can be developed in parallel with `cpt-cf-oagw-feature-alias-resolution` and `cpt-cf-oagw-feature-management-api`; it is consumed by `cpt-cf-oagw-feature-proxy-pipeline` (the edge `plugin-chain → proxy-pipeline` above).

**Dependency Rationale**:

- `cpt-cf-oagw-feature-domain-model` requires `cpt-cf-oagw-feature-gear-wiring`: domain types, config and errors are hosted by the gear skeleton and its error contract.
- `cpt-cf-oagw-feature-alias-resolution` requires `cpt-cf-oagw-feature-domain-model`: derivation operates on `Upstream` endpoints and persists through the repository traits.
- `cpt-cf-oagw-feature-management-api` requires `cpt-cf-oagw-feature-alias-resolution`: upstream create/replace must derive and validate aliases.
- `cpt-cf-oagw-feature-plugin-chain` requires `cpt-cf-oagw-feature-domain-model`: plugin traits operate on the domain request/response contexts and are configured from upstream/route config value objects. It also requires `cpt-cf-oagw-feature-gear-wiring`: the traits and registries are hosted in the crate skeleton that feature stands up.
- `cpt-cf-oagw-feature-hierarchical-config` requires `cpt-cf-oagw-feature-management-api`: effective configuration is resolved from the stored upstreams/routes through the repository surface.
- `cpt-cf-oagw-feature-rate-limiting` requires `cpt-cf-oagw-feature-hierarchical-config`: effective limits are `min(own, enforced ancestors, route)` computed there.
- `cpt-cf-oagw-feature-proxy-pipeline` requires `cpt-cf-oagw-feature-rate-limiting`: the proxy path enforces the rate limit before dispatching upstream, and requires `cpt-cf-oagw-feature-plugin-chain`: the proxy path executes the plugin chain on every proxied request.
- `cpt-cf-oagw-feature-streaming-proxy` requires `cpt-cf-oagw-feature-proxy-pipeline`: streaming extends the proxy pipeline's outbound call and response mapping.
- `cpt-cf-oagw-feature-observability` requires `cpt-cf-oagw-feature-proxy-pipeline`: metrics and audit records are emitted from the proxy pipeline stages.
- `cpt-cf-oagw-feature-streaming-proxy` and `cpt-cf-oagw-feature-observability` are independent of each other and can be developed in parallel.
