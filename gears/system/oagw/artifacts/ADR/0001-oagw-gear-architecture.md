---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# ADR-0001: OAGW as a Standard toolkit::gear REST Component with In-Memory Control Plane Repositories


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Standard toolkit::gear REST component with in-memory control-plane repositories](#standard-toolkitgear-rest-component-with-in-memory-control-plane-repositories)
  - [DB-backed control-plane repositories now (SeaORM / toolkit-db)](#db-backed-control-plane-repositories-now-seaorm--toolkit-db)
  - [Stateful lifecycle gear with a background data-plane task](#stateful-lifecycle-gear-with-a-background-data-plane-task)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-gear-architecture`
## Context and Problem Statement

The `oagw` gear (`/app/gears/system/oagw/oagw`, crate `cf-gears-oagw`) has an empty `src/lib.rs` and must be implemented in this repository against its own accepted PRD, DESIGN.md, and docs ADRs. The workspace has no OAGW database migration scaffold and the gear may not add any dependency beyond the workspace lockfile. The question is how to shape the gear — registration, capabilities, module layout, and control-plane state — so that management CRUD and proxy behavior are reachable on the `api-gateway` surface with acceptable MVP risk.

## Decision Drivers

* Platform convention: gears register via `#[toolkit::gear]` with a capability list; `api-gateway` is the sole `rest_host`, gears never nest their own routers, and the host applies `prefix_path`
* Workspace constraint (`cpt-cf-oagw-nfr-build-constraints`): no new dependencies beyond the workspace lockfile
* No OAGW DB migration scaffold exists in the workspace — control-plane persistence needs a deliberate MVP choice
* Module layout is already specified by DESIGN.md ("Gear Structure", module tree) and should not be reinvented
* MVP risk containment: full in-process behavior with graceful future evolution

## Considered Options

1. **Standard toolkit::gear REST component with in-memory control-plane repositories** — registered with `capabilities = [rest]` (not `rest_host`), module tree per DESIGN.md, and tenant-scoped upstream/route/plugin state held in in-memory repositories behind domain repository traits
2. **DB-backed control-plane repositories now** — SeaORM + `toolkit-db` persistence for upstreams/routes/plugins in the MVP
3. **Stateful lifecycle gear with a background data-plane task** — a `RunnableCapability` gear owning a long-running proxy/worker task

## Decision Outcome

Chosen option: "Standard toolkit::gear REST component with in-memory control-plane repositories", because it follows the platform gear pattern exactly (see `types-registry` gear), respects the no-new-dependencies and no-migration-scaffold constraints, and keeps the data plane request-driven so no background task is required.

The gear is declared as a standard Constructor Fabric gear: `#[toolkit::gear(name = "oagw", capabilities = [rest], deps = [...])]` where `deps` names the platform core gear set the design depends on (types-registry, cred-store, tenant-resolver, authz-resolver), matching DESIGN.md §3.4 and the SDK crates already declared in `oagw/Cargo.toml`. It is a plain REST gear on the `api-gateway` surface: `Gear::init` loads `OagwConfig` from the `oagw` config section via `ctx.config_or_default()` (lenient fallback to `Default` per toolkit semantics), and `RestApiCapability::register_rest` registers the management and proxy routes on the host router (the host applies `prefix_path`; the gear never nests its own router). The module tree follows DESIGN.md: `gear.rs`, `config.rs`, `api/rest/{handlers,routes,dto,error,extractors}`, `domain/{services,plugin,dto,repo,error}`, `infra/{proxy,storage,plugin,type_provisioning}`.

Control-plane state (upstreams, routes, plugins) is held in tenant-scoped in-memory repositories (`DashMap`/`parking_lot` — both already declared) behind domain repository traits (`UpstreamRepository`, `RouteRepository`, plugin storage), so the domain services program against trait contracts and every CRUD write is scoped to the calling tenant.

Alternatives rejected:

* **DB-backed repos with SeaORM/toolkit-db now**: rejected because there is no OAGW migration scaffold in the workspace, adding persistence would require dependency and migration wiring (conflicting with `cpt-cf-oagw-nfr-build-constraints`), and the DB layer would raise MVP risk without contributing required proxy behavior.
* **Stateful lifecycle gear with a background data-plane task**: rejected because proxying can be request-driven in-crate — the axum proxy handler invokes the data-plane service directly per request, with no long-running task needed. This keeps the gear a plain REST gear on the `api-gateway` surface (no `RunnableCapability`, no `rest_host`).

### Consequences

* Good, because full upstream/route/plugin CRUD works in-process and is testable without an external database, with tenant scoping enforced at the repository boundary
* Good, because zero new dependencies and zero new migration scaffolding — the build-constraints NFR is preserved
* Good, because repository traits keep the domain clean (DDD-Light layering per DESIGN.md) and a future ADR can swap the in-memory implementations for DB-backed ones behind the same traits
* Bad, because control-plane state is in-memory and lost on restart, and there is no shared multi-instance state — acceptable for the MVP, which already assumes per-instance state
* Neutral, because route registration on the host router (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy`) matches accepted ADR 0001's routing table

### Confirmation

Confirmed when:

* Code review verifies `lib.rs` exposes a gear declared with `#[toolkit::gear(name = "oagw", capabilities = [rest], ...)]`, `Gear::init` loads `OagwConfig` via `config_or_default()`, and `RestApiCapability::register_rest` returns the host router with management plus proxy routes registered
* The module tree matches DESIGN.md and domain services depend on repository traits implemented by the in-memory repositories
* The gear builds and runs under the workspace e2e feature set with no additions to the lockfile, and management CRUD is exercised in-process in integration tests

## Pros and Cons of the Options

### Standard toolkit::gear REST component with in-memory control-plane repositories

Registered with `capabilities = [rest]`, following the `types-registry` gear as the worked example.

* Good, because it matches platform conventions (registration, config loading, REST registration) requiring no new machinery
* Good, because it satisfies the no-new-dependencies and no-migration-scaffold constraints
* Good, because in-memory repositories behind traits allow the storage implementation to evolve without touching domain services
* Bad, because state does not survive restarts — durability is deferred to a future repository implementation

### DB-backed control-plane repositories now (SeaORM / toolkit-db)

Persist upstreams, routes, and plugins to PostgreSQL/MySQL/SQLite in the MVP, as DESIGN.md's original component model envisioned.

* Good, because durable, multi-instance-shared control-plane state
* Good, because aligns with the portable multi-SQL constraint and DESIGN.md §3.6 schema direction
* Bad, because no OAGW migration scaffold exists in the workspace, so migrations must be authored from scratch
* Bad, because it introduces dependency and wiring work that conflicts with the no-new-dependencies NFR
* Bad, because it raises MVP risk without changing any required proxy behavior

### Stateful lifecycle gear with a background data-plane task

A gear carrying `RunnableCapability` and a long-running task that processes proxy traffic or refreshes state.

* Good, because it would centralize background work in one owner
* Bad, because proxying is naturally request-driven — a background task adds a hop and lifecycle complexity for no MVP benefit
* Bad, because it deviates from the plain REST gear shape used by comparable gears on the `api-gateway` surface

## More Information

These in-repo pipeline ADRs complement — and do not supersede — the gear's accepted behavioral records in `docs/ADR/`:

- [ADR 0001: Request Routing](../../docs/ADR/0001-request-routing.md) — authoritative routing table and control-plane/data-plane separation this decision satisfies
- [ADR 0005: Control Plane Caching](../../docs/ADR/0005-data-plane-caching.md) — accepted caching direction; not required for MVP
- [ADR 0006: State Management](../../docs/ADR/0006-state-management.md) — accepted state ownership; per-instance data-plane state is the MVP stance
- [DESIGN.md](../../docs/DESIGN.md) — component model and "Gear Structure" module tree this decision follows
- Platform worked examples: `/app/gears/system/types-registry/types-registry/src/gear.rs` (registration, `Gear::init`, `RestApiCapability::register_rest`), `/app/gears/system/types-registry/types-registry/src/config.rs` (config struct idiom), `/app/gears/system/api-gateway/src/gear.rs` (sole `rest_host`; `prefix_path` applied by the host)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-gear-registration` — Gear registers as a host component under the `/oagw/v1` base path and reads the `oagw` config section at startup
* `cpt-cf-oagw-fr-upstream-mgmt` — Tenant-scoped upstream CRUD served by the control plane backed by in-memory repositories
* `cpt-cf-oagw-fr-route-mgmt` — Tenant-scoped route CRUD served by the control plane
* `cpt-cf-oagw-fr-plugin-lifecycle` — Plugin create/list/delete with immutability and in-use protection held in the in-memory plugin repository
* `cpt-cf-oagw-nfr-build-constraints` — No new dependencies; builds under the e2e feature set
* `cpt-cf-oagw-nfr-testability` — In-process repositories make the required behavior unit/integration testable
* `cpt-cf-oagw-nfr-multi-tenancy` — Repository boundary enforces tenant scoping at the data layer
* `cpt-cf-oagw-interface-host-registration` — Registration contract satisfied via `#[toolkit::gear]`
* `cpt-cf-oagw-interface-management-api` — Management endpoints registered on the host router
* `cpt-cf-oagw-interface-proxy-api` — Proxy endpoint registered on the host router (data plane)
* `cpt-cf-oagw-usecase-configure-upstream` — Upstream creation/validation/persistence flow supported in-process
* `cpt-cf-oagw-usecase-configure-route` — Route creation/validation flow supported in-process
* `cpt-cf-oagw-usecase-manage-plugin` — Custom plugin lifecycle flow supported in-process
