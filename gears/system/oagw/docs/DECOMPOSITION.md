# Decomposition: oagw


<!-- toc -->

- [1. Overview](#1-overview)
- [2. Entries](#2-entries)
  - [2.1 Gear Foundation - HIGH](#21-gear-foundation---high)
  - [2.2 Upstream Management - HIGH](#22-upstream-management---high)
  - [2.3 Route Management - HIGH](#23-route-management---high)
  - [2.4 Request Proxy - HIGH](#24-request-proxy---high)
  - [2.5 Error Handling - MEDIUM](#25-error-handling---medium)
  - [2.6 Plugin System - HIGH](#26-plugin-system---high)
  - [2.7 Rate Limiting - MEDIUM](#27-rate-limiting---medium)
  - [2.8 CORS - MEDIUM](#28-cors---medium)
  - [2.9 Observability and State - MEDIUM](#29-observability-and-state---medium)
- [3. Feature Dependencies](#3-feature-dependencies)

<!-- /toc -->

**Overall implementation status:**
- [x] `p1` - **ID**: `cpt-cf-oagw-status-oagw-decomposition`
## 1. Overview

Within entries, a ticked requirement reference means the PRD definition is already marked done upstream, so the entry inherits a satisfied obligation rather than delivering it; an unticked reference is planned work for that entry. The overall status above is checked only when every entry is implemented.

DESIGN.md is decomposed into nine independently implementable and independently testable feature candidates. The `features/<slug>.md` documents linked from the entry headings are the FEATURE artifacts authored from these candidates; each carries its own `cpt-cf-oagw-featstatus-<slug>-implemented` marker, and the status boxes below reflect the delivered state. The decomposition preserves the Control Plane / Data Plane separation inside the single `oagw` crate and the DDD-Light layering (`domain` / `infra` / `api`) recorded in `cpt-cf-oagw-design-layers`: `cpt-cf-oagw-feature-gear-foundation` establishes the gear wiring, configuration model, merge engine, domain types, and repository boundary; the management features build the Control Plane; `cpt-cf-oagw-feature-request-proxy` builds the Data Plane hot path; and the remaining features layer cross-cutting behavior — errors, plugins, rate limiting, CORS, observability and state — onto that path.

**Decomposition strategy.** Each entry is a cohesive feature candidate with an explicit dependency list, a scope that can be implemented and verified without opening another entry, and full traceability back to PRD, DESIGN, and ADR identifiers. Entries never re-decide architecture: where an ADR governs behavior, the entry cites the ADR identifier and inherits its decision. No entry introduces a new requirement, a new architectural decision, or a new external dependency.

**Ordering rationale.** The order is strictly dependency-driven. Configuration and domain primitives must exist before any resource can be stored (2.1); upstreams must exist before routes can reference them (2.2 then 2.3); both must exist before the proxy path can resolve a target (2.4); the error contract is expressed against the proxy path (2.5); the plugin chain and the rate limiter are the two extension points that hang off the proxy path (2.6, 2.7); CORS is a handler-level concern on the same path (2.8); and observability plus CP/DP state ownership instrument and cache the whole flow (2.9). Entries 2.5, 2.7, 2.8, and 2.9 are mutually independent and can be developed in parallel once 2.4 exists.

**Coverage statement.** Every PRD functional requirement, non-functional requirement, use case, public interface, and external integration contract identifier (`cpt-cf-oagw-fr-*`, `cpt-cf-oagw-nfr-*`, `cpt-cf-oagw-usecase-*`, `cpt-cf-oagw-interface-*`, `cpt-cf-oagw-contract-*`), and every DESIGN principle, constraint, component-model, sequence, and data-schema identifier (`cpt-cf-oagw-principle-*`, `cpt-cf-oagw-constraint-*`, `cpt-cf-oagw-component-model`, `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-db-schema`, plus the DESIGN section anchors `cpt-cf-oagw-design-drivers`, `cpt-cf-oagw-design-layers`, `cpt-cf-oagw-tech-dependencies`, `cpt-cf-oagw-design-overview`, `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-design-dependencies`) is referenced by at least one entry below. The nine ADR identifiers are cited in the prose of the entries that realize them, and the PRD actor identifiers are cited in the entries that serve those actors.

**Graded-configuration deviations.** The following task-level overrides apply to this decomposition and take precedence over the supplied PRD, DESIGN, and ADR text where the two conflict. Each deviation is restated in the Scope or Out of scope text of the entry it affects.

1. **Gear-relative route paths.** All OAGW endpoints are registered and documented under `/oagw/v1/...` with no leading `/api`. The PRD and DESIGN tables that show `/api/oagw/v1/...` describe a different deployment shape; in the graded configuration the api-gateway gear owns the axum `Router` and nests gear routers under its own (empty) `prefix_path`, so `oagw` registers `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]` itself.
2. **`http` is a legal endpoint scheme.** `oagw.config.allow_http_upstream: true` in the graded configuration lifts the default posture that `cpt-cf-oagw-constraint-https-only` describes. The endpoint `scheme` enum is therefore `http | https | wss | wt | grpc` (default `https`). `cpt-cf-oagw-constraint-https-only` is documented as the default-TLS posture with the config flag as the documented lift, not as a prohibition on `http`.
3. **Automated test coverage is in scope.** Every entry that introduces behavior states test coverage as an in-scope item: unit tests in the crate (sibling `*_tests.rs` modules) and integration-style tests inside the crate's `tests/` directory.
4. **No e2e suite under `testing/e2e/gears/oagw/`.** That directory is out of scope for this decomposition; verification happens inside the `oagw` crate.
5. **Persistence resolved as in-memory and config-backed.** DESIGN §3.6 (`cpt-cf-oagw-db-schema`) specifies SeaORM/table storage and `cpt-cf-oagw-constraint-multi-sql` requires SQL usage, but the crate manifest `gears/system/oagw/oagw/Cargo.toml` declares no `toolkit-db`/`sea-orm` dependency and the graded configuration `config/e2e-local.yaml` has no oagw database block. The repository is therefore an in-memory, config-backed store behind the DESIGN repository trait boundary, and the DESIGN §3.6 table shapes are recorded as the documented schema contract preserved for a future SQL backend. `cpt-cf-oagw-constraint-multi-sql` is covered as a documented deviation.
6. **Starlark resolved as registry-reference-only.** `cpt-cf-oagw-nfr-starlark-sandbox` and the DESIGN custom-plugin text describe a Starlark runtime, but the crate manifest declares no Starlark dependency. Custom-plugin support is registry-reference-only in this decomposition: plugin GTS identifiers are resolvable, custom Starlark script execution is explicitly out of scope, and the sandboxing surface is the plugin-trait boundary itself.
7. **gRPC is configuration surface only.** DESIGN marks the gRPC proxy code path as planned and not reachable. gRPC stays in scope only as configuration and schema surface (the `grpc` match block and the `grpc` protocol enum value); an actual gRPC proxy code path is out of scope.
8. **Rate-limit strategy and scope surface.** The graded configuration exercises the `reject` strategy only; ADR 0003's `queue` and `degrade` strategies and the budget-allocation modes (`unlimited` | `allocated` | `shared` with `overcommit_ratio`) remain legal configuration surface but are not executed. The counter-scope enum additionally admits `route`, which the PRD enumeration does not list.
9. **Circuit breaker.** DESIGN §4.7 defers the circuit breaker to future development; `cpt-cf-oagw-nfr-high-availability` is therefore covered by the resilience posture, the health/readiness surface, and the breaker metric names, with breaker state itself out of scope.
10. **Auth plugin surface.** `basic.v1` and `bearer.v1` are catalog-only GTS identifiers per PRD §5.3 and DESIGN §3.1; the implemented auth methods are API Key and OAuth2 Client Credentials, and the catalog-only identifiers are rejected at binding time.

**Upstream specification notes.** The supplied PRD, DESIGN, and ADR text contains the discrepancies below. This decomposition resolves them locally as stated; the upstream documents are not edited by this run.

- Credential references are `cred://` URIs resolved via `cred_store` (DESIGN §2.1, §3.2); the PRD's "UUID pointer" wording at PRD §1.4, §4.1 and §7.2 is superseded.
- Proxy path placeholders are `{alias}`, `{path_suffix}`, `{query}` (DESIGN §3.3, matching the `path_suffix_mode` configuration key); the PRD's `{path}` spelling is superseded.
- gRPC is deferred at design level as Phase 3 (DESIGN §3.1, §3.6); the PRD §4.2 "phase 4" numbering is superseded. This decomposition further narrows gRPC to configuration surface only — see deviation 7.
- DESIGN §1.2 and §5.2 omit ADR 0008 and ADR 0009; both ADRs govern this gear and are traced by the entries below.
- DESIGN §5.1 does not map every PRD requirement identifier; the entries below carry the authoritative requirement-to-feature mapping.
- DESIGN §3.3's line permitting explicit alias updates for IP-based upstreams is superseded by DESIGN §3.2's alias-update behavior: the alias is immutable once set and any differing alias is rejected.
- DESIGN §4.1 describes config caching as a future consideration; ADR 0005 and ADR 0006 place the L1 caches in scope, and entry 2.9 follows the ADRs.
- The PRD requires a 503 for a disabled upstream but DESIGN §3.3 defines no dedicated GTS type for it; entry 2.5 maps that outcome onto `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`.
- The PRD states that a user-provided alias is rejected without qualification; DESIGN §3.2 tolerates submitting the exact derived value for idempotency, and entry 2.2 follows DESIGN.
- PRD §10 does not list `pingora`, which DESIGN §1.3/§3.4 mandate as the reverse-proxy engine; the dependency is carried by the DESIGN dependency tables.
- PRD §6.1 links `../guidelines/`, which does not exist in this gear tree; project-wide NFR baselines are out of scope for this decomposition.

## 2. Entries

### 2.1 [Gear Foundation](./features/gear-foundation.md) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-gear-foundation`

- **Purpose**: Establishes `oagw` as a Gears ToolKit gear and provides every primitive the other entries consume: gear wiring and REST registration under the gear-relative `/oagw/v1` prefix, `OagwConfig` parsing, the hierarchical sharing-mode merge engine, the shared domain model types, repository traits with an in-memory config-backed implementation, and GTS type provisioning. Without these primitives no configuration can be stored, resolved, or proxied.

- **Depends On**: None
  - **Phases**: single phase

- **Scope**:
  - ToolKit gear wiring for the single `oagw` crate (`gear.rs` with `#[toolkit::gear]`, `Gear::init`, `RestApiCapability`), honoring `cpt-cf-oagw-constraint-toolkit-deploy` (single-executable deployment)
  - Gear-relative REST registration skeleton: the gear registers `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]` under its own (empty) `prefix_path`, because the api-gateway gear owns the router in the graded configuration
  - `OagwConfig` parsing and defaults: `allow_http_upstream`, `proxy_timeout_secs`, `ssrf_policy`, `token_cache_ttl_secs`, `token_cache_capacity`, and the body-size limit
  - Configuration layering and hierarchical merge engine: priority Upstream (base) < Route < Tenant; sharing modes `private` / `inherit` / `enforce`; `min()` for rate limits, add-only union for tags, union for CORS origins, concatenation for plugin chains, and override for auth
  - Domain model types shared by both planes: `Upstream`, `Route`, `Plugin`, `ServerConfig`, `Endpoint`, and their configuration sub-structs
  - Repository traits (`UpstreamRepository`, `RouteRepository`, plugin repository) plus the in-memory, config-backed implementation with strictly tenant-scoped reads and writes
  - GTS type provisioning through `types_registry` for `gts.cf.core.oagw.upstream.v1~`, `gts.cf.core.oagw.route.v1~`, and the plugin base types
  - Credential isolation at the configuration boundary — configuration sub-structures carry `cred://` references only; no secret material is parsed, logged, or persisted by foundation code
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for config parsing, merge semantics, and repository tenant scoping

- **Out of scope**:
  - SeaORM / `toolkit-db` persistence: the crate manifest declares no `toolkit-db`/`sea-orm` dependency and `config/e2e-local.yaml` has no oagw database block, so the store is in-memory and config-backed and the DESIGN §3.6 table shapes are preserved as the documented schema contract for a future SQL backend (graded deviation 5)
  - `cpt-cf-oagw-constraint-multi-sql` (PostgreSQL/MySQL/SQLite portability) is not satisfiable in the graded configuration and is recorded here as a documented deviation (graded deviation 5)
  - Custom Starlark plugin execution (graded deviation 6, see 2.6) and any gRPC proxy code path (graded deviation 7, see 2.4)
  - Endpoint handler logic for upstreams, routes, plugins, and proxy requests — owned by 2.2, 2.3, 2.4, and 2.6
  - `testing/e2e/gears/oagw/` — no e2e suite is created for this gear (graded deviation 4); verification is entirely in-crate

- **Requirements Covered**:

  - [x]p2` - `cpt-cf-oagw-fr-config-layering`
  - [x]p2` - `cpt-cf-oagw-fr-hierarchical-config`
  - [ ] `p1` - `cpt-cf-oagw-nfr-multi-tenancy`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation` — covered here for the configuration-boundary slice only; secret resolution is owned by 2.6
  - [ ] `p1` - `cpt-cf-oagw-contract-types-registry`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`
  - `p1` - `cpt-cf-oagw-principle-cred-isolation`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-toolkit-deploy`
  - `p1` - `cpt-cf-oagw-constraint-no-direct-internet`
  - `p1` - `cpt-cf-oagw-constraint-multi-sql`

  `cpt-cf-oagw-constraint-multi-sql` is covered as a documented deviation: the constraint cannot be satisfied in the graded configuration, which has no database block and no `toolkit-db` dependency (graded deviation 5).

- **Domain Model Entities**:
  - `Upstream` (`gts.cf.core.oagw.upstream.v1~`) — tenant-scoped root configuration object, unique per `(tenant_id, alias)`
  - `Route` (`gts.cf.core.oagw.route.v1~`) — belongs to an upstream, carries match rules and per-route overrides
  - `Plugin` (`gts.cf.core.oagw.{type}_plugin.v1~`) — named plugins resolved in-process, custom plugins referenced by identifier
  - `ServerConfig` / `Endpoint` — endpoint pool with `scheme`, `host`, `port`

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
    - Gear wiring (`gear.rs`) and `OagwConfig` (`config.rs`)
    - DDD-Light layer skeleton: `api/rest/`, `domain/`, `infra/`
    - Domain models (`domain/dto.rs`) and `DomainError` (`domain/error.rs`)
    - Repository traits (`domain/repo.rs`) and in-memory storage implementations (`infra/storage/`)
    - `infra/type_provisioning.rs` GTS registration
    - Tenant scoping enforced in the repository layer, serving `cpt-cf-oagw-actor-types-registry` and `cpt-cf-oagw-actor-tenant-admin`
  - `p1` - `cpt-cf-oagw-design-layers`
  - `p1` - `cpt-cf-oagw-design-domain-model`
  - `p1` - `cpt-cf-oagw-tech-dependencies`
  - `p1` - `cpt-cf-oagw-interface-api`
    - The DESIGN §3.3 API contract section: this entry registers the gear-relative route tree that 2.2, 2.3, 2.4, and 2.6 implement endpoints against
  - `p1` - `cpt-cf-oagw-design-drivers`
    - The gear realizes the architecture drivers by scoping foundation wiring, configuration, and the domain/repository boundary they drive
  - `p1` - `cpt-cf-oagw-design-overview`
    - The foundation establishes the Control Plane / Data Plane split and the DDD-Light layer structure the overview diagram shows
  - `p1` - `cpt-cf-oagw-design-dependencies`
    - The foundation declares and wires the gear's external dependencies: types-registry, cred_store, authz-resolver, tenant-resolver, and the Toolkit capabilities

- **API**: Registration only — no endpoint is completed by this entry. The gear registers the gear-relative route tree `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/{alias}[/{path_suffix}]`, whose handlers are implemented by 2.2, 2.3, 2.4, and 2.6.

- **Sequences**:

  None — DESIGN.md defines no interaction sequence for gear wiring, configuration parsing, or repository access; the only sequence DESIGN.md defines is `cpt-cf-oagw-seq-proxy-flow`, covered by 2.4.

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

  DESIGN §3.6 table shapes — `oagw_upstream` (PK `id`, UNIQUE `(tenant_id, alias)`), `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag` / `oagw_route_tag`, `oagw_plugin`, `oagw_upstream_plugin` / `oagw_route_plugin` — are preserved as the documented schema contract and are materialized as the in-memory, config-backed repository in the graded configuration.

### 2.2 [Upstream Management](./features/upstream-management.md) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-upstream-management`

- **Purpose**: Implements Control Plane CRUD for upstream configurations, the fundamental configuration unit that every proxy request targets. It enforces the alias derivation and immutability matrix, the `enabled` flag and its inheritance semantics, tags, and per-tenant alias uniqueness, and exposes all of it through the management REST API.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`
  - **Phases**: single phase

- **Scope**:
  - Upstream CRUD: `POST`, `GET` list, `GET` by id, `PUT` (full replacement), `DELETE` at `/oagw/v1/upstreams`
  - Alias derivation from endpoints: single hostname, non-standard port (`hostname:port`), multi-hostname registrable common suffix with public-suffix-list validation (bare public suffixes rejected), and explicit alias mandatory for IP-based or non-derivable endpoint pools
  - Alias immutability transition matrix on `PUT` (DESIGN §3.2, whose alias-update behavior supersedes DESIGN §3.3's line permitting explicit alias updates for IP-based upstreams): derivable → derivable, derivable → non-derivable, non-derivable → non-derivable, non-derivable → derivable, and no-endpoint-change cases; submitting the exact derived alias value is tolerated silently for idempotency
  - Alias normalization to ASCII lowercase with trailing dots stripped and case-insensitive resolution; RFC 1123 hostname validation; `409 Conflict` on a `(tenant_id, alias)` collision
  - `enabled` flag (default `true`) with enable/disable semantics: a disabled upstream rejects proxy requests, and an ancestor-disabled upstream cannot be re-enabled by a descendant
  - Tags with add-only union semantics across the tenant hierarchy; endpoint pool uniformity validation (identical `protocol`, `scheme`, and `port`)
  - Endpoint `scheme` enum `http | https | wss | wt | grpc` with `https` as the default; `http` is legal when `oagw.config.allow_http_upstream: true` (graded deviation 2)
  - OData list query parameters (`$filter`, `$select`, `$orderby`, `$top` default 50 and max 100, `$skip`); strict tenant scoping with ancestor resources returning `404`
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for CRUD, alias derivation and immutability, validation failures, and `409` conflicts

- **Out of scope**:
  - Proxy-time alias resolution and tenant-hierarchy shadowing — owned by 2.4
  - Auth plugin binding resolution and credential handling — owned by 2.6
  - Effective-configuration merge semantics — owned by the 2.1 merge engine
  - No `http` scheme prohibition is enforced here: `cpt-cf-oagw-constraint-https-only` is the default-TLS posture and `oagw.config.allow_http_upstream: true` is the documented lift (graded deviation 2)

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-upstream-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-fr-enable-disable`
  - [x]p2` - `cpt-cf-oagw-fr-alias-resolution`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-upstream`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry; `cpt-cf-oagw-constraint-https-only` is covered by 2.4 with the graded lift documented above.

- **Domain Model Entities**:
  - `Upstream` — `alias`, `tags`, `enabled`, `server`, `protocol`, `auth`, `headers`, `rate_limit`, `cors`, `plugins`
  - `ServerConfig` / `Endpoint` — endpoint pool and alias derivation inputs

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
    - `ControlPlaneService` upstream CRUD operations
    - Axum REST handlers and DTOs under `api/rest/handlers/` and `api/rest/dto.rs`
    - Alias derivation and alias-immutability enforcement helpers
    - Upstream request validation feeding the shared error contract realized in 2.5
    - Per `cpt-cf-oagw-adr-request-routing`, upstream operations route to the Control Plane, serving `cpt-cf-oagw-actor-platform-operator`

- **API**:
  - POST /oagw/v1/upstreams
  - GET /oagw/v1/upstreams
  - GET /oagw/v1/upstreams/{id}
  - PUT /oagw/v1/upstreams/{id}
  - DELETE /oagw/v1/upstreams/{id}

- **Sequences**:

  None — DESIGN.md documents management operations as a linear flow (Client → API Handler → ControlPlaneService → Response) rather than a named sequence; `cpt-cf-oagw-seq-proxy-flow` covers the proxy path and is owned by 2.4.

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

  Upstream-shaped records: `oagw_upstream` (PK `id`, UNIQUE `(tenant_id, alias)`), `oagw_upstream_tag` (PK `(parent_id, tag)`), and `oagw_upstream_plugin` (ordered bindings), held in the in-memory repository with the documented schema contract preserved.

### 2.3 [Route Management](./features/route-management.md) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-route-management`

- **Purpose**: Implements Control Plane CRUD for routes, the matching rules that map inbound proxy requests to specific upstream behaviors. Routes carry the HTTP match block, the gRPC match block as configuration surface, plugin bindings, and route-level rate-limit and CORS overrides.

- **Depends On**: `cpt-cf-oagw-feature-gear-foundation`, `cpt-cf-oagw-feature-upstream-management`
  - **Phases**: single phase

- **Scope**:
  - Route CRUD: `POST`, `GET` list, `GET` by id, `PUT` (full replacement), `DELETE` at `/oagw/v1/routes`
  - `match` block with exactly one of `http` or `grpc`: `http` requires `methods` (non-empty subset of `GET`, `POST`, `PUT`, `DELETE`, `PATCH`) and `path`; `grpc` requires `service` and `method`
  - `query_allowlist` (empty means allow none) and `path_suffix_mode` (`disabled` | `append`, default `append`)
  - `upstream_id` must belong to the calling tenant, and `upstream_id` is immutable across `PUT`
  - Match-rule uniqueness within an upstream (same path + priority + method → `409 Conflict`) and the `enabled` flag excluding disabled routes from matching
  - Route-level `rate_limit`, `cors`, `plugins`, and `tags` fields merged by the 2.1 merge engine
  - OData list query parameters as in 2.2
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for CRUD, match validation, uniqueness conflicts, and the immutable `upstream_id`

- **Out of scope**:
  - Any gRPC proxy code path: the `grpc` match block and the `grpc` protocol enum value are configuration and schema surface only (graded deviation 7); request classification and matching execution are owned by 2.4
  - Proxy-time route selection and priority resolution — owned by 2.4
  - Plugin resolution and execution — owned by 2.6

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-route-mgmt`
  - [ ] `p1` - `cpt-cf-oagw-usecase-configure-route`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api`

- **Design Principles Covered**:

  None — this entry introduces no new design principle; tenant scoping is inherited from the 2.1 repository layer and from 2.2.

- **Design Constraints Covered**:

  None — this entry introduces no DESIGN constraint; body and scheme constraints are enforced on the proxy path in 2.4.

- **Domain Model Entities**:
  - `Route` — `upstream_id`, `match` (`http_match` / `grpc_match`), `priority`, `enabled`, `rate_limit`, `cors`, `plugins`, `tags`
  - `MatchConfig` — protocol-scoped match keys

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
    - `ControlPlaneService` route CRUD operations
    - Axum REST route handlers and DTOs
    - Match-rule validation and uniqueness checks
    - Per `cpt-cf-oagw-adr-request-routing`, route operations route to the Control Plane

- **API**:
  - POST /oagw/v1/routes
  - GET /oagw/v1/routes
  - GET /oagw/v1/routes/{id}
  - PUT /oagw/v1/routes/{id}
  - DELETE /oagw/v1/routes/{id}

- **Sequences**:

  None — route management is a management-plane operation; `cpt-cf-oagw-seq-proxy-flow` is the only sequence DESIGN.md defines and is owned by 2.4.

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

  Route-shaped records: `oagw_route` (PK `id`, FK `upstream_id` cascade), `oagw_route_http_match`, `oagw_route_grpc_match`, `oagw_route_method` (PK `(route_id, method)`), `oagw_route_tag`, and `oagw_route_plugin`; match-determinism invariant: no two enabled routes under the same upstream share `(path_prefix, priority)` for the same method.

### 2.4 [Request Proxy](./features/request-proxy.md) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-request-proxy`

- **Purpose**: Implements the Data Plane proxy path, the core value proposition of the gear. It resolves an upstream by alias across the tenant hierarchy, matches a route, merges effective configuration, classifies and transforms headers, selects an endpoint from the pool, forwards the request to the external service, and returns the response with the error-source header — including streaming responses.

- **Depends On**: `cpt-cf-oagw-feature-upstream-management`, `cpt-cf-oagw-feature-route-management`, `cpt-cf-oagw-feature-gear-foundation`
  - **Phases**: single phase

- **Scope**:
  - `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]` handler dispatching to `DataPlaneService`, per `cpt-cf-oagw-adr-request-routing`
  - Tenant-hierarchy alias resolution from descendant to root with shadowing (closest match wins) and enforced ancestor constraints retained across shadowing
  - Route matching by method allowlist and longest path prefix with priority; disabled routes excluded from matching; a disabled upstream rejected with `503`
  - Path suffix handling per `path_suffix_mode` (`disabled` rejects a provided suffix; `append` appends it to the matched path)
  - `query_allowlist` enforcement, where an empty allowlist permits no query parameter
  - Header classification and transformation (DESIGN §3.2): routing headers (`X-OAGW-Target-Host` read then stripped, `Host` / `:authority` replaced), hop-by-hop stripping (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), and passthrough control with `headers.request` set/add/remove plus `passthrough` (`none` | `allowlist` | `all`) and `headers.response` set/add/remove
  - `X-OAGW-Target-Host` behavior matrix (DESIGN §3.2, ADR 0007) from `cpt-cf-oagw-adr-request-routing`: required for multi-endpoint upstreams with a common-suffix alias, optional but validated when present, and round-robin when absent
  - Endpoint pool round-robin selection with uniform `protocol` / `scheme` / `port` validation; upstream call over `http` (permitted by `oagw.config.allow_http_upstream: true`) or `https`
  - Request forwarding with no automatic full client-request retry (endpoint-level connection failover remains permitted) and response passthrough without caching
  - `X-OAGW-Error-Source: gateway|upstream` emitted on all error responses, including streaming error responses
  - SSE and WebSocket **and** WebTransport session flows with connection lifecycle handling (open / close / error) for the `sse`, `ws`, and `wt` endpoint schemes; `wt` is a legal endpoint scheme value
  - Request-surface hardening in the proxy path: strip well-known internal hop-by-hop headers from forwarded requests, validate request paths and query parameters against the matched route configuration, and enforce the endpoint scheme allowlist (the allowlist admits `http` per graded deviation 2)
  - Body validation: `Content-Length` integrity, only `chunked` Transfer-Encoding accepted, and the 100 MB hard limit rejected before buffering
  - Preflight `OPTIONS` detection returning a permissive `204` at handler level with no upstream resolution and no tenant context
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for alias resolution and shadowing, route matching, header rules, endpoint selection, streaming lifecycle, and body limits

- **Out of scope**:
  - Any gRPC proxy code path: `upstream.protocol = gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1` and the `grpc` match block are configuration and schema surface only, with no reachable gRPC proxy code path (graded deviation 7)
  - Response caching — `cpt-cf-oagw-principle-no-cache` places caching with the client and upstream
  - Automatic retries of the original client request — `cpt-cf-oagw-principle-no-retry` places retry responsibility with the client
  - CORS origin and method enforcement on actual requests — owned by 2.8
  - Auth, guard, and transform plugin execution — owned by 2.6; this entry defines the extension points they plug into
  - DP L1 hot-config cache implementation — owned by 2.9 per `cpt-cf-oagw-adr-state-management`
  - Gateway error body rendering — the shared problem+json contract is owned by 2.5
  - DNS-resolution validation and IP pinning — PRD §4.2 and DESIGN §4.5 declare DNS/IP-pinning rules a separate concern, so `cpt-cf-oagw-nfr-ssrf-protection` is covered here only by the request-surface validation above

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-request-proxy`
  - [ ] `p1` - `cpt-cf-oagw-fr-streaming`
  - [ ] `p1` - `cpt-cf-oagw-fr-header-transform`
  - [ ] `p1` - `cpt-cf-oagw-usecase-proxy-request`
  - [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`
  - [ ] `p1` - `cpt-cf-oagw-interface-proxy-api`
  - [ ] `p1` - `cpt-cf-oagw-nfr-low-latency`
  - [ ] `p1` - `cpt-cf-oagw-nfr-ssrf-protection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation` — owned by 2.4 for enforcement; 2.5 renders the resulting 4xx problem+json and 2.8 covers the CORS-configuration validation slice

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-no-cache`
  - `p1` - `cpt-cf-oagw-principle-no-retry`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  - `p1` - `cpt-cf-oagw-constraint-body-limit`
  - `p1` - `cpt-cf-oagw-constraint-https-only`

  `cpt-cf-oagw-constraint-https-only` is implemented as the default-TLS posture for upstream connections, and `oagw.config.allow_http_upstream: true` is the documented lift that makes `http` a legal endpoint scheme in the graded configuration (graded deviation 2).

- **Domain Model Entities**:
  - `ProxyContext`, `ProxyResponse` — internal Data Plane DTOs
  - `Upstream` / `Endpoint` — resolution target and load-balance pool
  - `Route` / `MatchConfig` — match keys applied at request time

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
    - `DataPlaneService` proxy orchestration
    - Proxy handler and extractors (`X-OAGW-Target-Host`, security context)
    - Alias resolver, route matcher, and effective-configuration merger
    - Header transformer, endpoint selector, and upstream connector
    - SSE and WebSocket streaming passthrough
    - Request validation and body-limit enforcement, serving `cpt-cf-oagw-actor-app-developer` and `cpt-cf-oagw-actor-upstream-service`

- **API**:
  - {METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]

- **Sequences**:

  - `p1` - `cpt-cf-oagw-seq-proxy-flow`

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

  Read-side resolution only: Find Upstream by Alias (tenant hierarchy walk with `enabled` inheritance), Find Matching Route for Request (`upstream_id`, method, longest path prefix, priority), and Resolve Effective Configuration (hierarchy walk with sharing modes), all served from the in-memory repository.

### 2.5 [Error Handling](./features/error-handling.md) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-error-handling`

- **Purpose**: Defines the single error contract applied to every OAGW endpoint: RFC 9457 `application/problem+json` gateway errors carrying GTS error types, the full DESIGN error-table mapping, and the `X-OAGW-Error-Source` distinction between gateway-generated and upstream-passthrough failures.

- **Depends On**: `cpt-cf-oagw-feature-request-proxy`
  - **Phases**: single phase

- **Scope**:
  - RFC 9457 problem+json responses (DESIGN §3.3) with `type`, `title`, `status`, `detail`, and `instance`, plus the OAGW extension fields `upstream_id`, `host`, `path`, `retry_after_seconds`, and `trace_id`
  - GTS error types under `gts.cf.core.errors.err.v1~cf.oagw.*` for the complete DESIGN §3.3 error table: validation and routing (`validation.error.v1`, `routing.missing_target_host.v1`, `routing.invalid_target_host.v1`, `routing.unknown_target_host.v1`), auth (`auth.failed.v1`), route matching (`route.not_found.v1`), plugin (`plugin.in_use.v1`, `plugin.not_found.v1`), payload (`payload.too_large.v1`), rate limit (`rate_limit.exceeded.v1`), secret (`secret.not_found.v1`), protocol, downstream, and stream (`protocol.error.v1`, `downstream.error.v1`, `stream.aborted.v1`), link and circuit breaker (`link.unavailable.v1`, `circuit_breaker.open.v1`), and timeouts (`timeout.connection.v1`, `timeout.request.v1`, `timeout.idle.v1`)
  - `X-OAGW-Error-Source: gateway` on gateway-generated errors with a problem+json body, and `X-OAGW-Error-Source: upstream` on passthrough upstream errors with the upstream body left unmodified
  - Timeout, `502` DownstreamError, and `504` Timeout mapping, and the `X-OAGW-Target-Host` missing / invalid / unknown error bodies with `valid_hosts`, `invalid_value`, and `alias` extension fields per `cpt-cf-oagw-adr-error-source-distinction`
  - The PRD error-code table (400 ValidationError, 401 AuthenticationFailed, 404 RouteNotFound, 413 PayloadTooLarge, 429 RateLimitExceeded, 500 SecretNotFound, 502 DownstreamError, 503 CircuitBreakerOpen, 504 Timeout) mapped onto the GTS types above
  - The PRD's 503 disabled-upstream outcome has no dedicated GTS type in DESIGN §3.3 and is served by `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` with `X-OAGW-Error-Source: gateway`
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for every mapped status, header value, and problem+json body shape

- **Out of scope**:
  - New endpoints — this entry adds no route; it supplies the error mapping consumed by every endpoint registered in 2.1
  - Upstream error body transformation — passthrough is unmodified by design
  - Retry decisions — retriability is metadata on the error type, not gateway retry behavior

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-error-codes`
  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation` — owned by 2.4 for enforcement; this entry renders the resulting 4xx problem+json bodies and 2.8 covers the CORS-configuration validation slice
  - [ ] `p2` - `cpt-cf-oagw-nfr-observability` — covered here only for the observability slice carried by the error surface (`trace_id` propagation and `error_type` audit fields); the metrics and logging surface is owned by 2.9

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-rfc9457`
  - `p1` - `cpt-cf-oagw-principle-error-source`

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry; `cpt-cf-oagw-constraint-body-limit` produces the `413` mapping but is enforced in 2.4.

- **Domain Model Entities**:
  - `DomainError` — gateway error taxonomy mapped to HTTP status and GTS type
  - Problem-details response DTO

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
    - Error mapping layer (`api/rest/error.rs`) and `DomainError` (`domain/error.rs`)
    - `X-OAGW-Error-Source` response-header emission
    - Problem-details serialization with GTS type identifiers and OAGW extension fields

- **API**:

  None — no externally addressable endpoint is introduced by this entry; the error contract applies to every `/oagw/v1/...` endpoint registered in 2.1.

- **Sequences**:

  None — `cpt-cf-oagw-seq-proxy-flow` is the only sequence DESIGN.md defines and is owned by 2.4; error rendering is a branch of that flow.

- **Data**:

  None — error mapping is stateless and persists nothing, so no documented schema table is owned by this entry.

### 2.6 [Plugin System](./features/plugin-system.md) - HIGH

- [x] `p1` - **ID**: `cpt-cf-oagw-feature-plugin-system`

- **Purpose**: Implements the three plugin types with trait-based extensibility, the registries that resolve them, the built-in plugins shipped in the crate, credential resolution from the credential store, and the plugin management API. This is the extension point that delivers credential injection, request guarding, and request/response mutation without modifying the gateway core.

- **Depends On**: `cpt-cf-oagw-feature-request-proxy`
  - **Phases**: single phase

- **Scope**:
  - `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` trait definitions with deterministic execution order: Auth → Guards → Transform(request) → upstream call → Transform(response/error)
  - Upstream-before-route chain composition (`[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`) and `plugins.items[]` resolution with sharing modes (concatenation; enforced plugins cannot be removed)
  - Registries `AuthPluginRegistry`, `GuardPluginRegistry`, and `TransformPluginRegistry` built via `with_builtins()`
  - Built-in auth plugins: `noop`, `apikey`, `oauth2_client_cred`, and `oauth2_client_cred_basic` (Form and Basic client-auth variants sharing one token cache, per `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`)
  - Built-in guard plugin: `required_headers` (request and response phases, fail-open when unconfigured, first missing header reported) per `cpt-cf-oagw-adr-required-headers-guard-plugin`
  - Built-in transform plugin: `request_id` (`X-Request-ID` injection and propagation)
  - Catalog-only GTS identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) registered in the types-registry catalog and rejected at binding time as unresolvable
  - `cred_store` secret resolution by `cred://` reference at request time, with credentials never logged, never returned in API responses, and never stored by OAGW
  - OAuth2 token cache configuration (`token_cache_ttl_secs`, `token_cache_capacity`) with tenant- and subject-scoped cache keys and no caching of failed fetches
  - Plugin management endpoints under `/oagw/v1/plugins`: create, list, get by id, delete (with `409 PluginInUse` and a `referenced_by` body when referenced), and source retrieval; plugins are immutable after creation (no `PUT`)
  - Custom plugin references resolvable by GTS identifier, with `PluginNotFound` returned when a referenced plugin cannot be resolved at proxy time
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for execution order, registry resolution, built-in plugin behavior, credential isolation, and `409` deletion semantics

- **Out of scope**:
  - Custom Starlark script execution: the crate manifest declares no Starlark dependency, so `cpt-cf-oagw-nfr-starlark-sandbox` is covered with the plugin-trait boundary as the sandboxing surface and Starlark interpretation is explicitly out of scope (graded deviation 6)
  - Plugin garbage collection and the `gc_eligible_at` lifecycle — no persisted plugin rows exist in the graded configuration
  - Header value validation in the `required_headers` guard, which is presence-only per `cpt-cf-oagw-adr-required-headers-guard-plugin`
  - Re-issuing the original client request after token refresh — `cpt-cf-oagw-principle-no-retry` forbids it

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-plugin-system`
  - [ ] `p2` - `cpt-cf-oagw-fr-builtin-plugins`
  - [ ] `p1` - `cpt-cf-oagw-fr-auth-injection`
  - [ ] `p1` - `cpt-cf-oagw-nfr-credential-isolation` — secret resolution is owned by this entry; 2.1 covers the configuration-boundary slice only
  - [ ] `p3` - `cpt-cf-oagw-nfr-starlark-sandbox`
  - [ ] `p1` - `cpt-cf-oagw-contract-cred-store`
  - [ ] `p1` - `cpt-cf-oagw-interface-management-api` — the plugin half of the management REST surface (`/oagw/v1/plugins`) is delivered here

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-cred-isolation`
  - `p1` - `cpt-cf-oagw-principle-plugin-immutable`

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry; deployment and scheme constraints are covered by 2.1 and 2.4.

- **Domain Model Entities**:
  - `Plugin` — `plugin_type`, `name`, `config_schema`, and `source_code` reference
  - Plugin binding — `plugin_ref` plus nullable `plugin_uuid`, ordered by position
  - `AuthContext`, `RequestContext`, `ResponseContext`, `ErrorContext` — plugin execution payloads

- **Design Components**:

  - `p1` - `cpt-cf-oagw-component-model`
    - `AuthPluginRegistry`, `GuardPluginRegistry`, and `TransformPluginRegistry` (`infra/plugin/`)
    - Built-in plugin implementations: `NoopAuthPlugin`, `ApiKeyAuthPlugin`, `OAuth2ClientCredAuthPlugin` (Form and Basic), `RequiredHeadersGuardPlugin`, `RequestIdTransformPlugin`
    - Plugin chain executor with upstream-before-route composition
    - Plugin management REST handlers and DTOs, serving `cpt-cf-oagw-actor-cred-store` and `cpt-cf-oagw-actor-platform-operator`
    - Per `cpt-cf-oagw-adr-plugin-system`, external plugins integrate by implementing the same traits

- **API**:
  - POST /oagw/v1/plugins
  - GET /oagw/v1/plugins
  - GET /oagw/v1/plugins/{id}
  - DELETE /oagw/v1/plugins/{id}
  - GET /oagw/v1/plugins/{id}/source

- **Sequences**:

  None — plugin execution is a set of steps inside `cpt-cf-oagw-seq-proxy-flow`, which is owned by 2.4; DESIGN.md defines no separate plugin sequence.

- **Data**:

  - `p1` - `cpt-cf-oagw-db-schema`

  Plugin-shaped records: `oagw_plugin` (PK `id`, UNIQUE `(tenant_id, name)`), and `oagw_upstream_plugin` / `oagw_route_plugin` (PK `(parent_id, position)`), with `plugin_ref` always stored and `plugin_uuid` present only for UUID-backed plugins; held in the in-memory repository with the documented schema contract preserved.

### 2.7 [Rate Limiting](./features/rate-limiting.md) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-rate-limiting`

- **Purpose**: Implements rate limiting on the proxy hot path with a dual sustained/burst token bucket, hierarchical `min()` inheritance, scoped counters, and the standard response headers. This protects external service agreements and prevents cost overruns without adding a control-plane hop per request.

- **Depends On**: `cpt-cf-oagw-feature-request-proxy`
  - **Phases**: single phase

- **Scope**:
  - Dual-rate configuration: `algorithm` (`token_bucket` default, `sliding_window` optional), `sustained.rate` with `sustained.window` (`second` | `minute` | `hour` | `day`), `burst.capacity` defaulting to `sustained.rate`, and `cost` defaulting to 1
  - Token bucket implementation with refill-on-read semantics and burst capacity, plus the sliding-window alternative
  - Hierarchical inheritance: `effective = min(ancestor.enforced, descendant)` so a descendant can only be stricter, and enforced ancestor limits retained across alias shadowing
  - Counter scopes `global`, `tenant`, `user`, `ip`, and `route` with per-instance in-memory counters owned by the Data Plane per `cpt-cf-oagw-adr-state-management`
  - Strategy `reject` returning `429` with `Retry-After` and the `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` response headers
  - Rate-limit evaluation on both upstream-level and route-level configuration, merged by the 2.1 merge engine
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for bucket refill, burst behavior, hierarchical `min()` inheritance, scope keying, and 429 headers

- **Out of scope**:
  - Distributed rate-limit synchronization (Redis-backed counters and the `rate_limit_sync` block in `cpt-cf-oagw-adr-rate-limiting`) — the MVP is per-instance limiting
  - Budget allocation modes (`unlimited` | `allocated` | `shared` with `overcommit_ratio`) — documented in the ADR but not required by the graded configuration
  - Strategy `queue` and strategy `degrade` execution — the graded configuration uses `reject`; the other values remain legal configuration surface
  - Distributed counter persistence — no documented schema table stores rate-limit state

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-fr-rate-limiting`
  - [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded` — covered here for the reject outcome and the 429/Retry-After mapping; `queue` and `degrade` execution are excluded per graded deviation 8

- **Design Principles Covered**:

  - `p1` - `cpt-cf-oagw-principle-tenant-scope`

  Rate-limit counters are keyed by tenant scope, so a tenant can never consume another tenant's budget.

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry; the low-latency budget it serves is `cpt-cf-oagw-nfr-low-latency`, covered by 2.4.

- **Domain Model Entities**:
  - `RateLimitConfig` — `sharing`, `algorithm`, `sustained`, `burst`, `scope`, `strategy`, `cost`
  - `TokenBucket` — per-instance counter state

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
    - `RateLimiterRegistry` owned by the Data Plane
    - Token-bucket and sliding-window implementations
    - Rate-limit decision integration point in the proxy path, ahead of the upstream call
    - `429` response construction with `Retry-After` and `X-RateLimit-*` headers, per `cpt-cf-oagw-adr-rate-limiting`

- **API**:

  None — no externally addressable endpoint is introduced by this entry; rate limiting adds the `429` response and `X-RateLimit-*` headers to `/oagw/v1/proxy/{alias}[/{path_suffix}]`.

- **Sequences**:

  None — the rate-limit check is a step inside `cpt-cf-oagw-seq-proxy-flow`, which is owned by 2.4.

- **Data**:

  None — rate-limit counters are in-memory, per-instance state per `cpt-cf-oagw-adr-state-management`; no documented schema table stores rate-limit state.

### 2.8 [CORS](./features/cors.md) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-cors`

- **Purpose**: Implements the built-in CORS handler as a first-class concern rather than a plugin: permissive preflight handling at the handler level for speed and upstream independence, and strict origin and method enforcement on actual cross-origin requests after upstream resolution.

- **Depends On**: `cpt-cf-oagw-feature-request-proxy`
  - **Phases**: single phase

- **Scope**:
  - Permissive `204` preflight response at handler level echoing the requested origin, method, and headers, with `Access-Control-Max-Age: 86400` and `Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers`, and no upstream resolution or tenant context
  - Actual-request enforcement after upstream resolution and before forwarding: `403` with `cors.origin_not_allowed.v1` for a disallowed origin and `403` with `cors.method_not_allowed.v1` for a disallowed method
  - CORS response headers on actual requests: `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers` from `expose_headers`, `Access-Control-Allow-Credentials` from `allow_credentials`, and `Vary: Origin`
  - Exact, port-sensitive, and protocol-sensitive origin matching with no regex patterns
  - Validation-time rejection of `allow_credentials: true` combined with a wildcard `*` origin
  - Hierarchical origin union under `sharing: inherit`, with `sharing: enforce` preventing a descendant from adding origins, via the 2.1 merge engine
  - `cors.enabled` defaulting to false so CORS is deny-by-default
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for preflight responses, origin and method rejection, credentials/wildcard validation, and hierarchical origin union

- **Out of scope**:
  - CORS as a guard plugin — `cpt-cf-oagw-adr-cors` rejects that option, and the `cors` guard GTS identifier remains catalog-only and not bindable through `plugins.items[].plugin_ref`
  - Proxying preflight requests to the upstream
  - Global or edge rate limiting and WAF/DDoS controls, which still apply to preflight requests but are provided by the platform

- **Requirements Covered**:

  - [ ] `p1` - `cpt-cf-oagw-nfr-input-validation` — owned by 2.4 for enforcement; this entry covers the CORS-configuration validation slice
  - [x]p2` - `cpt-cf-oagw-fr-hierarchical-config`

- **Design Principles Covered**:

  None — this entry introduces no new design principle; it realizes the decision recorded in `cpt-cf-oagw-adr-cors`.

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry.

- **Domain Model Entities**:
  - `CorsConfig` — `sharing`, `enabled`, `allowed_origins`, `allowed_methods`, `expose_headers`, `allow_credentials`

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
    - Built-in CORS handler invoked before the plugin chain
    - Preflight responder at the proxy handler level
    - Origin and method validator applied after upstream resolution
    - Per `cpt-cf-oagw-adr-cors`, CORS is core Data Plane logic rather than a `GuardPlugin` implementation

- **API**:
  - OPTIONS /oagw/v1/proxy/{alias}[/{path_suffix}] (preflight, returns a permissive 204 at handler level)

- **Sequences**:

  None — preflight and actual-request handling are branches of `cpt-cf-oagw-seq-proxy-flow`, which is owned by 2.4.

- **Data**:

  None — CORS is configured per upstream and route and holds no state of its own, so no documented schema table is owned by this entry.

### 2.9 [Observability and State](./features/observability-and-state.md) - MEDIUM

- [x] `p2` - **ID**: `cpt-cf-oagw-feature-observability-and-operability`

- **Purpose**: Implements the observability surface and the CP/DP state ownership that make the gear operable and fast: Prometheus metrics, structured audit logging, trace identifiers, the Control Plane L1 configuration cache with write-side invalidation, the Data Plane hot-config cache with explicit flush, and the health/readiness surface.

- **Depends On**: `cpt-cf-oagw-feature-request-proxy`
  - **Phases**: single phase

- **Scope**:
  - Prometheus metrics at `/metrics` (admin-only) with the names, labels, and histogram buckets from DESIGN §4.2: `oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_requests_in_flight`, `oagw_errors_total`, `oagw_circuit_breaker_state`, `oagw_circuit_breaker_transitions_total{host, from_state, to_state}`, `oagw_rate_limit_exceeded_total`, `oagw_rate_limit_usage_ratio`, `oagw_routing_target_host_used`, `oagw_routing_endpoint_selected`, `oagw_upstream_available`, and `oagw_upstream_connections`
  - The `path` label on the rate-limit metrics carries the normalized route match pattern (`http.route`), never the raw request path, per the DESIGN §4.2 cardinality rules.
  - Cardinality management: no tenant labels, `http.route` as the normalized route match pattern, `http.request.method` normalized to a standard verb or `_OTHER`, and numeric `http.response.status_code`, matching the OTel HTTP semantic conventions used by the inbound API gateway
  - Structured JSON audit logging to stdout with the field set from `cpt-cf-oagw-adr-request-routing` (`timestamp`, `level`, `event`, `request_id`, `tenant_id`, `principal_id`, `host`, `path`, `method`, `status`, `duration_ms`, `request_size`, `response_size`, `error_type`), with no PII and no secrets
  - Trace identifiers propagated into problem+json responses and log records
  - Control Plane L1 configuration cache (10,000-entry LRU, no TTL) with write-side invalidation on every configuration write, per `cpt-cf-oagw-adr-data-plane-caching`
  - Data Plane L1 hot-config cache (1,000-entry LRU, no TTL, explicit invalidation) with an explicit flush triggered on configuration writes, per `cpt-cf-oagw-adr-state-management`
  - Deployment-mode notes for single-exec (L1 only) and microservice (optional shared L2) operation, with no L2 implementation in the graded configuration
  - Health and readiness surface through the ToolKit `RestApiCapability` healthcheck hook
  - Automated test coverage: unit tests in the crate and integration-style tests in `tests/` for metric names and labels, audit-log field shape, cache hit/miss and invalidation, and DP flush on configuration writes

- **Out of scope**:
  - The L2 Redis cache layer — `cpt-cf-oagw-adr-data-plane-caching` makes it optional and the graded configuration has no Redis dependency
  - Circuit breaker implementation — DESIGN §4.7 lists it as future development, so `cpt-cf-oagw-nfr-high-availability` is covered here by the resilience posture, the health/readiness surface, and the circuit-breaker metric names, with the breaker state itself as future work
  - Distributed rate-limit state — owned by 2.7 as per-instance limiting
  - Response caching — forbidden by `cpt-cf-oagw-principle-no-cache`
  - `testing/e2e/gears/oagw/` — out of scope per graded deviation 4

- **Requirements Covered**:

  - [ ] `p2` - `cpt-cf-oagw-nfr-observability`
  - [ ] `p1` - `cpt-cf-oagw-nfr-high-availability`

  `cpt-cf-oagw-nfr-high-availability` is covered by the single-executable deployment posture, the health/readiness surface, and the cache-invalidation and flush behavior that keeps the hot path consistent; the circuit breaker itself is DESIGN §4.7 future work and only its metric surface is in scope.

- **Design Principles Covered**:

  None — this entry introduces no new design principle; it instruments the principles implemented by 2.4 and 2.5.

- **Design Constraints Covered**:

  None — no DESIGN constraint is introduced by this entry; `cpt-cf-oagw-constraint-toolkit-deploy` (single-executable deployment) is covered by 2.1.

- **Domain Model Entities**:
  - `CacheKey` — `upstream:{tenant_id}:{alias}`, `route:{upstream_id}:{method}:{path_prefix}`, `plugin:{plugin_id}`
  - `CPState` / `DPState` — cache, shared HTTP client, and rate-limiter ownership boundaries

- **Design Components**:

  - `p2` - `cpt-cf-oagw-component-model`
    - Metrics recorder and registry (`infra/metrics.rs`)
    - Audit logger emitting structured JSON to stdout
    - Control Plane L1 cache with write-side invalidation
    - Data Plane L1 hot-config cache with explicit flush
    - Health and readiness surface
    - Per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management`, the Control Plane is authoritative and the Data Plane L1 is an optimization layer

- **API**:
  - GET /metrics — registered by this entry (admin-only), and deliberately outside the gear-relative `/oagw/v1` prefix recorded in graded deviation 1
  - ToolKit `RestApiCapability` healthcheck hook (gear health and readiness surface; the path is provided by the framework)

- **Sequences**:

  None — the cache lookup and flush steps are part of `cpt-cf-oagw-seq-proxy-flow`, which is owned by 2.4.

- **Data**:

  None — caches and metrics are in-memory state per `cpt-cf-oagw-adr-data-plane-caching` and `cpt-cf-oagw-adr-state-management`, so no documented schema table is owned by this entry.

---

## 3. Feature Dependencies

Depends On lists direct dependencies only; transitive reachability follows the graph above.

```text
cpt-cf-oagw-feature-gear-foundation
    ↓
    ├─→ cpt-cf-oagw-feature-upstream-management
    │       ↓
    │       └─→ cpt-cf-oagw-feature-route-management
    │               ↓
    │               └─→ cpt-cf-oagw-feature-request-proxy
    │                       ↓
    │                       ├─→ cpt-cf-oagw-feature-error-handling
    │                       ├─→ cpt-cf-oagw-feature-plugin-system
    │                       ├─→ cpt-cf-oagw-feature-rate-limiting
    │                       ├─→ cpt-cf-oagw-feature-cors
    │                       └─→ cpt-cf-oagw-feature-observability-and-operability
    ├─→ cpt-cf-oagw-feature-route-management           (direct edge: repository boundary and merge engine)
    └─→ cpt-cf-oagw-feature-request-proxy              (direct edge: gear wiring and config model)
```

**Dependency Rationale**:

- `cpt-cf-oagw-feature-upstream-management` requires `cpt-cf-oagw-feature-gear-foundation`: upstream CRUD needs the gear wiring, the `OagwConfig` model, the domain types, the repository traits and their in-memory implementation, and GTS type provisioning before an upstream record can be stored or served.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-gear-foundation`: route CRUD uses the same repository boundary, DTO conventions, and merge engine.
- `cpt-cf-oagw-feature-route-management` requires `cpt-cf-oagw-feature-upstream-management`: every route references an `upstream_id` that must resolve to an existing, tenant-owned upstream, so the upstream store and its uniqueness rules must exist first.
- `cpt-cf-oagw-feature-request-proxy` requires `cpt-cf-oagw-feature-upstream-management` and `cpt-cf-oagw-feature-route-management`: the proxy path resolves an upstream by alias and then matches a route, so both configuration surfaces must be populated and queryable before a request can be forwarded. It also requires `cpt-cf-oagw-feature-gear-foundation` directly: the handler registration, `OagwConfig`, domain types, and repository boundary it consumes are foundation deliverables.
- `cpt-cf-oagw-feature-error-handling` requires `cpt-cf-oagw-feature-request-proxy`: the error contract is exercised by the proxy path (timeouts, upstream failures, target-host validation, streaming aborts) and must be verified against real proxy outcomes.
- `cpt-cf-oagw-feature-plugin-system` requires `cpt-cf-oagw-feature-gear-foundation` transitively (via `cpt-cf-oagw-feature-request-proxy`, which lists it directly): the plugin traits, registries, and `plugins.items[]` bindings are resolved against the domain types and configuration merge engine.
- `cpt-cf-oagw-feature-plugin-system` requires `cpt-cf-oagw-feature-request-proxy`: the plugin chain executes inside the proxy pipeline at defined points (Auth → Guards → Transform(request) → upstream call → Transform(response/error)), so the extension points must exist first.
- `cpt-cf-oagw-feature-rate-limiting` requires `cpt-cf-oagw-feature-request-proxy`: the rate check runs on the proxy hot path and its `429` response must be rendered through the proxy response pipeline.
- `cpt-cf-oagw-feature-cors` requires `cpt-cf-oagw-feature-request-proxy`: preflight handling and actual-request origin enforcement are both behaviors of the proxy handler and depend on upstream resolution.
- `cpt-cf-oagw-feature-observability-and-operability` requires `cpt-cf-oagw-feature-gear-foundation` transitively (via `cpt-cf-oagw-feature-request-proxy`, which lists it directly): the caches and metrics are wired during gear initialization alongside the services they instrument.
- `cpt-cf-oagw-feature-observability-and-operability` requires `cpt-cf-oagw-feature-request-proxy`: metrics, audit records, and the Data Plane hot-config cache are keyed by proxy traffic and flushed on configuration writes.
- `cpt-cf-oagw-feature-error-handling`, `cpt-cf-oagw-feature-rate-limiting`, and `cpt-cf-oagw-feature-cors` are independent of each other and can be developed in parallel once `cpt-cf-oagw-feature-request-proxy` exists.
- `cpt-cf-oagw-feature-observability-and-operability` and `cpt-cf-oagw-feature-plugin-system` are independent of each other and can be developed in parallel; `cpt-cf-oagw-feature-observability-and-operability` can additionally start its Control Plane cache and metrics work as soon as `cpt-cf-oagw-feature-gear-foundation` lands, without waiting for the proxy path.

