# Decomposition: OAGW Gear Implementation


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 OAGW Gear Foundation - HIGH](#21-oagw-gear-foundation---high)
  - [2.2 OAGW Control Plane - MEDIUM](#22-oagw-control-plane---medium)
  - [2.3 OAGW Data Plane - MEDIUM](#23-oagw-data-plane---medium)
  - [2.4 OAGW GTS Type Provisioning - LOW](#24-oagw-gts-type-provisioning---low)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

## 1. Overview

The OAGW gear implementation DESIGN is decomposed into four ordered FEATURE work packages, grouped by functional cohesion along the Control-Plane / Data-Plane separation that the DESIGN and ADR 0010 already establish. Each feature is an implementable, independently assignable, and incrementally deliverable unit; features are ordered dependency-acyclically (foundation first, then sibling planes, then the cross-cutting provisioning step), and every DESIGN element and every pipeline PRD FR/NFR is assigned to exactly one feature.

**Decomposition Strategy**:
- **Foundation first** — `feature-gear-foundation` (HIGH) turns the empty `src/lib.rs` into a registered, configured, healthy gear: the `#[toolkit::gear]` declaration, lifecycle wiring, `OagwConfig`, gear-relative route mounting, and readiness/health contribution. It alone owns the crate-wide build/toolchain/dependency constraints and the startup + health sequences, because those concerns are what make every other feature compilable and reachable.
- **Sibling planes** — `feature-control-plane` (MEDIUM) owns the management surface, the `ControlPlaneService` CRUD/hierarchy/caching behavior, the REST surface component, and the entire persistence model (`db-control-plane` plus all seven `oagw_*` tables). `feature-data-plane` (MEDIUM) owns the pingora-based proxy engine, the credential/rate-limit/plugin/CORS/security/error-source behavior, and the proxy-call sequence. The two depend only on the foundation and can be developed in parallel, because the DESIGN mandates that all inter-module communication goes through versioned traits and SDK clients (DESIGN §3.4) rather than internal types.
- **Cross-cutting provisioning** — `feature-type-provisioning` (LOW) owns GTS catalog registration in `infra/type_provisioning.rs`: small, self-contained, invoked from inside `Gear::init`, and useful to both planes through identifier validation.
- **Ordering** — each feature declares an explicit `Depends On` (DAG in §3); foundation features have no dependencies; priority markers `p1`–`p3` follow the dependency order.

**Coverage and exclusivity notes (deliberate, not omissions)**:
- All 6 components, all 5 principles, all 5 constraints, all 4 sequences, `cpt-cf-oagw-db-control-plane`, all 7 dbtables, and `cpt-cf-oagw-interface-oagw-rest-surface` are assigned to exactly one feature each. No design element is double-assigned.
- Two deliberately shared requirement-surface IDs: `cpt-cf-oagw-nfr-test-coverage` is a crate-level DoD NFR and is intentionally carried by both `feature-gear-foundation` (establishes the crate-level test harness and the no-encroachment rule on `testing/e2e/gears/oagw/`) and `feature-data-plane` (the largest behavioral surface under test). `cpt-cf-oagw-usecase-startup-registration` is anchored in `feature-gear-foundation`; the GTS-registration step of that use case is delivered by `feature-type-provisioning`, so the use case ID is referenced in both with this documented reason.
- `cpt-cf-oagw-component-rest-surface` is owned by `feature-control-plane`; the proxy route it also registers (`/oagw/v1/proxy/...`) is a runtime integration point consumed by `feature-data-plane`, not duplicated scope.
- Deliberate exclusions inherited from PRD §4.2 and DESIGN §4 (custom Starlark plugin execution, distributed/Redis-backed rate limiting, the L2 config-cache tier, response caching, automatic retries, gRPC proxying, DNS/TLS/mTLS/HTTP3 implementation, and authoring the acceptance suite) are out of scope for every feature and are not reassigned.

## 2. Entries

**Overall implementation status:**

- [x] `p1` - **ID**: `cpt-cf-oagw-status-overall`

### 2.1 [OAGW Gear Foundation](feature-gear-foundation/) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Make the empty `oagw` crate a live, configured, healthy gear: declare `OagwGear` through the platform `#[toolkit::gear(...)]` macro, implement the complete lifecycle (config load, DB slot, SDK clients, migrations and route registration, start/stop), read and validate the host-injected `OagwConfig` with safe defaults, and contribute readiness to the host health endpoint so the DoD build/start/health gates hold.

- **Depends On**: None

- **Scope**:
  - `OagwGear` declaration with `name = "oagw"`, `deps = [authz_resolver, types_registry, tenant_resolver, credstore]`, `capabilities = [stateful, rest]`; inventory-based registration (`toolkit::inventory::submit!`) so the host discovers and links the gear behind the `oagw` feature.
  - `Gear::init` wiring: `ctx.config_or_default::<OagwConfig>()` (persistence-free MVP per ADR 0010 — no `db` capability, no `ctx.db_required()`), `-sdk` client resolution via `ctx.client_hub()`; startup validation with loud failure on invalid configuration.
  - `OagwConfig` serde model (`deny_unknown_fields`, all fields defaulted): `proxy_timeout_secs` (2), `allow_http_upstream` (false), `ssrf_policy.enabled` (true), additive ADR 0008 keys `token_cache_ttl_secs` (300) and `token_cache_capacity` (10 000); derived settings threaded into the data plane.
  - Gear-relative route registration (no `/api` segment) via `RestApiCapability::register_rest`; the persistence-free control-plane repository (no `DatabaseCapability::migrations` — ADR 0010 persistence-free MVP clause); `RunnableCapability::{start, stop}` for the data-plane service; readiness reported only after the data plane starts.
  - Crate-level test harness: registration/lifecycle/config-parsing/defaults/migrations/route-mounting unit and integration tests, with the acceptance path `testing/e2e/gears/oagw/` deliberately untouched.

- **Out of scope**:
  - Management CRUD, hierarchy computation, and persistence of the `oagw_*` tables (feature-control-plane).
  - Outbound proxy, streaming, plugin execution, rate limiting, and SSRF enforcement behavior (feature-data-plane).
  - GTS catalog content and registration implementation (feature-type-provisioning).
  - Custom Starlark execution, distributed rate limiting, L2 caching, response caching, retries, gRPC proxying (PRD §4.2).

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-oagw-fr-gear-registration`
  - [x] `p1` - `cpt-cf-oagw-fr-gear-lifecycle`
  - [x] `p1` - `cpt-cf-oagw-fr-gear-configuration`
  - [x] `p1` - `cpt-cf-oagw-fr-route-serving`
  - [x] `p1` - `cpt-cf-oagw-nfr-build-integration`
  - [x] `p1` - `cpt-cf-oagw-nfr-startup-health`
  - [x] `p1` - `cpt-cf-oagw-nfr-test-coverage`
  - [x] `p1` - `cpt-cf-oagw-contract-host-runtime`
  - [x] `p1` - `cpt-cf-oagw-usecase-startup-registration`
  - [x] `p1` - `cpt-cf-oagw-usecase-health-reporting`

- **Design Principles Covered**:

  - [x] `p2` - `cpt-cf-oagw-principle-gear-relative-routing`
  - [x] `p2` - `cpt-cf-oagw-principle-fail-loud-config`

- **Design Constraints Covered**:

  - [x] `p2` - `cpt-cf-oagw-constraint-workspace-lints`
  - [x] `p2` - `cpt-cf-oagw-constraint-toolchain`
  - [x] `p2` - `cpt-cf-oagw-constraint-locked-deps`
  - [x] `p2` - `cpt-cf-oagw-constraint-no-api-segment`
  - [x] `p2` - `cpt-cf-oagw-constraint-authoritative-immutable`

- **Domain Model Entities**:
  - OagwGear (gear root and lifecycle state)
  - OagwConfig (validated runtime configuration model)

- **Design Components**:

  - [x] `p2` - `cpt-cf-oagw-component-gear-root`
  - [x] `p2` - `cpt-cf-oagw-component-config`
  - [x] `p3` - `cpt-cf-oagw-topology-e2e-single-exec`

- **API**:
  - GET /healthz on :8086 (host-composed) — readiness/health contribution
  - Gear-relative route roots mounted via `register_rest` (actual management/proxy endpoints are owned by feature-control-plane and feature-data-plane)

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-startup-registration`
  - `p1` - `cpt-cf-oagw-seq-health-reporting`

- **Data**:
  - None — persistence (`cpt-cf-oagw-db-control-plane` and the `oagw_*` tables) is owned by feature-control-plane; this feature wires the persistence-free control-plane repository (no `DatabaseCapability::migrations` surface — ADR 0010 persistence-free MVP clause).

### 2.2 [OAGW Control Plane](feature-control-plane/) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-control-plane`

- **Purpose**: Deliver the configuration backbone of the gateway: the REST management surface and the tenant-scoped, validated, hierarchy-aware CRUD for upstreams, routes, and plugins, persisted across the `oagw_*` tables with in-memory L1 configuration caching and explicit invalidation on every successful write.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - REST management surface (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`) registered with `OperationBuilder`; DTO/extractor mapping, inbound validation, and RFC 9457 problem-details error mapping with the error-source header.
  - `ControlPlaneService` CRUD with authoritative validation, alias derivation/enforcement, enable/disable with ancestor-disable propagation, tenant scoping (ancestor resources not addressable), plugin immutability with in-use deletion protection, and atomic multi-table writes.
  - Hierarchical configuration computation: sharing modes (`private`/`inherit`/`enforce`), merge rules (rate stricter-wins, plugin concatenation, CORS origin union, tags add-only union), and alias shadowing via the tenant-resolver SDK with enforced ancestor constraints.
  - SeaORM repositories over the seven `oagw_*` tables (multi-backend, tenant-scoped); CP L1 cache ownership (10 000 entries) and invalidation of affected CP entries plus DP L1 signals on every successful write.
  - Inbound authorization for management operations via the authz-resolver SDK (exact per-actor/operation permissions per `cpt-cf-oagw-fr-inbound-authz`).

- **Out of scope**:
  - Outbound proxying, credential injection, rate-limit enforcement, plugin execution, CORS enforcement, and streaming (feature-data-plane).
  - Gear lifecycle, configuration model, and route mounting (feature-gear-foundation).
  - Persistence of secret values — no secret material is stored by the control plane.

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-oagw-fr-control-plane-crud`
  - [x] `p1` - `cpt-cf-oagw-fr-config-hierarchy`
  - [x] `p2` - `cpt-cf-oagw-fr-config-caching`
  - [x] `p1` - `cpt-cf-oagw-usecase-manage-upstream`

- **Design Principles Covered**:
  - None — the authoritative state-ownership principle `cpt-cf-oagw-principle-cp-authoritative-dp-bounded` is assigned to feature-data-plane; its control-plane half (validate → persist → invalidate before the data plane observes a new write) is captured in the Scope above.

- **Design Constraints Covered**:
  - None — the crate-level constraints (workspace lints, toolchain, locked deps, no `/api` segment, authoritative immutability) are owned by feature-gear-foundation and apply transitively to this feature's module layout (`api/rest`, `domain`, `infra/storage`).

- **Domain Model Entities**:
  - Upstream
  - Route
  - Plugin
  - ServerConfig / Endpoint
  - EffectiveConfig (computed on the CP side via the tenant-hierarchy walk before consumption by the data plane)

- **Design Components**:

  - [x] `p2` - `cpt-cf-oagw-component-rest-surface`
  - [x] `p2` - `cpt-cf-oagw-component-control-plane`
  - [x] `p2` - `cpt-cf-oagw-interface-oagw-rest-surface`

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

- **Sequences**:

  - `p2` - `cpt-cf-oagw-seq-management-write`

- **Data**:

  - [x] `p3` - `cpt-cf-oagw-db-control-plane`
  - `p2` - `cpt-cf-oagw-dbtable-upstream`
  - `p2` - `cpt-cf-oagw-dbtable-route`
  - `p2` - `cpt-cf-oagw-dbtable-route-http-match`
  - `p2` - `cpt-cf-oagw-dbtable-plugin`
  - `p2` - `cpt-cf-oagw-dbtable-upstream-plugin`
  - `p2` - `cpt-cf-oagw-dbtable-route-plugin`
  - `p2` - `cpt-cf-oagw-dbtable-config`

### 2.3 [OAGW Data Plane](feature-data-plane/) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-data-plane`

- **Purpose**: Implement the gateway's core value proposition — proxying requests to external services over the pooled, load-balanced, streaming pingora transport, applying credential injection, rate limiting, plugin-chain execution, CORS enforcement, outbound security policy, and gateway-vs-upstream error attribution on every response.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - Pingora proxy bridge in `infra/proxy` (`pingora-proxy`/`core`/`load-balancing`/`http`): request mapping, hop-by-hop header handling, streaming (HTTP/SSE/WebSocket), connection pooling, load balancing, multi-endpoint selection, `X-OAGW-Target-Host` handling, and no automatic full-request retries (connector-level failover only).
  - Alias resolution (tenant-hierarchy walk with shadowing), route matching, effective-config layering (upstream < route < tenant), plugin-chain execution (Auth → Guards → Transform request → upstream → Transform response/error), credential injection via the credstore SDK with `pingora-memory-cache` token cache, token-bucket rate limiting, CORS enforcement, SSRF policy enforcement, and `X-OAGW-Error-Source` wiring per ADR 0007.
  - DP L1 config cache (1 000 entries), in-memory rate limiters, and `RunnableCapability::{start, stop}` for the proxy runtime and sockets.
  - Structured request logging with correlation IDs and Prometheus metrics per the authoritative vocabulary (no PII, no secrets, bounded cardinality).
  - Security baseline preserved regardless of e2e allowances: HTTPS-only default posture and SSRF-enabled default exercised by security-focused tests.

- **Out of scope**:
  - Management CRUD, hierarchy computation, and persistence (feature-control-plane).
  - Upstream-response caching, distributed/L2 caching, automatic retries, custom Starlark plugin execution, and gRPC proxying.
  - Secret persistence — secret material exists only transiently in memory, never in logs, errors, responses, metrics, or cache keys.

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-oagw-fr-data-plane-proxy`
  - [x] `p1` - `cpt-cf-oagw-fr-credential-injection`
  - [x] `p1` - `cpt-cf-oagw-fr-rate-limit-enforcement`
  - [x] `p2` - `cpt-cf-oagw-fr-stream-proxying`
  - [x] `p2` - `cpt-cf-oagw-fr-plugin-execution`
  - [x] `p2` - `cpt-cf-oagw-fr-cors-enforcement`
  - [x] `p1` - `cpt-cf-oagw-fr-error-source-semantics`
  - [x] `p1` - `cpt-cf-oagw-fr-inbound-authz`
  - [x] `p1` - `cpt-cf-oagw-fr-security-policy`
  - [x] `p1` - `cpt-cf-oagw-fr-secret-resolution`
  - [x] `p1` - `cpt-cf-oagw-nfr-proxy-overhead`
  - [x] `p1` - `cpt-cf-oagw-nfr-availability`
  - [x] `p1` - `cpt-cf-oagw-nfr-concurrency-safety`
  - [x] `p1` - `cpt-cf-oagw-nfr-secret-hygiene`
  - [x] `p1` - `cpt-cf-oagw-nfr-ssrf-safety`
  - [x] `p2` - `cpt-cf-oagw-nfr-observability-metrics`
  - [x] `p1` - `cpt-cf-oagw-nfr-test-coverage`
  - [x] `p1` - `cpt-cf-oagw-usecase-proxy-call`

- **Design Principles Covered**:

  - [x] `p2` - `cpt-cf-oagw-principle-pingora-reuse`
  - [x] `p2` - `cpt-cf-oagw-principle-cp-authoritative-dp-bounded`
  - [x] `p2` - `cpt-cf-oagw-principle-ssrf-defense-in-depth`

- **Design Constraints Covered**:
  - None — the crate-level constraints owned by feature-gear-foundation apply transitively: the locked dependency set bounds the data plane to the declared pingora surface, workspace lints and the toolchain govern the bridge code, and the no-`/api` rule governs the proxy route root.

- **Domain Model Entities**:
  - EffectiveConfig (DP-owned L1 cached read view; never persisted)
  - Consumed read-only through resolved configuration (Upstream, Route, Plugin, ServerConfig/Endpoint — no management writes from this feature)

- **Design Components**:

  - [x] `p2` - `cpt-cf-oagw-component-data-plane`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}/{*path}

- **Sequences**:

  - `p2` - `cpt-cf-oagw-seq-proxy-call`

- **Data**:
  - None — no DB persistence (the control plane is authoritative); state is bounded to the in-memory DP L1 config cache and the in-memory rate limiters.

### 2.4 [OAGW GTS Type Provisioning](feature-type-provisioning/) - LOW

- [x] `p3` - **ID**: `cpt-cf-oagw-feature-type-provisioning`

- **Purpose**: Register the OAGW GTS type catalog (upstream, route, and plugin base types plus the built-in named-plugin identifiers, including catalog-only identifiers) with the types registry and provide identifier validation helpers, so plugin and protocol identifiers used across the API resolve consistently platform-wide.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`

- **Scope**:
  - GTS catalog registration via the types-registry SDK during `Gear::init` (`infra/type_provisioning.rs`): upstream/route/plugin base types, the built-in auth/guard/transform plugin identifiers, and catalog-only identifiers.
  - Identifier validation helpers consumed by the control-plane CRUD and the plugin registries.

- **Out of scope**:
  - Owning the types registry or seeding the process-wide GTS inventory (the types-registry gear's `system` role).
  - Custom Starlark plugin execution (PRD §4.2).

- **Requirements Covered**:

  - [x] `p1` - `cpt-cf-oagw-contract-gts-registry`
  - [x] `p1` - `cpt-cf-oagw-usecase-startup-registration`

- **Design Principles Covered**:
  - None — this feature follows, rather than owns, the principles assigned to other features.

- **Design Constraints Covered**:
  - None — the authoritative-immutability and locked-deps constraints (owned by feature-gear-foundation) bound the catalog to the contract's identifiers and the declared dependency set.

- **Domain Model Entities**:
  - GTS catalog entries (upstream/route/plugin base types; built-in named-plugin and catalog-only identifiers)

- **Design Components**:

  - [x] `p2` - `cpt-cf-oagw-component-type-provisioning`
  - [x] `p3` - `cpt-cf-oagw-tech-oagw-stack`

- **API**:
  - None (in-process types-registry SDK calls during `Gear::init`; no HTTP surface of its own)

- **Sequences**:
  - None (GTS catalog registration is a step inside `cpt-cf-oagw-seq-startup-registration`, whose owning feature is feature-gear-foundation; it executes during init per the DESIGN)

- **Data**:
  - None (no DB tables; the catalog lives in the types registry via the SDK)

## 3. Feature Dependencies

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    ├─→ cpt-cf-oagw-feature-control-plane
    ├─→ cpt-cf-oagw-feature-data-plane
    └─→ cpt-cf-oagw-feature-type-provisioning
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-control-plane` requires `cpt-cf-oagw-feature-gear-foundation`: `Gear::init` (config load, persistence-free control-plane repository construction, SDK client resolution) and the `RestApiCapability::register_rest` wiring must exist before `ControlPlaneService` can serve management CRUD over the mounted management router (persistence-free MVP per ADR 0010 — the `oagw_*` table semantics are reflected by the in-memory repository).
- `cpt-cf-oagw-feature-data-plane` requires `cpt-cf-oagw-feature-gear-foundation`: the data-plane service is started/stopped by `RunnableCapability` and its proxy router is registered by `register_rest` — both provided by the gear-root lifecycle — and `OagwConfig` (proxy timeout, allowances, SSRF policy, token-cache settings) is loaded and validated by the foundation and threaded into `DataPlaneService`.
- `cpt-cf-oagw-feature-type-provisioning` requires `cpt-cf-oagw-feature-gear-foundation`: the GTS catalog registration is invoked from inside `Gear::init`, so it only exists once the lifecycle has been implemented.
- `cpt-cf-oagw-feature-control-plane` and `cpt-cf-oagw-feature-data-plane` are independent of each other and can be developed in parallel: the DESIGN mandates that all inter-module communication goes through versioned traits and SDK clients (`ControlPlaneService`, `DataPlaneService`, and the platform `-sdk` clients), so there is no work-package ordering between them. At runtime the data plane consumes control-plane-resolved effective configuration — a contract edge, not a build-order edge — which is why the data plane depends on the foundation only.
