# Feature: OAGW Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Startup and Registration](#gear-startup-and-registration)
  - [Health Reporting](#health-reporting)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Configuration Load and Validation](#configuration-load-and-validation)
  - [Lifecycle Wiring (Migrations, REST Mounting, Runnable Service)](#lifecycle-wiring-migrations-rest-mounting-runnable-service)
- [4. States (CDSL)](#4-states-cdsl)
  - [OagwGear Lifecycle State Machine](#oagwgear-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Registration](#gear-registration)
  - [Configuration Model and Defaults](#configuration-model-and-defaults)
  - [Lifecycle Wiring](#lifecycle-wiring)
  - [Gear-Relative Route Mounting](#gear-relative-route-mounting)
  - [Health Contribution](#health-contribution)
  - [Crate-Level Test Harness](#crate-level-test-harness)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation`

- [x] `p1` - `cpt-cf-oagw-feature-gear-foundation`
## 1. Feature Context

### 1.1 Overview

The gear foundation turns the empty `oagw` crate into a live, configured, healthy gear inside the `cf-gears-example-server` host: it declares `OagwGear` through the platform `#[toolkit::gear(...)]` macro, implements the complete lifecycle (config load, database slot acquisition, SDK client resolution, migrations and route registration, start/stop), reads and validates the host-injected `OagwConfig` with safe defaults, mounts all API routes gear-relative under `/oagw/v1/...`, and contributes readiness to the host health endpoint so the DoD build/start/health gates hold.

### 1.2 Purpose

The workspace member `gears/system/oagw/oagw/` has an intact manifest and wiring but an empty `src/lib.rs`. As a result nothing registers the gear with the host registry, none of its behavior is reachable, and every request to its API surface is answered with the host's unhandled-path response while the server still builds and reports healthy. This feature removes that silent failure: it makes the gear discoverable behind the `oagw` feature and lays the foundation (registration macro + trait impls, `OagwConfig` model with defaults and validation, lifecycle wiring for migrations/`register_rest`/start/stop, gear-relative route mounting, health contribution) on which the control plane, data plane, and type-provisioning features are built. The crate-level test harness established here verifies registration, lifecycle, config parsing/defaults, migrations, and route mounting without encroaching on the reserved acceptance path `testing/e2e/gears/oagw/`.

Success criteria: the DoD build command exits 0 under the workspace lint denials; the server starts on `config/e2e-local.yaml`, binds :8086, and `/healthz` reports healthy; the gear is registered and routable; invalid configuration prevents startup loudly.

**Requirements**: `cpt-cf-oagw-fr-gear-registration`, `cpt-cf-oagw-fr-gear-lifecycle`, `cpt-cf-oagw-fr-gear-configuration`, `cpt-cf-oagw-fr-route-serving`, `cpt-cf-oagw-nfr-build-integration`, `cpt-cf-oagw-nfr-startup-health`, `cpt-cf-oagw-nfr-test-coverage`

**Principles**: `cpt-cf-oagw-principle-gear-relative-routing`, `cpt-cf-oagw-principle-fail-loud-config`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-host-runtime` | Discovers the gear through the inventory-based `#[toolkit::gear]` registration, drives `Gear::init` and the capability lifecycle (migrations, `register_rest`, start/stop), and aggregates gear readiness into the host health endpoint on :8086 |
| `cpt-cf-oagw-actor-gateway-operator` | Authors the `gears.oagw.config` host block consumed by `OagwConfig`; depends on fail-loud startup validation so a misconfigured block is never silently ignored |
| `cpt-cf-oagw-actor-gts-registry` | Receives the GTS type catalog registration executed during `Gear::init` (registration content is delivered by feature-type-provisioning; the invocation point and its fail-loud surfacing live in this feature's lifecycle) |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [0010-oagw-gear-bootstrap-and-composition.md](../ADR/0010-oagw-gear-bootstrap-and-composition.md) (`cpt-cf-oagw-adr-bootstrap-and-composition`)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: None
- **Interfaces**: `cpt-cf-oagw-interface-gear-registration`, `cpt-cf-oagw-interface-config-model`
- **Components**: `cpt-cf-oagw-component-gear-root`, `cpt-cf-oagw-component-config`
- **Sequences**: `cpt-cf-oagw-seq-startup-registration`, `cpt-cf-oagw-seq-health-reporting`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-startup-registration`, `cpt-cf-oagw-usecase-health-reporting`

### Gear Startup and Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-startup-registration`

**Actor**: `cpt-cf-oagw-actor-host-runtime`

**Success Scenarios**:
- The server built with the e2e feature set (including `oagw`) starts, discovers the gear, drives the lifecycle to ready, and reports healthy on `/healthz`.
- All management and proxy routes are mounted gear-relative under `/oagw/v1/...` and served by the gear (never the host's unhandled-path response).

**Error Scenarios**:
- Invalid `OagwConfig` (unknown key, out-of-range value, or semantic violation) rejects startup loudly with a clear error; the gear never boots half-configured.
- A required platform dependency or the database slot is unavailable at startup; startup fails loudly per the lifecycle contract.

**Steps**:
1. [x] - `p1` - Host discovers `OagwGear` through the `#[toolkit::gear(name = "oagw", deps = [authz_resolver, types_registry, tenant_resolver, credstore], capabilities = [stateful, rest])]` inventory registration (`toolkit::inventory::submit!`) - `inst-inventory-submit`
2. [x] - `p1` - Host invokes `Gear::init(ctx)` with the host-injected context - `inst-gear-init`
3. [x] - `p1` - Gear loads configuration via `API: (in-process) ctx.config_or_default::<OagwConfig>()` from the `gears.oagw.config` block - `inst-config-load`
4. [x] - `p1` - **IF** the config block is invalid (unknown keys, out-of-range values, semantic violations) - `inst-config-invalid`
   1. [x] - `p1` - **RETURN** a clear startup error; fail startup loudly (no half-configured boot) - `inst-fail-loud`
5. [x] - `p1` - **ELSE** proceed with all fields defaulted when the block is minimal or absent - `inst-config-valid`
6. [x] - `p1` - Gear builds the persistence-free control-plane repository in `Gear::init` (in-memory, seeded from the validated `OagwConfig`; no `db` capability, no `ctx.db_required()` — ADR 0010 persistence-free MVP clause) - `inst-db-required`
7. [x] - `p1` - Gear registers and gets the platform `-sdk` client traits (`authz_resolver_sdk`, `types_registry_sdk`, `tenant_resolver_sdk`, `credstore_sdk`) via `ctx.client_hub()` - `inst-client-hub`
8. [x] - `p1` - Gear registers the GTS type catalog via the types-registry SDK (`infra/type_provisioning.rs`; content owned by feature-type-provisioning) - `inst-gts-catalog`
9. [x] - `p1` - Gear validates the persistence-free control-plane repository invariants at init (unique aliases, no dangling upstream refs); no `DatabaseCapability::migrations` is registered (persistence-free MVP per ADR 0010) - `inst-migrations`
10. [x] - `p1` - Host invokes `RestApiCapability::register_rest`; gear mounts the management and proxy routers gear-relative under `/oagw/v1/...` with no embedded `/api` segment - `inst-register-rest`
11. [x] - `p1` - Host invokes `RunnableCapability::start`; gear starts the data-plane service and its sockets - `inst-start`
12. [x] - `p1` - Gear signals readiness only after the data-plane service is running - `inst-ready`
13. [x] - `p1` - **RETURN** init complete; host health aggregates the registered, configured, running gear - `inst-init-complete`

### Health Reporting

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-foundation-health-reporting`

**Actor**: `cpt-cf-oagw-actor-host-runtime`

**Success Scenarios**:
- A health probe against `/healthz` on :8086 receives a healthy response while the gear is registered, configured, and running.
- Readiness is reported only after init completes and the data-plane service starts; a running-but-unhealthy gateway is not a supported state.

**Error Scenarios**:
- The gear fails to initialize (invalid config or unavailable dependency) and startup fails loudly; the health endpoint never masks a half-configured gateway.

**Steps**:
1. [x] - `p1` - Health probe issues `API: GET /healthz` on the host-composed health surface (`:8086`) - `inst-health-probe`
2. [x] - `p1` - Host queries the gear's ready state (init clean, config validated, migrations and routes registered, data-plane service started) - `inst-readiness-query`
3. [x] - `p1` - **IF** the gear reports ready - `inst-ready-check`
   1. [x] - `p1` - Host aggregates the gear as healthy - `inst-healthy`
4. [x] - `p1` - **ELSE** host surfaces the gear as not ready/unhealthy (a half-initialized gear must not report healthy) - `inst-unhealthy`
5. [x] - `p1` - **RETURN** the health response to the probe - `inst-return-health`

## 3. Processes / Business Logic (CDSL)

### Configuration Load and Validation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-gear-foundation-config-load-validation`

**Input**: host-injected `gears.oagw.config` block (`proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`, additive token-cache keys), possibly minimal or absent

**Output**: validated `OagwConfig` with safe defaults threaded into the data plane, or a startup-rejecting validation error

**Steps**:
1. [x] - `p1` - Parse the gear block with serde into `OagwConfig` (`deny_unknown_fields`, every field `#[serde(default)]`) - `inst-parse`
2. [x] - `p1` - Apply safe defaults: `proxy_timeout_secs = 2`, `allow_http_upstream = false`, `ssrf_policy.enabled = true`, additive `token_cache_ttl_secs = 300`, `token_cache_capacity = 10 000` - `inst-defaults`
3. [x] - `p1` - **IF** unknown keys, out-of-range values, or semantic violations (e.g. invalid SSRF/allowance combinations) are present - `inst-validate`
   1. [x] - `p1` - **RETURN** a clear startup error (fail loud; runtime behavior is driven only by validated configuration) - `inst-return-error`
4. [x] - `p1` - **ELSE** validation succeeds - `inst-valid`
   1. [x] - `p1` - Thread derived settings (token-cache config, timeout, allowances, SSRF policy) into the data-plane service and the plugin registry - `inst-thread-settings`
5. [x] - `p1` - **RETURN** validated `OagwConfig` - `inst-return-valid`

### Lifecycle Wiring (Migrations, REST Mounting, Runnable Service)

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring`

**Input**: initialized `GearCtx` (validated `OagwConfig`, persistence-free control-plane repository, resolved `-sdk` clients)

**Output**: gear fully wired and running (persistence-free control-plane repository wired, gear-relative router mounted, data-plane service started/stopped) with readiness signaled

**Steps**:
1. [x] - `p1` - Wire the persistence-free control-plane repository into the data-plane gate and management handlers (no migrations — persistence-free MVP per ADR 0010; config-seeded in-memory model) - `inst-register-migrations`
2. [x] - `p1` - Register the management and proxy routers via `RestApiCapability::register_rest` at gear-relative roots (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy/...`) with no `/api` segment - `inst-register-rest`
3. [x] - `p1` - Compose mounted routes under the host's `ApiGateway::apply_prefix` (empty prefix in e2e, so routes are served at `/oagw/v1/...`) - `inst-apply-prefix`
4. [x] - `p1` - Implement `RunnableCapability::start` to launch the data-plane service and its sockets - `inst-start`
5. [x] - `p1` - Implement `RunnableCapability::stop` for clean shutdown of the service and sockets - `inst-stop`
6. [x] - `p1` - **IF** the data-plane service fails to start - `inst-start-check`
   1. [x] - `p1` - Gear does not report ready; host health reflects the failure (fail loud) - `inst-not-ready`
7. [x] - `p1` - **ELSE** the service starts - `inst-start-ok`
   1. [x] - `p1` - Gear reports ready and contributes readiness to the host health endpoint - `inst-ready`
8. [x] - `p1` - **RETURN** gear registered, configured, routable, and healthy - `inst-wired`

## 4. States (CDSL)

### OagwGear Lifecycle State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-foundation-lifecycle`

**States**: `Registered`, `Initializing`, `Ready`, `Stopping`, `Stopped`

**Initial State**: `Registered`

**Transitions**:
1. [x] - `p1` - **FROM** Registered **TO** Initializing **WHEN** the host discovers the gear and invokes `Gear::init(ctx)` - `inst-init`
2. [x] - `p1` - **FROM** Initializing **TO** Ready **WHEN** config is validated, the DB slot is acquired, SDK clients resolve, migrations and routes are registered, and the data-plane service starts - `inst-ready`
3. [x] - `p1` - **FROM** Initializing **TO** Stopped **WHEN** startup fails loudly (invalid config or unavailable dependency) - `inst-failed`
4. [x] - `p1` - **FROM** Ready **TO** Stopping **WHEN** the host invokes `RunnableCapability::stop` (shutdown) - `inst-stop`
5. [x] - `p1` - **FROM** Stopping **TO** Stopped **WHEN** the data-plane service and background tasks exit cleanly - `inst-stopped`

## 5. Definitions of Done

### Gear Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-registration`

The system **MUST** declare `OagwGear` via `#[toolkit::gear(name = "oagw", deps = [authz_resolver, types_registry, tenant_resolver, credstore], capabilities = [db, stateful, rest])]` with inventory-based registration (`toolkit::inventory::submit!`) so the host discovers and links the gear behind the `oagw` feature, and implement `Gear::init` covering config load (`ctx.config_or_default::<OagwConfig>()`), the database slot (`ctx.db_required()`), and register/get of the `-sdk` client traits via `ctx.client_hub()`.

**Implements**: `cpt-cf-oagw-flow-gear-foundation-startup-registration`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`, `cpt-cf-oagw-constraint-toolchain`, `cpt-cf-oagw-constraint-locked-deps`

**Touches**: API: `GET /healthz` (host-composed readiness contribution) / DB: `oagw_upstream`, `oagw_route`, `oagw_route_http_match`, `oagw_plugin`, `oagw_upstream_plugin`, `oagw_route_plugin`, `oagw_config` (migration registration surface only; tables owned by feature-control-plane) / Entities: `OagwGear`

### Configuration Model and Defaults

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-config-model`

The system **MUST** define `OagwConfig` (serde, `deny_unknown_fields`, all fields defaulted) with `proxy_timeout_secs` (2), `allow_http_upstream` (false), `ssrf_policy.enabled` (true), and the additive ADR 0008 keys `token_cache_ttl_secs` (300) and `token_cache_capacity` (10 000), validate the block at startup, and reject invalid configuration loudly before booting.

**Implements**: `cpt-cf-oagw-algo-gear-foundation-config-load-validation`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`, `cpt-cf-oagw-constraint-locked-deps`

**Touches**: DB: none / Entities: `OagwConfig`

### Lifecycle Wiring

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-lifecycle-wiring`

The system **MUST** implement `DatabaseCapability::migrations` (registering the `oagw_*` control-plane tables), `RestApiCapability::register_rest` (mounting the management and proxy routers), and `RunnableCapability::{start, stop}` (data-plane service lifecycle), reporting readiness only after the data plane starts.

**Implements**: `cpt-cf-oagw-algo-gear-foundation-lifecycle-wiring`, `cpt-cf-oagw-state-gear-foundation-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`

**Touches**: DB: migration registration surface for the `oagw_*` tables / Entities: `OagwGear`

### Gear-Relative Route Mounting

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-route-mounting`

The system **MUST** mount all management and proxy routes gear-relative under `/oagw/v1/...` with no embedded `/api` or host prefix, composing correctly under the host's `apply_prefix` so the contract paths are reachable end-to-end (served directly at `/oagw/v1/...` in the e2e empty-prefix mount).

**Implements**: `cpt-cf-oagw-flow-gear-foundation-startup-registration`

**Constraints**: `cpt-cf-oagw-constraint-no-api-segment`, `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: API: route roots `/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`, `/oagw/v1/proxy/...` / Entities: `OagwGear`

### Health Contribution

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-health-contribution`

The system **MUST** contribute gear readiness to the host health endpoint such that `GET /healthz` on :8086 reports healthy after the gear initializes and starts its data plane, and never reports healthy from a half-configured or failed state.

**Implements**: `cpt-cf-oagw-flow-gear-foundation-health-reporting`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`

**Touches**: API: `GET /healthz` / Entities: `OagwGear`

### Crate-Level Test Harness

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-foundation-test-harness`

The system **MUST** ship crate-level unit and integration tests covering registration, lifecycle, config parsing/defaults/validation, migration registration, and route mounting under the workspace lint denials, and MUST NOT author any code under `testing/e2e/gears/oagw/` (reserved for acceptance).

**Implements**: `cpt-cf-oagw-flow-gear-foundation-startup-registration`, `cpt-cf-oagw-algo-gear-foundation-config-load-validation`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`, `cpt-cf-oagw-constraint-toolchain`

**Touches**: Entities: `OagwGear`, `OagwConfig`

## 6. Acceptance Criteria

- [x] The DoD build succeeds: `cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"` exits 0 with no lint/warning-as-error failures.
- [x] The server starts on `config/e2e-local.yaml`, binds `:8086`, and `GET /healthz` reports healthy and stays healthy during a sustained smoke run.
- [x] The gear is registered behind the `oagw` feature: requests to `/oagw/v1/...` route roots are served by the gear, not the host's unhandled-path response.
- [x] `OagwConfig` applies the documented defaults (proxy_timeout_secs=2, allow_http_upstream=false, ssrf_policy.enabled=true, token_cache_ttl_secs=300, token_cache_capacity=10000) when the block is minimal or absent; additive keys are accepted and validated.
- [x] Invalid configuration (unknown key, out-of-range value, semantic violation) prevents startup with a clear error.
- [x] Routes are served gear-relative at `/oagw/v1/...` with no `/api` segment when the host prefix is empty.
- [x] Readiness is reported only after init completes and the data-plane service starts; a failed init never reports healthy.
- [x] Crate-level tests for registration/lifecycle/config/migrations/route-mounting pass on the configured toolchain, and `testing/e2e/gears/oagw/` receives no code from this change.
