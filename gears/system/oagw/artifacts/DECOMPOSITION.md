---
status: accepted
date: 2026-08-27
---

# Decomposition: OAGW Gear (cf-gears-oagw)

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Shell, Configuration, and Host Registration ⏳ HIGH](#21-gear-shell-configuration-and-host-registration--high)
  - [2.2 Control Plane: Upstream/Route/Plugin CRUD, In-Memory Storage, and Error Contract ⏳ HIGH](#22-control-plane-upstreamrouteplugin-crud-in-memory-storage-and-error-contract--high)
  - [2.3 Proxy Data Plane: Resolution, Matching, Forwarding, and Streaming ⏳ HIGH](#23-proxy-data-plane-resolution-matching-forwarding-and-streaming--high)
  - [2.4 Plugin System: Traits, Registries, and Built-in Plugins ⏳ MEDIUM](#24-plugin-system-traits-registries-and-built-in-plugins--medium)
  - [2.5 Rate Limiting and CORS ⏳ MEDIUM](#25-rate-limiting-and-cors--medium)
  - [2.6 Cross-cutting: Type Provisioning (GTS), Observability, and OpenAPI ⏳ MEDIUM](#26-cross-cutting-type-provisioning-gts-observability-and-openapi--medium)
  - [2.7 Test and Verification Suite ⏳ LOW](#27-test-and-verification-suite--low)
- [3. Feature Dependencies](#3-feature-dependencies)
- [4. Coverage Matrix](#4-coverage-matrix)
- [5. Build and Stage Ordering](#5-build-and-stage-ordering)
- [6. Traceability](#6-traceability)

<!-- /toc -->

## 1. Overview

This DECOMPOSITION breaks the OAGW implementation DESIGN (`/app/gears/system/oagw/artifacts/DESIGN.md`) into seven dependency-ordered, mutually exclusive FEATURE-level work items that together cover 100% of the gear's implementation, per the accepted pipeline ADRs (`cpt-cf-oagw-adr-gear-architecture`, `cpt-cf-oagw-adr-proxy-data-plane`, `cpt-cf-oagw-adr-plugins-rate-limit-cors`) and the gear's own accepted contract (`docs/DESIGN.md`, `docs/ADR/0001..0009`, JSON Schemas, `oagw/Cargo.toml`).

**Decomposition Strategy**:

- Features are grouped by functional cohesion following the DESIGN component model: gear shell + config (F1), control-plane CRUD + storage + error contract (F2), proxy data plane (F3), plugin system (F4), rate limiting + CORS (F5), cross-cutting type provisioning/observability/OpenAPI (F6), and verification (F7).
- Dependencies form a strict DAG derived from implementation order: F1 (foundation, no dependencies) feeds F2, which feeds F3; F4 and F5 are independent branches off F3 (parallelizable); F6 integrates F4 + F5; F7 verifies the whole.
- The plugin/rate-limit/CORS invocation seams are established by the data plane (F3) with no-op defaults; F4 and F5 provide the concrete components and wire them into those seams. The `domain/plugin` trait family is authored in F3 as the data-plane seam contract and realized by F4 (registries + built-ins) — an explicit, non-silent shared element (DOC-001/EXC-001).
- The single in-memory control-plane data layout (`cpt-cf-oagw-db-inmemory`) is owned entirely by F2; F3 reads resolved configuration through the control-plane service and never touches storage directly.
- Each FEATURE below will be elaborated into its own FEATURE artifact under `artifacts/features/` by `cf-sdlc-doc-feature`; the feature list here is intentionally small (7) so each FEATURE document stays practical. Feature IDs are stable and are reused verbatim by those FEATURE artifacts.
- 100% coverage is verified in Section 4 (Coverage Matrix): every DESIGN component, interface, sequence, data entity, principle, and constraint is assigned to exactly one feature (with the documented exception above), and every PRD FR/NFR/interface/contract/usecase id is covered by at least one feature.

## 2. Entries

**Overall implementation status:**

- [ ] `p1` - **ID**: `cpt-cf-oagw-status-overall`

### 2.1 [Gear Shell, Configuration, and Host Registration](feature-gear-shell/) ⏳ HIGH

- [ ] `p1` - **ID**: `cpt-cf-oagw-feature-gear-shell`

- **Purpose**: Establish the runnable OAGW gear as a standard `toolkit::gear` REST component on the `api-gateway` surface — module tree, host registration, config surface, route mounting, and extractor infrastructure — so that management and proxy capabilities are reachable under `/oagw/v1` and every later feature plugs into a stable shell.

- **Depends On**: None

- **Scope**:
  - `src/lib.rs` public exports and `#[toolkit::gear(name = "oagw", capabilities = [rest], deps = [types-registry, cred-store, tenant-resolver, authz-resolver])]` gear declaration per `cpt-cf-oagw-component-gear`.
  - `src/gear.rs`: `Gear::init` loads `OagwConfig` from the `oagw` config section via `ctx.config_or_default()` (lenient fallback to `Default`), resolves the SDK clients (`dyn TypesRegistryClient`, `dyn CredStoreClientV1`, tenant-resolver, authz-resolver) from `ctx.client_hub()`, and stores the gear state behind `OnceLock`s.
  - `RestApiCapability::register_rest`: `api/rest/routes.rs` registers the management and proxy route table (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/plugins/{id}/source`, `/oagw/v1/proxy/{alias}...`) on the host router via `OperationBuilder`; the host applies `prefix_path` and the gear never nests its own router.
  - `src/config.rs`: `OagwConfig` (`proxy_timeout_secs` default 30/e2e 2, `allow_http_upstream` default false/e2e true, `ssrf_policy.enabled` default true/e2e false, `token_cache { ttl_secs, capacity }`) with `serde(deny_unknown_fields, default)` + manual `Default`, mirroring the workspace config idiom.
  - Full module tree scaffolding per the DESIGN "Gear Structure" (`api/rest/{handlers,routes,dto,error,extractors}`, `domain/{services,plugin,dto,repo,error}`, `infra/{proxy,storage,plugin,type_provisioning}`) with the `api -> domain <- infra` dependency direction enforced by module visibility (DDD-Light).
  - `api/rest/extractors.rs`: `SecurityContext`/tenant extraction and the permission-check harness wired to the `authz-resolver` SDK, applied by all handlers.
  - `api/rest/dto.rs` serde/utoipa scaffolding (schema-complete DTOs authored in F2/F3; full OpenAPI annotation completed in F6).
  - Handler wiring points (management and proxy) are registered here and implemented/finalized by F2 (management handlers) and F3 (proxy handler).

- **Out of scope**:
  - Management/proxy business logic (F2/F3), plugin registries and built-ins (F4), rate limiter/CORS components (F5), type provisioning and metrics wiring (F6) — `Gear::init` collaborator constructors land with their owning features and are fully assembled by F6.
  - Persistence: no OAGW DB migration scaffold is introduced (`cpt-cf-oagw-constraint-inmemory-cp` is realized in F2).
  - Any background data-plane task or `RunnableCapability`/`rest_host` capability (`cpt-cf-oagw-constraint-single-exec`).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-gear-registration`
  - [ ] `p1` - `cpt-cf-oagw-nfr-build-constraints`
  - [ ] `p1` - `cpt-cf-oagw-interface-host-registration`
  - [ ] `p1` - `cpt-cf-oagw-contract-authn-authz`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-cp-dp-separation`
  - [ ] `p2` - `cpt-cf-oagw-principle-ddd-light`

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-lockfile-only`
  - [ ] `p2` - `cpt-cf-oagw-constraint-single-exec`
  - [ ] `p2` - `cpt-cf-oagw-constraint-toolkit-deploy`

- **Domain Model Entities**:
  - None — this feature creates no domain entities; the entity model is introduced with the control plane (F2).

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-gear`
  - [ ] `p2` - `cpt-cf-oagw-component-config`
  - [ ] `p3` - `cpt-cf-oagw-design-gear`
  - [ ] `p3` - `cpt-cf-oagw-tech-layers`
  - [ ] `p3` - `cpt-cf-oagw-tech-dependencies`
  - [ ] `p3` - `cpt-cf-oagw-topology-single-exec`

- **API**:
  - Host registration surface mounting `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy/{alias}...` (wire paths `/api/oagw/v1/...` after host `prefix_path`)
  - No feature-owned endpoints; handlers are implemented by F2/F3

- **Sequences**:

  - [ ] `p1` - None

- **Data**:

  - [ ] `p1` - None

- **Acceptance (DoD)**:
  - Gear builds and runs under the workspace e2e feature set (`config/e2e-features.txt` includes `oagw`) with zero additions to the lockfile (`cpt-cf-oagw-nfr-build-constraints`).
  - `Gear::init` loads `OagwConfig` via `config_or_default()` with e2e overrides (`config/e2e-local.yaml`); `register_rest` returns the host router with the full `/oagw/v1` route table; SDK clients resolve from `client_hub`.
  - Module tree matches the DESIGN "Gear Structure"; domain modules compile with no `infra` dependency.
  - Code review confirms registration/permissions/security-context extraction; integration smoke test mounts the router.

### 2.2 [Control Plane: Upstream/Route/Plugin CRUD, In-Memory Storage, and Error Contract](feature-control-plane/) ⏳ HIGH

- [ ] `p2` - **ID**: `cpt-cf-oagw-feature-control-plane`

- **Purpose**: Implement the tenant-scoped management plane — the sole writer of control-plane state — with full upstream/route/plugin CRUD, schema-aligned validation, alias derivation/enforcement, sharing-mode merge semantics, in-memory repository persistence behind domain traits, and the single RFC 9457 error contract with `X-OAGW-Error-Source` that the whole gear shares.

- **Depends On**: `cpt-cf-oagw-feature-gear-shell`

- **Scope**:
  - `domain/services` `ControlPlaneService` (traits + impl): upstream/route/plugin CRUD, alias derivation and enforcement (`compute_derived_alias`, PSL common-suffix rules, hostname/IP rules, immutability on replace), plugin lifecycle (create immutable, list/get/delete with `plugin.in_use` 409, lazy in-process GC past 30-day retention, source retrieval), and effective-config resolution (tenant chain walk with shadowing, sharing-mode merge, enabled inheritance).
  - `domain/repo.rs` repository traits (`UpstreamRepository`, `RouteRepository`, `PluginStore`) and `infra/storage` in-memory implementations (DashMap/parking_lot indexes per `cpt-cf-oagw-db-inmemory`: `upstreams_by_id`, `upstreams_by_alias`, `routes_by_id`, `routes_by_upstream`, `plugin_store`, `plugin_ref_counts`, arc-swap effective snapshot).
  - Validation aligned to the JSON Schemas (upstream.v1 / route.v1): endpoint pool homogeneity, hostname RFC 1123, port bounds, match-rule determinism, plugin-ref resolution/bind validation, CORS credentials+wildcard rejection, rate-limit strictness under `enforce`; `additionalProperties: false` DTOs.
  - `domain/error.rs` `DomainError` (variants per the error table + `error_source`) and `api/rest/error.rs` `#[resource_error(gts_id!(...))]` mapping to `toolkit-canonical-errors`; every management response carries `X-OAGW-Error-Source: gateway`.
  - `api/rest/handlers` management handlers + `api/rest/dto.rs` upstream/route/plugin DTOs; 201/200/204/400/404/409 semantics, list envelope `{items, count}` + OData (`$filter`, `$select`, `$orderby`, `$top`/`$skip`), GTS identifier path-param normalization; per-operation permissions via the F1 extractor harness.
  - Tenant scoping at the service/repository boundary: ancestor resources invisible (404), ancestor-disable inheritance with no descendant re-enable, `oagw:*` override-permission evaluation (bind/override_auth/override_rate/add_plugins).
  - Enable/disable flag semantics at write time (default enabled; ancestor-disabled resources cannot be re-enabled). Proxy-time enforcement of disable/route-exclusion is F3.
  - Management-side input validation per `cpt-cf-oagw-nfr-input-validation` (proxy-side body/header validation is F3).
  - `contract-tenant-resolver` wiring: tenant hierarchy resolution feeds the alias chain walk and sharing evaluation.

- **Out of scope**:
  - Proxy execution, routing, forwarding, and streaming (F3).
  - Plugin trait definitions, registries, and built-in plugin implementations (F4); only bind validation and lifecycle storage live here.
  - Rate-limiter/CORS runtime components (F5); effective rate/CORS config merge resolution is produced here and consumed by F5.
  - Durable/DB-backed persistence, Redis, or any shared multi-instance state (deferred per `cpt-cf-oagw-constraint-inmemory-cp`).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [ ] `p2` - `cpt-cf-oagw-fr-upstream-pooling`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-plugin-lifecycle`
  - [ ] `p1` - `cpt-cf-oagw-fr-plugin-source`
  - [ ] `p2` - `cpt-cf-oagw-fr-config-layering`
  - [ ] `p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-fr-error-source-distinction`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`
  - [ ] `p2` - `cpt-cf-oagw-interface-mgmt-upstreams`
  - [ ] `p2` - `cpt-cf-oagw-interface-mgmt-routes`
  - [ ] `p2` - `cpt-cf-oagw-interface-mgmt-plugins`
  - [ ] `p2` - `cpt-cf-oagw-interface-error-contract`
  - [ ] `p1` - `cpt-cf-oagw-contract-tenant-resolver`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`
  - [ ] `p1` - `cpt-cf-oagw-usecase-manage-plugin`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-error-contract`
  - [ ] `p2` - `cpt-cf-oagw-principle-rfc9457`
  - [ ] `p2` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-inmemory-cp`

- **Domain Model Entities**:
  - Upstream, Route, Plugin (custom), Endpoint, ServerConfig, AuthConfig, HeadersConfig, RateLimitConfig, CorsConfig, PluginsConfig, HttpMatchConfig

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-control-plane`
  - [ ] `p2` - `cpt-cf-oagw-component-storage`

- **API**:
  - POST/GET/PUT/DELETE `/oagw/v1/upstreams(/{id})`
  - POST/GET/PUT/DELETE `/oagw/v1/routes(/{id})`
  - POST/GET/DELETE `/oagw/v1/plugins(/{id})` and GET `/oagw/v1/plugins/{id}/source`
  - All under the host-applied `/api/oagw/v1/...` prefix; OData list params; RFC 9457 problem+json error bodies

- **Sequences**:

  - [ ] `p2` - `cpt-cf-oagw-seq-upstream-create`
  - [ ] `p2` - `cpt-cf-oagw-seq-route-create`
  - [ ] `p2` - `cpt-cf-oagw-seq-plugin-lifecycle`

- **Data**:

  - [ ] `p3` - `cpt-cf-oagw-db-inmemory`

- **Acceptance (DoD)**:
  - Management CRUD integration-tested over an in-process router: 201/200/204, 400 validation, 404 ancestor-invisible, 409 alias/plugin-in-use/route-conflict, list envelope + OData.
  - Validation rules per JSON Schemas exercised (pool homogeneity, hostname/port, match-rule determinism, alias derivation/immutability, catalog-only plugin refs rejected at write time, credentials+wildcard CORS rejected).
  - Tenant-isolation tests prove zero cross-tenant reads/writes; ancestor-disable inheritance and no-re-enable verified.
  - All `DomainError` variants map 1:1 to the error table with GTS ids, retriable flags, and `X-OAGW-Error-Source: gateway`; canonical-error middleware fills `instance`/`trace_id`.
  - Effective-config resolution (chain walk, shadowing, min-merge, plugin concat, CORS union/enforce, tag add-only union) unit-tested; snapshots stamped on writes.

### 2.3 [Proxy Data Plane: Resolution, Matching, Forwarding, and Streaming](feature-proxy-data-plane/) ⏳ HIGH

- [ ] `p3` - **ID**: `cpt-cf-oagw-feature-proxy-data-plane`

- **Purpose**: Implement the request-driven proxy hot path — resolve the upstream by alias, match the route deterministically, select the target endpoint, transform/validate the request, forward outbound under timeout and circuit-breaker policy, stream SSE with lifecycle handling, and attribute error source on every response — while establishing the plugin/rate-limit/CORS execution seams the policy features (F4/F5) fill in.

- **Depends On**: `cpt-cf-oagw-feature-control-plane`

- **Scope**:
  - `domain/services` `DataPlaneService` trait + `infra/proxy` `DataPlaneServiceImpl`; proxy axum handler in `api/rest/handlers`; `domain/dto.rs` internal types (`ProxyContext`, `EffectiveUpstream`, `MatchedRoute`).
  - Alias resolution: tenant chain walk (descendant -> root, closest match wins, shadowing), enforced ancestor limits never bypassed; disabled upstream -> 503 `link.unavailable`.
  - Route matching by `(upstream, method, longest path prefix, priority)`; query-allowlist validation; `path_suffix_mode` (`disabled` rejects suffix, `append` appends); no match -> 404 `route.not_found`.
  - `X-OAGW-Target-Host` behavior matrix (absent/valid/invalid/unknown + round-robin selection) per accepted docs ADR 0001.
  - Header handling: consume routing headers without forwarding, strip hop-by-hop headers, replace `Host`/`:authority` with the upstream host, apply passthrough (none/allowlist/all) then `set`/`add`/`remove` rules, validate well-known entity headers.
  - Body validation: `Content-Length` valid + matching, 100 MB ceiling rejected before buffering (413), only `chunked` transfer encoding (else 400) — proxy side of `cpt-cf-oagw-nfr-input-validation`.
  - Outbound forwarding with hyper/hyper-util/toolkit-http: HTTPS default, plaintext only under `allow_http_upstream`, `ssrf_policy.enabled` enforcement hook, bounded connector-level retry via `tokio-retry` (never full-request replay), `proxy_timeout_secs` overall/connection/idle timeouts mapped to 504 `timeout.*`.
  - Per-upstream circuit breaker as core policy (trip at 5 failures in 30 s; open -> 503 `circuit_breaker.open` before forwarding; HALF-OPEN transition) per `cpt-cf-oagw-nfr-high-availability`.
  - SSE streaming passthrough with open/close/error lifecycle on both legs (upstream close -> close client + log; client disconnect -> close upstream).
  - Plugin-chain, rate-limit, and CORS execution seams at the fixed order (Auth -> Guards -> Transform(request) -> upstream -> Transform(response/error)) with no-op defaults; the `domain/plugin` trait family (`AuthPlugin`/`GuardPlugin`/`TransformPlugin` + contexts) is authored here as the seam contract and realized by F4; F5 wires in the rate limiter and CORS handler at their seams.
  - Proxy authorization: resolved upstream must be owned by the caller's tenant or shared by an ancestor (`proxy.v1~:invoke` permission already enforced by the F1 harness).
  - Error-source attribution on every response: gateway errors as RFC 9457 problem+json with `X-OAGW-Error-Source: gateway`; upstream responses (including errors) pass through unmodified with `X-OAGW-Error-Source: upstream`; shared error model from F2.

- **Out of scope**:
  - Plugin registries and built-in implementations (F4); rate limiter and CORS handler components (F5) — only the seams and no-op defaults live here.
  - gRPC, WebSocket, and WebTransport proxying; response caching; automatic full-request retry; Redis/distributed state — all deferred per the accepted ADRs and DESIGN future-work list.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-fr-route-matching`
  - [ ] `p1` - `cpt-cf-oagw-fr-target-host`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-fr-passthrough`
  - [ ] `p1` - `cpt-cf-oagw-fr-body-size`
  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p2` - `cpt-cf-oagw-fr-circuit-breaker`
  - [ ] `p2` - `cpt-cf-oagw-fr-timeout`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`
  - [ ] `p2` - `cpt-cf-oagw-interface-proxy-plane`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-hot-path-in-memory`
  - [ ] `p2` - `cpt-cf-oagw-principle-no-retry-cache`
  - [ ] `p2` - `cpt-cf-oagw-principle-no-retry`
  - [ ] `p2` - `cpt-cf-oagw-principle-no-cache`

- **Design Constraints Covered**:

  - [ ] `p2` - `cpt-cf-oagw-constraint-https-default`
  - [ ] `p2` - `cpt-cf-oagw-constraint-https-only`
  - [ ] `p2` - `cpt-cf-oagw-constraint-body-limit`

- **Domain Model Entities**:
  - ProxyContext, EffectiveUpstream, MatchedRoute (internal domain types consumed from F2's resolved configuration)

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-data-plane`

- **API**:
  - `* /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` (any method; `text/event-stream` passthrough)

- **Sequences**:

  - [ ] `p3` - `cpt-cf-oagw-seq-proxy-flow`
  - [ ] `p3` - `cpt-cf-oagw-seq-sse-streaming`

- **Data**:

  - [ ] `p1` - None (resolved config read through the control-plane service; DP L1 hot-config caching remains an OPTIONAL in-memory optimization, not MVP-required)

- **Acceptance (DoD)**:
  - Integration tests with `httpmock`: alias resolution with shadowing, enforced-ancestor 503, `X-OAGW-Target-Host` matrix (missing/invalid/unknown/round-robin), longest-path-prefix route matching, query allowlist, path-suffix modes, hop-by-hop stripping and host replacement, passthrough vs transform, body-rejection paths (CL mismatch, 100 MB pre-buffer 413, transfer-encoding 400).
  - Gateway errors return `application/problem+json` with GTS `type` and `X-OAGW-Error-Source: gateway`; upstream responses pass through byte-for-byte with `X-OAGW-Error-Source: upstream`.
  - SSE test verifies event forwarding and upstream-close/client-disconnect lifecycle; timeout mapping (504) and circuit-breaker open (503) fault-injection tests pass.
  - Plugin/rate-limit/CORS seams invoked in the fixed order with defaults; latency benchmark seeded for the <10 ms p95 NFR (final in F7).

### 2.4 [Plugin System: Traits, Registries, and Built-in Plugins](feature-plugin-system/) ⏳ MEDIUM

- [ ] `p4` - **ID**: `cpt-cf-oagw-feature-plugin-system`

- **Purpose**: Deliver the extensible Auth/Guard/Transform processing model as native Rust — trait contracts realized as per-type GTS-keyed registries with the full required built-in set (noop, apikey, OAuth2 client-credentials Form/Basic with token caching, required_headers, request_id) and deterministic rejection of catalog-only identifiers — wired into the data-plane seams established in F3.

- **Depends On**: `cpt-cf-oagw-feature-proxy-data-plane`

- **Scope**:
  - Finalize `domain/plugin` trait family (`AuthPlugin::authenticate`, `GuardPlugin::guard_request`/`guard_response`, `TransformPlugin::transform_request`/`transform_response`/`transform_error`; `Send + Sync`, async, stateless or internally-cached) and plugin contexts per accepted docs ADR 0002.
  - `infra/plugin` registries keyed by GTS identifier -> `Arc<dyn Trait>`: `AuthPluginRegistry::with_builtins(...)`, `GuardPluginRegistry::with_builtins()`, `TransformPluginRegistry::with_builtins()`; deterministic execution order (Auth -> Guards -> Transform(request) -> upstream -> Transform(response/error); upstream bindings before route bindings) enforced by the chain composition at the F3 seams.
  - Built-in auth: `noop`; `apikey` (injects API key into header or query from a `cred://` reference resolved via the `cred_store` client at request time); `oauth2_client_cred` (Form) and `oauth2_client_cred_basic` (Basic) per accepted docs ADR 0008 — `token_endpoint`/`issuer_url`, `client_id_ref`/`client_secret_ref`, `scopes`; internal `pingora-memory-cache` token cache keyed by `(subject_tenant_id, subject_id, auth_method, config_hash)` with a `CachedToken` key-verification wrapper (hash-collision safety); TTL = `min(config_ttl, expires_in - 30s)`; failed fetches never cached.
  - Built-in guard: `required_headers` per accepted docs ADR 0009 — presence-only, case-insensitive, independent `required_request_headers`/`required_response_headers` phases, fail-open when absent/blank, first missing header reported (request phase 400 validation, response phase 502 upstream-relative).
  - Built-in transform: `request_id` — generates/injects `X-Request-ID` upstream and echoes/relays the inbound `X-Request-Id`; declared phases request, response.
  - Catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) are NOT registered and fail resolution with an "unknown plugin" condition at binding time (consumed by F2's bind validation).
  - GTS plugin identifier constants shared with F6 type provisioning; registries constructed in the F1 gear assembly with `cred_store` client and `token_cache` config wired in.

- **Out of scope**:
  - Plugin lifecycle CRUD, immutability, `plugin.in_use`, GC, and source retrieval (F2).
  - Execution of tenant-defined custom (Starlark) plugin source, including any sandbox — explicitly NOT in the MVP per `cpt-cf-oagw-nfr-starlark-sandbox` / pipeline ADR-0003 (deliberate omission, DOC-001).
  - Retry-on-401 orchestration and event-driven token-cache invalidation (deferred per accepted docs ADR 0008); timeouts/CORS/rate-limiting/logging/metrics as bindable plugins (core data-plane capabilities).

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p2` - `cpt-cf-oagw-fr-required-headers-guard`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p2` - `cpt-cf-oagw-fr-oauth2-token-cache`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-plugin-order-determinism`

- **Design Constraints Covered**:

  - [ ] `p2` - `None` (plugin design constraints are enforced via `cpt-cf-oagw-constraint-lockfile-only` in F1; no constraint id is unique to this feature)

- **Domain Model Entities**:
  - AuthPlugin/GuardPlugin/TransformPlugin (trait contracts), PluginContext, CachedToken

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-plugin-system`

- **API**:
  - None (in-process registries and built-ins; no REST surface)

- **Sequences**:

  - [ ] `p4` - None (chain execution is exercised inside `cpt-cf-oagw-seq-proxy-flow` from F3)

- **Data**:

  - [ ] `p4` - None (token cache is runtime memory owned by the component, not control-plane data)

- **Acceptance (DoD)**:
  - Code review confirms the three trait families, per-type registries, and the full built-in set; catalog-only identifiers are not registered and fail resolution.
  - Integration tests verify deterministic execution order end-to-end through the data plane; OAuth2 token isolation per `(tenant, subject, config)`, TTL `min(ttl, expires_in - 30s)`, no caching of failed fetches; `required_headers` request (400) and response (502) phases and fail-open; `request_id` propagation; credential material never appears in logs/errors/responses.

### 2.5 [Rate Limiting and CORS](feature-rate-limit-cors/) ⏳ MEDIUM

- [ ] `p5` - **ID**: `cpt-cf-oagw-feature-rate-limit-cors`

- **Purpose**: Implement the two core data-plane policy capabilities mandated by accepted docs ADRs 0003 and 0004 — DP-owned per-instance dual-rate token buckets with hierarchical min-merge, and a built-in CORS handler with a local preflight fast path — wired into the F3 seams, with secure defaults and no distributed dependency.

- **Depends On**: `cpt-cf-oagw-feature-proxy-data-plane`

- **Scope**:
  - Rate limiter (`infra/proxy`): DP-owned, per-instance token buckets (dual-rate sustained refill `sustained.rate / window_seconds` + `burst.capacity` defaulting to `sustained.rate`; continuous refill, capacity clamp); hierarchical min-merge over enforced ancestors, upstream, route, and tenant (a descendant can never be looser than an ancestor-enforced limit); scopes `global`/`tenant`/`user`/`ip`/`route` with counter identity from `SecurityContext`/source IP/route; cost-per-request; strategies `reject` (429 + `X-RateLimit-Limit`/`Remaining`/`Reset` + `Retry-After`, retriable), `queue` (bounded), `degrade`; registry keyed by resource id (upstream/route + scope), limiters created lazily and dropped on resource/route deletion; cold-start burst accepted (`cpt-cf-oagw-principle-per-instance-state`).
  - CORS handler (`infra/proxy`): preflight detection (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) -> permissive 204 fast path answered locally (echo origin/method/request-headers; `Access-Control-Allow-Methods`, `Access-Control-Allow-Headers`, `Access-Control-Max-Age: 86400`, `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`); actual cross-origin requests validated after upstream resolution (exact, protocol-/port-sensitive origin match; method check) with 403 `cors.origin_not_allowed` / `cors.method_not_allowed`; adds `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials` and always `Vary: Origin`; configuration-time rejection of `allow_credentials: true` with wildcard origin (enforced at write time via F2 validation and mirrored here); sharing merge (origins union on `inherit`; no child additions on `enforce`); CORS disabled unless explicitly enabled.
  - Wire the rate limiter and CORS handler into the F3 seams (rate evaluation after plugin-chain resolution; CORS at the preflight seam and before forwarding); use effective rate/CORS config from the F2 resolution service.
  - Redis-backed distributed rate-limit synchronization and shared L2 caching are OPTIONAL/future and NOT in the MVP.

- **Out of scope**:
  - Rate-limit and CORS configuration models, write-time validation, and merge resolution (F2).
  - Cross-instance synchronization, persistence of counters across restarts, WebSocket/WebTransport proxying — all deferred.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-fr-cors`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`
  - [ ] `p2` - `cpt-cf-oagw-usecase-cors-preflight`

- **Design Principles Covered**:

  - [ ] `p2` - `cpt-cf-oagw-principle-per-instance-state`

- **Design Constraints Covered**:

  - [ ] `p2` - `None` (behavioral constraints are governed by the accepted docs ADRs 0003/0004; no unique constraint id applies)

- **Domain Model Entities**:
  - RateLimitConfig (in-memory TokenBucket state), CorsConfig (effective merged config consumed from F2)

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-rate-limiter`
  - [ ] `p2` - `cpt-cf-oagw-component-cors`

- **API**:
  - None (in-process components invoked at the data-plane seams)

- **Sequences**:

  - [ ] `p5` - `cpt-cf-oagw-seq-rate-limit-reject`
  - [ ] `p5` - `cpt-cf-oagw-seq-cors-preflight`

- **Data**:

  - [ ] `p5` - None (runtime in-memory state; no control-plane data layout)

- **Acceptance (DoD)**:
  - Integration tests verify 429 with `X-RateLimit-Limit`/`Remaining`/`Reset` + `Retry-After`, hierarchical `min()` merge under `enforce`, scope identities, cost, and queue/degrade strategies.
  - Integration tests verify preflight 204 fast path (echo + `Max-Age` + `Vary`), actual-request 403 for disallowed origin/method, exact origin matching, `Access-Control-*` response headers, and configuration-time rejection of `allow_credentials` with wildcard origin.
  - Limiters created lazily and dropped on resource/route deletion; cold-start burst documented and accepted.

### 2.6 [Cross-cutting: Type Provisioning (GTS), Observability, and OpenAPI](feature-cross-cutting/) ⏳ MEDIUM

- [ ] `p6` - **ID**: `cpt-cf-oagw-feature-cross-cutting`

- **Purpose**: Complete the gear's platform integration surfaces that span the whole component: register the gear's GTS types/catalog/error instances with the in-process types-registry during startup, emit the accepted OpenTelemetry metrics and structured correlation logging, and finalize the OpenAPI surface — so the gear is cataloged, observable, and discoverable as a complete host component.

- **Depends On**: `cpt-cf-oagw-feature-plugin-system`, `cpt-cf-oagw-feature-rate-limit-cors`

- **Scope**:
  - `infra/type_provisioning.rs`: register GTS type schemas backing the upstream/route JSON Schemas, the protocol identifiers, the built-in and catalog-only plugin identifiers (`cf.core.oagw.noop.v1` ... `metrics.v1`), and error instances via the types-registry SDK (`dyn TypesRegistryClient`); invoked during `Gear::init` startup provisioning; shares plugin identifier constants with F4.
  - Observability: OpenTelemetry counters/histograms per the accepted metrics vocabulary — request counts/latencies by host (alias) and normalized route/method/status, error counters by `error_type`, rate-limit exceed counters, circuit-breaker transitions/state gauge, upstream health gauges, target-host/endpoint-selection counters; structured JSON logs with the allowed field set (no bodies, no query params, no headers except allowlisted, no credentials); correlation via `request_id`/`trace_id` in gateway error problems.
  - OpenAPI: complete `utoipa` annotations on DTOs and routes so the management/proxy surface is fully documented in the host OpenAPI registry.
  - Final gear assembly verification: `Gear::init` constructs and wires every collaborator from F2-F5 (repositories, registries, rate-limit registry, services) behind the F1 `OnceLock`s; startup/shutdown participation with no background task.

- **Out of scope**:
  - Registry-only deployment mode; event-driven token-cache invalidation; DNS/IP-pinning rule implementation; documented future work items from the DESIGN (DB repos, Redis sync, Starlark execution, WebSocket/WebTransport/gRPC proxying).

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`
  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`

- **Design Principles Covered**:

  - [ ] `p2` - `None` (no principle id is unique to this feature; it completes surfaces governed by F1-F5 principles)

- **Design Constraints Covered**:

  - [ ] `p2` - `None` (constraints are realized in their owning features F1-F3)

- **Domain Model Entities**:
  - Registered GTS type/catalog/error instances (upstream/route types, protocol ids, plugin catalog ids, error problem types)

- **Design Components**:

  - [ ] `p2` - `cpt-cf-oagw-component-type-provisioning`

- **API**:
  - None (startup provisioning + metrics emission; no new endpoints)

- **Sequences**:

  - [ ] `p6` - None

- **Data**:

  - [ ] `p6` - None

- **Acceptance (DoD)**:
  - Type provisioning runs at startup and is verifiable through the types-registry client (upstream/route types, protocol ids, full plugin catalog including catalog-only ids, error instances).
  - Metrics counters emitted and asserted in integration tests; log-field policy verified (no bodies/query/creds); correlation IDs present on proxy requests and gateway problems.
  - OpenAPI registry exposes the complete management + proxy surface; full gear assembly under e2e features verified end-to-end.

### 2.7 [Test and Verification Suite](feature-tests/) ⏳ LOW

- [ ] `p7` - **ID**: `cpt-cf-oagw-feature-tests`

- **Purpose**: Deliver the automated verification that gates the gear's acceptances — crate-root integration tests over an in-process router, in-crate unit tests, and the e2e acceptance suite under `testing/e2e/gears/oagw` — proving every PRD acceptance criterion and the >=90% coverage NFR for the assembled gear.

- **Depends On**: `cpt-cf-oagw-feature-cross-cutting`

- **Scope**:
  - Crate-root integration tests (`tests/`, `test-utils` feature, `httpmock` dev-dependency): management CRUD semantics (201/200/204/400/404/409, tenant scoping, list envelope, OData), proxy forwarding to a mocked upstream (alias shadowing, target-host matrix, route matching, passthrough vs transform headers, SSE streaming lifecycle, error-source header on every response, gateway problem+json bodies), plugin chain order, rate-limit and CORS behaviors.
  - In-crate unit tests per the DESIGN testing strategy: alias derivation/enforcement transitions, route-match determinism, token-bucket/rate-limit min-merge, header-category processing, body-validation rules, plugin semantics (required_headers fail-open, request_id, oauth2 cache key/TTL, catalog-only resolution failure), error-table mapping (`DomainError` -> `CanonicalError`), config defaults.
  - E2E acceptance suite under `testing/e2e/gears/oagw` (the gear's own tests do not live there): PRD acceptance criteria, latency benchmark (<10 ms added p95), fault-injection (breaker trips within 5 failures/30 s), SSRF negative tests, log assertions for zero credential exposure.
  - CI coverage gate enforcing >=90% automated coverage across unit/integration/e2e.
  - Verification-only feature: contains no product behavior; it proves the acceptances of F1-F6.

- **Out of scope**:
  - Any product behavior, feature implementation, or new dependency; e2e suite authoring for other gears; manual/ops procedures.

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-nfr-testability`

- **Design Principles Covered**:

  - [ ] `p2` - `None` (verification of principles is delegated to the owning features' tests)

- **Design Constraints Covered**:

  - [ ] `p2` - `None`

- **Domain Model Entities**:
  - None (test fixtures and mocked upstreams only)

- **Design Components**:

  - [ ] `p2` - `None` (no new design components; exercises all F1-F6 components)

- **API**:
  - None (test harness surfaces only)

- **Sequences**:

  - [ ] `p7` - None (all sequences are exercised through the owning features' tests)

- **Data**:

  - [ ] `p7` - None

- **Acceptance (DoD)**:
  - Full unit + integration + e2e suite green under the workspace e2e feature set; measured coverage >=90% gated in CI.
  - Every PRD acceptance criterion from the PRD §9 holds: CRUD semantics, proxy semantics with error-source distinction, disable semantics, rate limiting with retry guidance, CORS contract, SSE lifecycle, zero credential exposure, and the coverage threshold.

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-shell
    ↓
cpt-cf-oagw-feature-control-plane
    ↓
cpt-cf-oagw-feature-proxy-data-plane
    ↓
    ├─→ cpt-cf-oagw-feature-plugin-system
    └─→ cpt-cf-oagw-feature-rate-limit-cors
            ↓
            └─→ cpt-cf-oagw-feature-cross-cutting
                    ↓
                    cpt-cf-oagw-feature-tests
```
(with `cpt-cf-oagw-feature-cross-cutting` also depending on `cpt-cf-oagw-feature-plugin-system` for shared plugin identifier constants)

**Dependency Rationale**:

- `cpt-cf-oagw-feature-control-plane` requires `cpt-cf-oagw-feature-gear-shell`: the gear declaration, config loading, route mounting, extractor harness, and module tree must exist before any management handler or service can be authored.
- `cpt-cf-oagw-feature-proxy-data-plane` requires `cpt-cf-oagw-feature-control-plane`: alias/route resolution and the effective-config merge run through the control-plane service, and the proxy error model reuses the F2 `DomainError` -> `CanonicalError` contract.
- `cpt-cf-oagw-feature-plugin-system` and `cpt-cf-oagw-feature-rate-limit-cors` both require `cpt-cf-oagw-feature-proxy-data-plane`: F3 defines and invokes the plugin/rate-limit/CORS execution seams these features fill; F4 additionally consumes F2's bind-validation flow and the F1 gear assembly's registries.
- `cpt-cf-oagw-feature-plugin-system` and `cpt-cf-oagw-feature-rate-limit-cors` are independent of each other and can be developed in parallel once F3 lands (loose coupling per DEP-002); the linear numbering reflects the canonical build order only.
- `cpt-cf-oagw-feature-cross-cutting` requires `cpt-cf-oagw-feature-plugin-system` (shared GTS plugin identifier constants for type provisioning) and `cpt-cf-oagw-feature-rate-limit-cors` (all components present before the final gear assembly and metrics wiring are verified).
- `cpt-cf-oagw-feature-tests` requires `cpt-cf-oagw-feature-cross-cutting`: the e2e suite and coverage gate can only run against the fully assembled gear.
- Shared-element note (per EXC-003): `cpt-cf-oagw-interface-error-contract` is authored in F2 (error model + management mapping); F3 enforces proxy-side error-source attribution against that model — a documented dependency, not duplicated scope. The `domain/plugin` traits are authored in F3 as the data-plane seam and implemented by F4 — the only intentional design-element sharing, recorded in Section 1.

## 4. Coverage Matrix

**Design elements -> Features** (every element in exactly one feature, except the two documented shares):

| Design element (ID) | Feature |
|---|---|
| `cpt-cf-oagw-design-gear`, `cpt-cf-oagw-tech-layers`, `cpt-cf-oagw-tech-dependencies`, `cpt-cf-oagw-topology-single-exec` | F1 |
| `cpt-cf-oagw-component-gear`, `cpt-cf-oagw-component-config` | F1 |
| `cpt-cf-oagw-principle-cp-dp-separation`, `cpt-cf-oagw-principle-ddd-light` | F1 |
| `cpt-cf-oagw-constraint-lockfile-only`, `cpt-cf-oagw-constraint-single-exec`, `cpt-cf-oagw-constraint-toolkit-deploy` | F1 |
| `cpt-cf-oagw-component-control-plane`, `cpt-cf-oagw-component-storage` | F2 |
| `cpt-cf-oagw-interface-mgmt-upstreams`, `cpt-cf-oagw-interface-mgmt-routes`, `cpt-cf-oagw-interface-mgmt-plugins` | F2 |
| `cpt-cf-oagw-interface-error-contract` | F2 (enforced by F3 on the proxy path — documented share) |
| `cpt-cf-oagw-principle-error-contract`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source` | F2 |
| `cpt-cf-oagw-constraint-inmemory-cp` | F2 |
| `cpt-cf-oagw-db-inmemory` | F2 |
| `cpt-cf-oagw-seq-upstream-create`, `cpt-cf-oagw-seq-route-create`, `cpt-cf-oagw-seq-plugin-lifecycle` | F2 |
| `cpt-cf-oagw-component-data-plane`, `cpt-cf-oagw-interface-proxy-plane` | F3 |
| `cpt-cf-oagw-principle-hot-path-in-memory`, `cpt-cf-oagw-principle-no-retry-cache`, `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-no-cache` | F3 |
| `cpt-cf-oagw-constraint-https-default`, `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-body-limit` | F3 |
| `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-seq-sse-streaming` | F3 |
| `cpt-cf-oagw-component-plugin-system` | F4 |
| `cpt-cf-oagw-principle-plugin-order-determinism` | F4 |
| `cpt-cf-oagw-component-rate-limiter`, `cpt-cf-oagw-component-cors` | F5 |
| `cpt-cf-oagw-principle-per-instance-state` | F5 |
| `cpt-cf-oagw-seq-rate-limit-reject`, `cpt-cf-oagw-seq-cors-preflight` | F5 |
| `cpt-cf-oagw-component-type-provisioning` | F6 |
| `domain/plugin` trait family (shared: authored F3, implemented F4) | F3 + F4 (documented share) |

**Requirements -> Features** (PRD FR/NFR/interface/contract/usecase ids):

| Requirement (ID) | Feature(s) |
|---|---|
| `cpt-cf-oagw-fr-gear-registration` | F1 |
| `cpt-cf-oagw-nfr-build-constraints` | F1 |
| `cpt-cf-oagw-interface-host-registration`, `cpt-cf-oagw-contract-authn-authz` | F1 |
| `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-enable-disable`, `cpt-cf-oagw-fr-upstream-pooling` | F2 (DP enforcement of disable/route-exclusion under F3 scope) |
| `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-plugin-lifecycle`, `cpt-cf-oagw-fr-plugin-source` | F2 |
| `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config` | F2 (resolution; consumed F3/F5) |
| `cpt-cf-oagw-fr-error-codes`, `cpt-cf-oagw-fr-error-source-distinction` | F2 (model; enforced on proxy responses in F3/F5) |
| `cpt-cf-oagw-nfr-multi-tenancy` | F2 (repository boundary) |
| `cpt-cf-oagw-nfr-input-validation` | F2 (management DTOs) + F3 (proxy body/header) |
| `cpt-cf-oagw-interface-management-api`, `cpt-cf-oagw-contract-tenant-resolver` | F2 |
| `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`, `cpt-cf-oagw-usecase-manage-plugin` | F2 |
| `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-route-matching`, `cpt-cf-oagw-fr-target-host` | F3 |
| `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-passthrough`, `cpt-cf-oagw-fr-body-size` | F3 |
| `cpt-cf-oagw-fr-streaming`, `cpt-cf-oagw-fr-circuit-breaker`, `cpt-cf-oagw-fr-timeout` | F3 |
| `cpt-cf-oagw-nfr-low-latency`, `cpt-cf-oagw-nfr-high-availability`, `cpt-cf-oagw-nfr-ssrf-protection` | F3 |
| `cpt-cf-oagw-interface-proxy-api` | F3 |
| `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-sse-streaming` | F3 |
| `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-fr-required-headers-guard` | F4 |
| `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-oauth2-token-cache` | F4 |
| `cpt-cf-oagw-nfr-credential-isolation` | F4 (resolution/token handling; zero-exposure asserts in F7) |
| `cpt-cf-oagw-contract-cred-store` | F4 |
| `cpt-cf-oagw-fr-rate-limiting`, `cpt-cf-oagw-fr-cors` | F5 |
| `cpt-cf-oagw-usecase-rate-limit-exceeded`, `cpt-cf-oagw-usecase-cors-preflight` | F5 |
| `cpt-cf-oagw-contract-types-registry`, `cpt-cf-oagw-nfr-observability` | F6 |
| `cpt-cf-oagw-nfr-testability` | F7 |
| `cpt-cf-oagw-nfr-starlark-sandbox` | Deliberately NOT covered (DOC-001): Starlark custom-plugin execution and sandboxing are explicitly deferred by pipeline ADR-0003 and accepted docs ADR 0002; the DESIGN NFR allocation marks this N/A in the MVP. Custom-plugin lifecycle/source (F2) ships; no sandbox runtime is introduced. |

## 5. Build and Stage Ordering

The build proceeds in four stages with stage-exit conditions, each stage corresponding to a deplorable increment of the gear:

1. **Foundation (F1)** — gear shell, config, registration, module tree. Exit: gear boots under e2e features and mounts the `/oagw/v1` route table; lockfile unchanged.
2. **Control plane (F2)** — management CRUD, storage, error contract, effective-config resolution. Exit: full management API working in-process against the types-registry pattern; the configuration substrate the data plane depends on exists.
3. **Data plane + policies (F3, then F4/F5 in parallel)** — proxy execution with seams (F3), then the plugin system (F4) and rate limiting/CORS (F5) as independent branches off those seams. Exit: end-to-end proxying with plugins, rate limits, and CORS verified against a mocked upstream.
4. **Cross-cutting + verification (F6, F7)** — type provisioning, observability, OpenAPI, final assembly, then the acceptance suite and coverage gate. Exit: all PRD acceptance criteria hold; >=90% coverage gated in CI.

Rationale: infrastructure and state-bearing surfaces are built before the behavior that reads them (F1 before F2 before F3); policy components are layered onto the request path only after the request path's seams exist (F4/F5 after F3) so each work item is independently testable at its stage; platform integration (F6) and acceptance (F7) are deliberately last because they need the fully assembled gear. Ordering also minimizes rework: the error contract (F2) and effective-config resolution (F2) are fixed once and reused by all three later stages, and the F4/F5 parallel branch shortens the critical path.

## 6. Traceability

- **PRD**: [PRD.md](./PRD.md) — requirements ids reused verbatim: every `cpt-cf-oagw-fr-*`, `cpt-cf-oagw-nfr-*`, `cpt-cf-oagw-interface-*`, `cpt-cf-oagw-contract-*`, and `cpt-cf-oagw-usecase-*` id is allocated to exactly one feature (or a documented pair) in Sections 2 and 4, with no orphaned requirements.
- **DESIGN**: [DESIGN.md](./DESIGN.md) — design-side ids reused verbatim: all `cpt-cf-oagw-component-*` (9), `cpt-cf-oagw-interface-mgmt-*`/`-proxy-plane`/`-error-contract`, `cpt-cf-oagw-seq-*` (7), `cpt-cf-oagw-db-inmemory`, `cpt-cf-oagw-principle-*`, `cpt-cf-oagw-constraint-*`, `cpt-cf-oagw-tech-*`, `cpt-cf-oagw-design-gear`, and `cpt-cf-oagw-topology-single-exec` are mapped in Section 4; forward links are listed per feature in Section 2.
- **ADRs**: pipeline ADRs [0001](./ADR/0001-oagw-gear-architecture.md), [0002](./ADR/0002-proxy-data-plane.md), [0003](./ADR/0003-plugins-rate-limit-cors.md) (`cpt-cf-oagw-adr-*`) and the gear's accepted docs ADRs `docs/ADR/0001..0009` ground the feature boundaries (F1 = registration/assembly per ADR-0001; F3 = in-crate proxy + error semantics per ADR-0002; F4/F5 = built-ins, DP-owned token buckets, built-in CORS per ADR-0003 and docs ADRs 0002/0003/0004/0008/0009).
- **Features**: each entry in Section 2 will be elaborated as its own FEATURE artifact under `artifacts/features/` (`cf-sdlc-doc-feature`), reusing the feature ids `cpt-cf-oagw-feature-*` defined here; those artifacts will trace back to this DECOMPOSITION and to the same DESIGN/PRD ids.
- **Artifact schema**: `[artifacts.DECOMPOSITION]` per `artifacts.toml` (path `gears/system/oagw/artifacts/DECOMPOSITION.md`, kind `DECOMPOSITION`, traceability FULL); status-overall id `cpt-cf-oagw-status-overall` unchecks until all seven `cpt-cf-oagw-feature-*` checkboxes are complete.
