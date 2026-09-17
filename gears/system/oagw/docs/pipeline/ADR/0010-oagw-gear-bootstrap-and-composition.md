---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Gears Platform Team
---

# OAGW Gear Bootstrap and Composition — Platform Registration, Gear-Relative Route Mounting, Capabilities, and Module Layout


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Pingora-based data plane (`infra/proxy`)](#pingora-based-data-plane-infraproxy)
  - [Axum-only reverse proxy](#axum-only-reverse-proxy)
  - [Gear-relative route registration (`/oagw/v1/...`)](#gear-relative-route-registration-oagwv1)
  - [Embedding the host prefix (`/api/oagw/v1/...`)](#embedding-the-host-prefix-apioagwv1)
  - [Platform gear macro registration](#platform-gear-macro-registration)
  - [Hand-rolled registration and linkage](#hand-rolled-registration-and-linkage)
  - [Capabilities `db`, `stateful`, `rest`](#capabilities-db-stateful-rest)
  - [Capabilities `system`, `db`, `rest` with `post_init` provisioning](#capabilities-system-db-rest-with-post_init-provisioning)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-bootstrap-and-composition`
## Context and Problem Statement

The `oagw` workspace member (`gears/system/oagw/oagw/`, package `cf-gears-oagw`, lib `oagw`, v0.4.0) is fully wired — `oagw` feature flag, workspace dependency mapping, and an e2e configuration block — but its `src/lib.rs` is empty, so nothing registers the gear with the host runtime and every request to its API surface is answered with the host's unhandled-path response. The product behaviour (routing matrix, plugin runtime, rate limiting, CORS, caching, state ownership, error-source semantics, OAuth2 plugin, required-headers guard) is already decided by the authoritative ADRs 0001-0009; this ADR records the implementation-mapping decision only: how the gear is bootstrapped and composed into the workspace — registration mechanism, lifecycle wiring, route-prefix composition, capability and dependency declarations, configuration model, module layout, and the data-plane engine.

The decision answers the question: how does the `oagw` crate become a live, routable, healthy gear in the `cf-gears-example-server` host while remaining faithful to the authoritative contract and to sibling-gear conventions (types-registry, account-management, api-gateway)?

Scope and boundaries of this decision: it covers bootstrap and composition only — registration mechanism, lifecycle wiring, route-prefix composition, capability/dependency declarations, the configuration model, module layout, and the data-plane engine choice. Product behaviour (routing matrix, plugin traits, rate-limit algorithms, CORS rules, the error catalog, OAuth2 plugin internals) stays owned by the authoritative ADRs 0001-0009 and the authoritative DESIGN, so this record deliberately does not restate or re-open any of it.

## Decision Drivers

* Host composition contract: the api-gateway gear nests every gear router under its configured `prefix_path` (`ApiGateway::apply_prefix`); `config/e2e-local.yaml` sets no prefix, so gear-relative routes are served directly at `/oagw/v1/...`, and the gear must never embed `/api` or any host prefix itself.
* Platform convention: sibling gears register through the platform `#[toolkit::gear(...)]` macro (which expands to `toolkit::inventory::submit!` in `libs/toolkit-macros`), implement toolkit traits, and declare `deps`/`capabilities` declaratively; the `oagw` feature link in `registered_gears.rs` consumes exactly this discovery path.
* DoD build/start gate: the crate must compile with the exact e2e feature set under the workspace lint denials, and the server must start on `config/e2e-local.yaml` and report healthy (`/healthz` on :8086).
* Contract fidelity at implementation level: this ADR maps onto the authoritative ADRs 0001-0009 and the authoritative PRD/DESIGN; it neither restates nor alters behaviour they already decide.
* Reuse over reimplementation: the crate manifest already declares `pingora-proxy`, `pingora-core`, `pingora-load-balancing`, and `pingora-http`; the data plane should build on them rather than introduce a new transport.
* Change-surface realism: declared capabilities and deps must match what the e2e host actually provides; availability of a host database-capability slot for the gear is resolved at implementation (pipeline PRD §3.1/§11).

## Considered Options

1. **Pingora-based data plane** in `infra/proxy` using the declared pingora-* crates, versus an **axum-only reverse proxy** built on a plain hyper client.
2. **Gear-relative route registration** (`/oagw/v1/...`) composed by the host's prefix nesting, versus **embedding the host prefix** in the gear's own routes (`/api/oagw/v1/...`).
3. **Platform `#[toolkit::gear(...)]` macro registration** with inventory submit and toolkit trait impls, versus **hand-rolled registration** and linkage.
4. **Capabilities `[db, stateful, rest]`** with deps on authz-resolver/types-registry/tenant-resolver/credstore, versus **`[system, db, rest]`** with `post_init` provisioning.

## Decision Outcome

Chosen option: "Platform `#[toolkit::gear(...)]` macro registration, gear-relative route roots, a pingora-based data plane, and capabilities `[db, stateful, rest]`", because it is the only combination that satisfies the host composition contract (option 2), follows the registration convention the host registry is built on (option 3), reuses the already-declared pingora dependencies and supports the authoritative latency and error-source semantics (option 1), and matches the gear's actual runtime needs without claiming the `system`/`post_init` lifecycle that types-registry specifically owns (option 4).

Specifically: a `OagwGear` struct annotated `#[toolkit::gear(name = "oagw", deps = [authz_resolver, types_registry, tenant_resolver, credstore], capabilities = [db, stateful, rest])]` implements `Gear::init` (config via `ctx.config_or_default::<OagwConfig>()`, database via `ctx.db_required()`, platform SDK clients resolved and/or published via `ctx.client_hub().register/get` for the `-sdk` traits), `DatabaseCapability::migrations` (oagw_* control-plane tables per the authoritative DESIGN §3.6), `RestApiCapability::register_rest` (management + proxy routes), and `RunnableCapability::{start,stop}` (data-plane service and p2 background concerns). Routes are registered gear-relative (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy/...`) with no `/api` segment; the host's `apply_prefix` nests them under `prefix_path` (empty in e2e → served at `/oagw/v1/...`). Module layout follows the authoritative DESIGN §3.2 DDD-Light structure (`api/rest`, `domain`, `infra` including `infra/proxy` on pingora, `infra/type_provisioning.rs` for GTS catalog registration).

### Consequences

* Good, because registration and route mounting follow the exact convention the host registry and the `oagw` feature link exercise, so the gear is discovered, loaded, configured, and routable without any host change.
* Good, because gear-relative `/oagw/v1/...` routes compose correctly under `ApiGateway::apply_prefix` for every deployment — served directly in e2e, `/api`-nested only where a deployment sets `prefix_path: "/api"` — keeping the gear host-agnostic.
* Good, because the pingora-based data plane provides pooled, load-balanced, streaming transport aligned with the authoritative latency NFR and with ADR 0008's `pingora-memory-cache` token cache, all from the already-declared dependency set.
* Good, because capabilities `[db, stateful, rest]` map one-to-one onto the authoritative roles: db for `oagw_*` control-plane persistence, rest for the management/proxy API, stateful for the data-plane caches and limiters the authoritative ADRs 0005/0006 place in the gear.
* Bad, because pingora runs its own runtime and sockets; the gear must bridge them into the Axum host inside `infra/proxy` (request/response mapping, hop-by-hop handling, `X-OAGW-Error-Source` wiring per ADR 0007), and that bridging complexity must stay under test.
* Bad, because declaring `db` couples startup to the host's database-capability slot; per the pipeline PRD the backend is resolved at implementation, and a persistence-free MVP is acceptable only if authoritative management semantics and the DoD still hold.

### Confirmation

Confirmed when: (1) the DoD build (`cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"`) succeeds and the server starts on `config/e2e-local.yaml` with `/healthz` healthy on :8086; (2) requests to `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, and `/oagw/v1/proxy/...` are served by the gear (not the host's unhandled path) at those gear-relative paths, with no `/api` segment in the e2e mount; (3) code review confirms the gear struct, macro metadata (deps/capabilities), trait impls, and DDD-Light module layout match the sibling-gear conventions and the authoritative DESIGN §3.2; (4) crate-level unit/integration tests cover registration, lifecycle, config parsing/defaults, migrations, and route mounting; and (5) review confirms this record introduces no behavioural decisions that the authoritative ADRs 0001-0009 already own.

## Pros and Cons of the Options

### Pingora-based data plane (`infra/proxy`)

* Good, because the pingora-* crates are already declared in the manifest (with rustls), adding no new dependency surface.
* Good, because connection pooling, load balancing, and multi-endpoint selection per the authoritative ADR 0001 matrix come from `pingora-load-balancing` rather than hand-rolled logic.
* Good, because streaming (HTTP/SSE/WebSocket per the authoritative contract) and ASYNC client behaviour map onto pingora proxy primitives, supporting `cpt-cf-oagw-fr-data-plane-proxy` and streaming requirements.
* Good, because ADR 0008 already adopts `pingora-memory-cache` for the token cache, keeping transport and caching within one ecosystem.
* Neutral, because pingora runs its own runtime; the gear bridges it into the Axum host instead of running pingora as a standalone server.
* Bad, because the bridge and pingora-specific error mapping (gateway vs upstream per ADR 0007) add integration complexity that must be owned and tested in `infra/proxy`.

### Axum-only reverse proxy

* Good, because it stays entirely within Axum's request/response model, reducing integration surface.
* Bad, because the declared pingora dependencies would go unused, contradicting the manifest and ADR 0008's `pingora-memory-cache` choice.
* Bad, because pooling, load balancing, and adaptive HTTP/2 per the authoritative DESIGN would need reimplementation on a bespoke hyper client, raising risk against the <10ms p95 overhead NFR rather than lowering it.

### Gear-relative route registration (`/oagw/v1/...`)

* Good, because it matches the host composition contract exactly: `ApiGateway::apply_prefix` nests the gear router under `prefix_path`, which is empty in `config/e2e-local.yaml` (routes served at `/oagw/v1/...` directly).
* Good, because the gear stays host-agnostic: the same crate serves under any prefix a deployment chooses, with the `/api` segment supplied only by the host.
* Good, because auth policy in the api-gateway matches on unprefixed `OperationBuilder` paths, so gear-relative registration composes cleanly with the host's auth layer.
* Neutral, because the authoritative PRD/DESIGN spell the surface in the api-gateway-nested form (`/api/oagw/v1/...`); the pipeline PRD explicitly leaves exact mount resolution to design and requires end-to-end reachability of the contract paths to be verified.
* Bad, none decisive — the only cost is that a reader cross-referencing the gear route table against the authoritative path table must recall that the `/api` segment is host-supplied.

### Embedding the host prefix (`/api/oagw/v1/...`)

* Good, because the gear's own route table would then match the authoritative contract spelling verbatim.
* Bad, because with the e2e host's empty `prefix_path` the host does not strip `/api`, so `/api/oagw/v1/...` would 404 and the DoD surface would be unreachable.
* Bad, because embedding a host-owned prefix couples the gear to one host configuration and duplicates the prefix wherever the host does set one (e.g. `/cf/api/oagw/...`).
* Bad, because it violates the api-gateway composition contract (auth matching keyed on unprefixed `OperationBuilder` paths).

### Platform gear macro registration

* Good, because it is the convention the host registry consumes via `toolkit::inventory::submit!` (verified in `libs/toolkit-macros`), proven by types-registry, account-management, and api-gateway; the `oagw` feature link expects exactly this path.
* Good, because deps (`deps = [authz_resolver, types_registry, tenant_resolver, credstore]`) and capabilities (`[db, stateful, rest]`) are declared in the macro, and lifecycle hooks — `Gear::init`, `DatabaseCapability::migrations`, `RestApiCapability::register_rest`, `RunnableCapability::{start,stop}` — are provided by toolkit traits.
* Good, because `ctx.config_or_default::<OagwConfig>()`, `ctx.db_required()`, and `ctx.client_hub().register/get` deliver config injection, the database slot, and the platform `-sdk` client traits with no custom plumbing.
* Neutral, because the macro hides mechanical boilerplate behind codegen; debugging registration occasionally requires reading the expansion.
* Bad, because the gear struct must match the macro's trait surface exactly, so any shape mismatch is a compile error located in generated code.

### Hand-rolled registration and linkage

* Good, because it avoids macro/codegen indirection and gives complete control over wiring.
* Bad, because it would bypass the host's inventory-based discovery, so the gear would not be loaded or its routes mounted through `registered_gears.rs`; the API surface would remain unreachable.
* Bad, because it diverges from sibling-gear conventions, introducing a second registration path the platform must maintain.
* Bad, because it forfeits the declarative deps/capabilities metadata the host uses to sequence initialization and feature gating.

### Capabilities `db`, `stateful`, `rest`

* Good, because `db` matches the authoritative DESIGN's relational persistence of the `oagw_*` control-plane tables via `DatabaseCapability::migrations` on the multi-backend `toolkit-db` constraint.
* Good, because `rest` matches the management and proxy REST surface registered through `RestApiCapability::register_rest`.
* Good, because `stateful` matches the data-plane caches and limiters that authoritative ADRs 0005/0006 place in the gear (DP-owned L1 config cache, in-memory rate limiters) without claiming early-init system ordering.
* Neutral, because whether the e2e host actually provisions a database-capability slot for the gear is resolved at implementation (pipeline PRD §11); a persistence-free MVP is acceptable only if authoritative management semantics and the DoD hold.

### Capabilities `system`, `db`, `rest` with `post_init` provisioning

* Good, because `post_init` could materialize GTS entities after all gears have initialized (relevant to the DESIGN's future registry-only mode, §4.7).
* Bad, because `system` capability marks early-init core-infrastructure gears (the types-registry role); OAGW is a normal service gear, and claiming it would force premature ordering and readiness semantics on a data-plane gear.
* Bad, because OAGW's GTS catalog registration already happens in `Gear::init` via the types-registry SDK (`infra/type_provisioning.rs`), so no system-level post-init step is needed for the current change surface.

## More Information

* **Change requirements (this decision's owner)**: `gears/system/oagw/docs/pipeline/PRD.md` — DoD build/start gates, §3.1 host-composed route mounting, §5.1 gear registration/lifecycle/configuration, §11 persistence assumption.
* **Authoritative contract (behaviour source of truth, not restated here)**: `gears/system/oagw/docs/PRD.md`, `gears/system/oagw/docs/DESIGN.md`, `gears/system/oagw/docs/schemas/route.v1.schema.json`, `gears/system/oagw/docs/schemas/upstream.v1.schema.json`, and `gears/system/oagw/docs/ADR/0001-request-routing.md` through `0009-required-headers-guard-plugin.md` (0007 for error-source semantics, 0008 for the OAuth2 token cache and gear-level config keys, 0005/0006 for caching and state ownership).
* **Gear-level config beyond the e2e block**: ADR 0008 defines two additive `OagwConfig` keys — `token_cache_ttl_secs` (default 300) and `token_cache_capacity` (default 10,000) — bundled into the token-cache config threaded through the data-plane service to the auth-plugin registry.
* **Sibling-gear worked examples**: `gears/system/types-registry/types-registry/src/gear.rs`, `gears/system/account-management/account-management/src/gear.rs`, `gears/system/api-gateway/src/gear.rs` (`apply_prefix`, `normalize_prefix_path`).
* **e2e wiring**: `config/e2e-local.yaml` (the `oagw` gear block with `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`; api-gateway with no `prefix_path`), `config/e2e-features.txt`, `apps/cf-gears-example-server/src/registered_gears.rs`.
* **Toolkit macro expansion**: `libs/toolkit-macros/src/lib.rs` (`::toolkit::inventory::submit!`).

## Traceability

- **PRD (change surface)**: [./PRD.md](../PRD.md)
- **Authoritative contract**: [../../PRD.md](../../PRD.md), [../../DESIGN.md](../../DESIGN.md), [../../ADR/](../../ADR/), [../../schemas/](../../schemas/)

This decision directly addresses the following pipeline PRD requirements:

* `cpt-cf-oagw-fr-gear-registration` — the `#[toolkit::gear(...)]` entry point registers the crate with the host registry so the gear is discovered and loaded behind the `oagw` feature.
* `cpt-cf-oagw-fr-gear-lifecycle` — the `Gear::init`/migrations/`register_rest`/`start`/`stop` wiring makes the lifecycle complete, fail-loud on misconfiguration, and health-contributing.
* `cpt-cf-oagw-fr-gear-configuration` — `OagwConfig` parsed from the `gears.oagw.config` YAML block (`proxy_timeout_secs`, `allow_http_upstream` default false, `ssrf_policy.enabled` default true) with safe defaults and additive, startup-validated fields.
* `cpt-cf-oagw-fr-route-serving` — gear-relative `/oagw/v1/...` route roots, composed under the host prefix, serve the whole management and proxy surface end to end.
* `cpt-cf-oagw-fr-data-plane-proxy` — the pingora-based data plane in `infra/proxy` provides pooled, load-balanced, streaming transport implementing the already-decided authoritative ADR 0001 and 0007 semantics.
* `cpt-cf-oagw-interface-gear-registration` — the gear macro registration surface is the workspace gear-registration convention the host links through `registered_gears.rs`.
* `cpt-cf-oagw-interface-config-model` — `OagwConfig` is the public serde configuration contract (current keys plus the additive token-cache keys from ADR 0008) between host configuration authors and the gear.
