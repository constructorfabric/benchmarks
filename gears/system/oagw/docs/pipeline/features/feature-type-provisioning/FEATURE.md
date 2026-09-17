# Feature: OAGW GTS Type Provisioning


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Register GTS Catalog](#register-gts-catalog)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Catalog Materialization](#catalog-materialization)
- [4. States (CDSL)](#4-states-cdsl)
  - [GTS Catalog Registration State Machine](#gts-catalog-registration-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [GTS Catalog Registration](#gts-catalog-registration)
  - [Idempotent Registration](#idempotent-registration)
  - [Bounded Failure](#bounded-failure)
  - [Validation Helpers](#validation-helpers)
  - [Type-Provisioning Test Harness](#type-provisioning-test-harness)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [x] `p3` - **ID**: `cpt-cf-oagw-featstatus-type-provisioning`

- [x] `p3` - `cpt-cf-oagw-feature-type-provisioning`
## 1. Feature Context

### 1.1 Overview

Type provisioning registers the gear's GTS catalog with the platform types-registry at startup: the seven type ids under `gts.cf.core.oagw.*.v1` (upstream, route, auth_plugin, guard_plugin, transform_plugin, proxy, protocol) and the plugin inventory instances registered through the `gts_type_schema!`/`gts_instance!` registry calls, all from `infra/type_provisioning.rs` invoked inside `Gear::init` (the invocation point owned by feature-gear-foundation). Registration is idempotent, fail-loud but bounded (a registration failure surfaces at startup without bringing down unrelated gears), and gives the ecosystem a shared, discoverable vocabulary for OAGW upstream/route schemas, plugin types, the proxy protocol, and the protocol viability catalog.

### 1.2 Purpose

The authoritative contract `cpt-cf-oagw-contract-gts-registry` requires the gear to publish its types so that policy authoring, error typing, and cross-gear reference (e.g. the authz-resolver's `gts.cf.core.oagw.proxy.v1~:invoke` operation referenced by feature-data-plane, and the GTS error types in the proxy error catalog) resolve against the platform registry rather than bespoke strings. Without this feature the gear operates but its types exist nowhere authoritative: error problems cannot be typed, plugin catalogs cannot be discovered, and the design's registry-only mode has no foundation. The feature is deliberately narrow (registration and its idempotency/bounded-failure guarantees); it follows rather than owns the contract and reuses the types-registry SDK wired by feature-gear-foundation.

**Requirements**: `cpt-cf-oagw-contract-gts-registry`, `cpt-cf-oagw-usecase-startup-registration` (shared with feature-gear-foundation; the registration content is owned here, the invocation point there)

**Principles**: None — this feature follows the platform registration contract and the types-registry SDK rather than owning a new architectural decision.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-gts-registry` | The authoritative types-registry: the target that receives and answers registration of the seven type ids and the plugin inventory instances; failures surface at startup |
| `cpt-cf-oagw-actor-host-runtime` | Drives `Gear::init` (feature-gear-foundation) whose lifecycle calls this feature's catalog registration as one step, and observes bounded failures |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [0010-oagw-gear-bootstrap-and-composition.md](../ADR/0010-oagw-gear-bootstrap-and-composition.md) (`cpt-cf-oagw-adr-bootstrap-and-composition`, section on GTS catalog registration in `Gear::init`)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: `cpt-cf-oagw-feature-gear-foundation` (lifecycle invocation point, config, SDK wiring)
- **Interfaces**: `cpt-cf-oagw-interface-config-model`, `cpt-cf-oagw-interface-gear-registration` (registration surface wiring supplied by feature-gear-foundation)
- **Components**: `cpt-cf-oagw-component-type-provisioning`
- **Tech**: `cpt-cf-oagw-tech-oagw-stack`
- **Sequences**: `cpt-cf-oagw-seq-startup-registration` (shared; registration content owned here)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-startup-registration` (shared with feature-gear-foundation)

### Register GTS Catalog

- [x] `p3` - **ID**: `cpt-cf-oagw-flow-type-provisioning-register-gts-catalog`

**Actor**: `cpt-cf-oagw-actor-gts-registry`

**Success Scenarios**:
- At startup the gear registers the seven type ids under `gts.cf.core.oagw.*.v1` and the plugin inventory instances through the registry calls; registration is idempotent (re-runs converge, no duplicates), and the catalog is resolvable by identity-hungry consumers (policy authoring, error typing, plugin discovery).

**Error Scenarios**:
- A registration call fails (registry unavailable, conflict that idempotency cannot absorb): the gear surfaces the failure at startup loud enough to be diagnosed, but bounded so unrelated gears and the host continue.

**Steps**:
1. [x] - `p1` - `Gear::init` reaches the type-provisioning step (invocation point owned by feature-gear-foundation) - `inst-init-invokes`
2. [x] - `p1` - Resolve the types-registry `-sdk` client through the already-wired `ctx.client_hub()` - `inst-sdk-client`
3. [x] - `p1` - Define the seven type ids under `gts.cf.core.oagw.*.v1` (upstream, route, auth_plugin, guard_plugin, transform_plugin, proxy, protocol) with their registries' catalog context - `inst-define-types`
4. [x] - `p1` - Register the type ids through the registry calls (`gts_type_schema!` style) - `inst-register-types`
5. [x] - `p1` - Register the plugin inventory instances through the instance catalog (`gts_instance!` style) so plugin types are discoverable by the plugin registry (feature-data-plane consumes them) - `inst-register-instances`
6. [x] - `p1` - **IF** a registration returns a conflict that idempotency cannot absorb or the registry is unreachable - `inst-registration-fail`
   1. [x] - `p1` - **TRY** produce a precise, actionable startup error (which type id, which failure) - `inst-error-out`
   2. [x] - `p1` - **CATCH** error-authoring failure - `inst-error-fallback-out`
   3. [x] - `p1` - **RETURN** the bounded failure so only this gear's registration surfaces as failed (fail loud, do not cascade) - `inst-bounded-return`
7. [x] - `p1` - **ELSE** registration succeeds - `inst-registration-ok`
   1. [x] - `p1` - **RETURN** success so init can continue to migrations/`register_rest`/start - `inst-init-continue`

## 3. Processes / Business Logic (CDSL)

### Catalog Materialization

- [x] `p3` - **ID**: `cpt-cf-oagw-algo-type-provisioning-catalog-materialization`

**Input**: resolved types-registry SDK client; the gear's fixed catalog (seven type ids, plugin inventory instances)

**Output**: the catalog fully registered (idempotently) with success, or a precise bounded failure surfaced at startup

**Steps**:
1. [x] - `p1` - Compose the fixed catalog: type ids `gts.cf.core.oagw.upstream.v1`, `gts.cf.core.oagw.route.v1`, `gts.cf.core.oagw.auth_plugin.v1`, `gts.cf.core.oagw.guard_plugin.v1`, `gts.cf.core.oagw.transform_plugin.v1`, `gts.cf.core.oagw.proxy.v1`, `gts.cf.core.oagw.protocol.v1`, plus the plugin inventory instances - `inst-compose-catalog`
2. [x] - `p1` - **FOR EACH** type id in the catalog - `inst-type-loop`
   1. [x] - `p1` - Register the type id (schema-bearing) with the registry SDK - `inst-register-one`
   2. [x] - `p1` - **IF** the registry reports the type already present under this identity - `inst-already-present`
      1. [x] - `p1` - Treat as convergent (idempotent re-run) and continue - `inst-converge`
   3. [x] - `p1` - **ELSE IF** registration fails for a non-identity reason - `inst-hard-fail`
      1. [x] - `p1` - **RETURN** the precise bounded failure (fail loud for this gear only) - `inst-bounded-fail`
3. [x] - `p1` - **FOR EACH** plugin inventory instance - `inst-instance-loop`
   1. [x] - `p1` - Register the instance against its catalog type - `inst-instance-register`
   2. [x] - `p1` - **IF** the instance already exists - `inst-instance-exists`
      1. [x] - `p1` - Accept the convergent state (idempotency) - `inst-instance-converge`
4. [x] - `p1` - Weave the validation helpers (schema-backed reference checks for identity, type, and required fields) - `inst-validation-helpers`
5. [x] - `p1` - **RETURN** the materialized, registered, idempotent catalog (or the bounded failure) - `inst-return-materialized`

## 4. States (CDSL)

### GTS Catalog Registration State Machine

- [x] `p3` - **ID**: `cpt-cf-oagw-state-type-provisioning-registration`

**States**: `Pending`, `Registered`, `Failed`

**Initial State**: `Pending`

**Transitions**:
1. [x] - `p1` - **FROM** Pending **TO** Registered **WHEN** every type id and plugin inventory instance registers with the types-registry SDK (idempotent convergence accepts already-present identities) - `inst-registered`
2. [x] - `p1` - **FROM** Pending **TO** Failed **WHEN** a registration call returns a conflict that idempotency cannot absorb or the registry is unreachable - `inst-failed`
3. [x] - `p1` - **FROM** Failed **TO** Registered **WHEN** startup retries and registration converges (bounded failure is retryable across process restarts) - `inst-retry-registered`

**Note**: there is no run-time stateful lifecycle beyond this startup-time registration step and no externally observable run-time transitions; the machine exists solely to make the bounded-failure contract (`cpt-cf-oagw-dod-type-provisioning-bounded-failure`) explicit.

## 5. Definitions of Done

### GTS Catalog Registration

- [x] `p3` - **ID**: `cpt-cf-oagw-dod-type-provisioning-gts-catalog`

The system **MUST** register the seven type ids under `gts.cf.core.oagw.*.v1` (upstream, route, auth_plugin, guard_plugin, transform_plugin, proxy, protocol) and the plugin inventory instances through the types-registry SDK from `infra/type_provisioning.rs` during `Gear::init`, making the catalog resolvable to identity-hungry consumers per `cpt-cf-oagw-contract-gts-registry`.

**Implements**: `cpt-cf-oagw-flow-type-provisioning-register-gts-catalog`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`, `cpt-cf-oagw-constraint-workspace-lints`

**Touches**: DB: none (registry side effect) / Entities: `GtsTypeId`, `GtsInstance`

### Idempotent Registration

- [x] `p3` - **ID**: `cpt-cf-oagw-dod-type-provisioning-idempotency`

The system **MUST** make registration idempotent: re-running startup (repeated deployments, retries after bounded failure) converges to one authoritative catalog with no duplicate type ids and no duplicate plugin instances, under the constraint that first-registration ordering does not matter.

**Implements**: `cpt-cf-oagw-algo-type-provisioning-catalog-materialization`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: DB: none / Entities: `GtsTypeId`, `GtsInstance`

### Bounded Failure

- [x] `p3` - **ID**: `cpt-cf-oagw-dod-type-provisioning-bounded-failure`

The system **MUST** surface a registration failure at startup loud enough to be diagnosed (precise, actionable error naming the failing type id/instance and the reason), and bounded so that a failure in this feature does not cascade into unrelated gears or the host.

**Implements**: `cpt-cf-oagw-flow-type-provisioning-register-gts-catalog`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`, `cpt-cf-oagw-constraint-toolchain`

**Touches**: Entities: `GtsRegistrationError`

### Validation Helpers

- [x] `p3` - **ID**: `cpt-cf-oagw-dod-type-provisioning-validation-helpers`

The system **MUST** provide schema-backed validation helpers for the registered catalog (reference checks for identity, type, and required fields against the accepted shapes) so downstream consumers and the control/data-plane features can rely on a consistent, discoverable type vocabulary.

**Implements**: `cpt-cf-oagw-algo-type-provisioning-catalog-materialization`

**Constraints**: `cpt-cf-oagw-constraint-authoritative-immutable`

**Touches**: Entities: `CatalogValidationHelper`

### Type-Provisioning Test Harness

- [x] `p3` - **ID**: `cpt-cf-oagw-dod-type-provisioning-test-harness`

The system **MUST** test catalog composition, idempotent re-registration, bounded failure on registry unavailability, and the validation helpers at the crate level under the workspace lint denials, and MUST NOT author any code under `testing/e2e/gears/oagw/`.

**Implements**: `cpt-cf-oagw-algo-type-provisioning-catalog-materialization`

**Constraints**: `cpt-cf-oagw-constraint-workspace-lints`, `cpt-cf-oagw-constraint-toolchain`

**Touches**: Entities: `GtsTypeId`, `GtsInstance`, `CatalogValidationHelper`

## 6. Acceptance Criteria

- [x] The seven type ids under `gts.cf.core.oagw.*.v1` are registered with the registry during startup and resolvable exactly once.
- [x] The plugin inventory instances are registered and discoverable against their catalog types.
- [x] Rerunning startup converges: repeated registration produces no duplicate type ids and no duplicate instances.
- [x] A registry-unavailable or unresolvable-conflict scenario surfaces a precise, actionable startup error and does not cascade to unrelated gears or the host.
- [x] Validation helpers reject references with unknown identity, wrong type, or missing required fields.
- [x] Every crate-level type-provisioning test passes on the configured toolchain; `testing/e2e/gears/oagw/` receives no code from this change.
