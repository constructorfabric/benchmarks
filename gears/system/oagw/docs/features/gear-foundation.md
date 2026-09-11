# Feature: Gear Foundation


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Registration and Route Mounting](#gear-registration-and-route-mounting)
  - [Canonical Error Response and Error Source](#canonical-error-response-and-error-source)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Gear Config Load and Defaults](#gear-config-load-and-defaults)
  - [REST Route Mounting](#rest-route-mounting)
  - [Domain Error to Problem Details Mapping](#domain-error-to-problem-details-mapping)
  - [Error Source Response Header Layer](#error-source-response-header-layer)
- [4. States (CDSL)](#4-states-cdsl)
  - [Gear Runtime State Machine](#gear-runtime-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Gear Declaration and Link-Time Registration](#gear-declaration-and-link-time-registration)
  - [Gear Config Struct and Defaults](#gear-config-struct-and-defaults)
  - [DDD-Light Module Layout](#ddd-light-module-layout)
  - [REST Capability Wiring and Gear Mount Root](#rest-capability-wiring-and-gear-mount-root)
  - [Canonical Error Mapping](#canonical-error-mapping)
  - [Error Source Response Header Layer](#error-source-response-header-layer-1)
  - [Startup Observability and Rollback](#startup-observability-and-rollback)
  - [Test Layering and Compile-Time Gate](#test-layering-and-compile-time-gate)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-gear-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-gear-foundation`

<!--
=============================================================================
FEATURE SPECIFICATION
=============================================================================
PURPOSE: Define detailed implementation behavior — flows, algorithms, states,
and implementation requirements that bridge PRD and DESIGN to code.

SCOPE:
  ✓ Actor flows (user-facing interactions, step by step)
  ✓ Processes / Business Logic (incl. internal logic, validation, async jobs, etc)
  ✓ State machines (entity lifecycle)
  ✓ Implementation requirements (what to build)
  ✓ Acceptance criteria (how to verify)

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Architecture, components, APIs → DESIGN.md
  ✗ Why a specific approach was chosen → ADR/

CDSL PSEUDO-CODE:
  Optional. Use for complex flows or when precise behavior must be
  communicated. Skip for simple features to avoid overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This feature establishes the `oagw` gear crate skeleton for the outbound API gateway — the toolkit
gear declaration and its link-time registration, the `OagwConfig` struct bound to the runtime
configuration keys, the DDD-Light module layout, and the REST capability wiring that makes the host
api-gateway mount the gear's router under the gear mount root `/oagw/v1/...` — and delivers the
cross-cutting response contract every later feature depends on: canonical RFC 9457
`application/problem+json` error plumbing with GTS `type` identifiers, and the
`X-OAGW-Error-Source: gateway|upstream` response header on every gear response. The crate
`cf-gears-oagw` (library name `oagw`) currently ships an empty `src/lib.rs`; this feature defines
what that entry point must contain before any business feature can register a handler.

### 1.2 Purpose

The gear skeleton comes first in the decomposition because entries 2.2 through 2.7 register their
handlers, read their configuration and return their errors through it: the control plane (2.2, 2.3)
mounts its CRUD endpoints on the router this feature wires, the data plane (2.4, 2.5, 2.6) reads the
timeouts, SSRF posture and token-cache bounds this feature loads, and all of them return the error
responses this feature maps and stamps. This feature owns the wiring half of
`cpt-cf-oagw-interface-management-api` — the mount root and the transport plumbing — while the
management endpoints themselves are delivered by entry 2.2, per the decomposition's
shared-requirement split. It realizes the error-source and problem-details principles
`cpt-cf-oagw-principle-error-source` and `cpt-cf-oagw-principle-rfc9457` as gear-owned transport
behaviour rather than as per-handler convention, and it follows the layering of
`cpt-cf-oagw-design-layers` and the gear structure of `cpt-cf-oagw-component-model`.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-interface-management-api`

**Principles**: `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`

**Feature-local deviations from platform baselines** (each inherited from the decomposition's
task-level assumptions; none is a new decision taken here):

- Gear-relative route paths — DECOMPOSITION assumption 1. The gear registers `/oagw/v1/...` without
  the `/api` prefix, because the host api-gateway nests every gear router under its own
  `prefix_path` and that prefix is empty in the graded configuration. The `/api/oagw/v1/...` paths in
  `cpt-cf-oagw-interface-api` are the absolute form behind an operator gateway and are not what this
  gear registers. Review owner: OAGW component maintainer (`cf-gears-oagw`).
- Plaintext upstream endpoints are admissible in this deployment — DECOMPOSITION assumption 2,
  recorded against `cpt-cf-oagw-constraint-https-only`. The endpoint scheme set admits `http`
  alongside `https|wss|wt|grpc` and the `allow_http_upstream` key governs whether a plaintext
  upstream connection is actually attempted; the default posture of the constraint stays HTTPS-only.
  Review owner: OAGW component maintainer, with the security reviewer as second approver.
  Validation: an in-crate test asserts the default posture fails closed — with the recorded default
  `allow_http_upstream: false` a plaintext upstream connection is refused, and only an explicit
  runtime-configuration opt-in admits one.
- `ssrf_policy.enabled` is part of the config surface — DECOMPOSITION assumption 7. The key is
  present in the runtime configuration but absent from the DESIGN's `OagwConfig` surface; setting it
  to `false` relaxes upstream host validation, while the default `true` keeps the DESIGN's SSRF
  posture unconditional. Review owner: OAGW component maintainer, with the security reviewer as
  second approver.
  Validation: an in-crate test asserts the default posture fails closed — with the recorded default
  `ssrf_policy.enabled: true` upstream host validation stays unconditional, and only an explicit
  `false` relaxes it.
- Gear dependency substitution — DECOMPOSITION assumption 8. The declared gear dependencies resolve
  to `types-registry`, `tenant-resolver`, `credstore` and `authz-resolver`; REST hosting is supplied
  by the host api-gateway rather than by the gear itself, the reverse-proxy engine arrives through
  the crate's existing `pingora-*` and `toolkit-http` dependencies rather than a direct `pingora`
  gear dependency, and no `toolkit-db` dependency is added (in-memory store, DECOMPOSITION
  assumption 3). Review owner: OAGW component maintainer.

**Cross-cutting concerns**:

- Security: the config surface carries no secret material — credentials stay behind `cred://`
  references per `cpt-cf-oagw-principle-cred-isolation` — and startup fails closed on an unusable
  configuration rather than running with a partially applied one.
- Reliability: the gear is stateless apart from the loaded configuration, so there is no recovery
  path to build here; a failed init aborts host startup and a restart with corrected configuration
  is the full remedy.
- Observability: structured startup logging (gear registered, config loaded, routes mounted) is in
  scope; metrics and audit records are not — OAGW registers metric families on the host-provided
  metrics surface (DECOMPOSITION assumption 9) and audit logging is entry 2.7.
- Rollback: no persistence and no data migration exist in this feature, so rollback is the
  operational act of redeploying the previous executable and restoring the previous
  `gears.oagw.config` keys; no in-gear rollback action is required.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer and integration tests under the crate's `tests/` directory; the `testing/e2e/gears/oagw/`
  directory is reserved for the component acceptance suite and is not used by this feature
  (DECOMPOSITION assumption 5).
- Compile-time gate: the gear exists in the host executable only when the host feature `oagw` is
  enabled, and gear registration is link-time (inventory), so an executable that does not link the
  crate registers no gear at all — the compile gate is the feature flag, and the FIPS build gate
  enables the gear together with FIPS-approved TLS cipher suites.
- Performance: not applicable in this feature — no request path is registered here, so there is no
  latency budget to state or measure; that work belongs to entries 2.4 (proxy request path) and 2.7
  (observability). The feature only records the `proxy_timeout_secs` knob that those entries read,
  with its recorded default of `cpt-cf-oagw-algo-config-load`.
- Compliance/Privacy: not applicable in this feature — no personal data is processed and no record is
  persisted (in-memory store, DECOMPOSITION assumption 3), and the config surface carries no secret
  material, so there is no data-retention, residency or subject-right surface to describe here.
- Accessibility: not applicable in this feature — no user-facing interface is authored here beyond
  the `application/problem+json` error body contract, whose machine-readable `type`, `title` and
  `detail` fields are the only surface an accessibility concern could attach to.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-platform-operator` | Deploys the single host executable with the `oagw` feature enabled, owns the `gears.oagw.config` keys this feature binds, and observes the startup outcome (gear registered, configuration loaded, routes mounted, readiness reported). |
| `cpt-cf-oagw-actor-app-developer` | Calls paths under the gear mount root `/oagw/v1/...` and consumes the canonical response contract: `application/problem+json` bodies with GTS `type` identifiers and `X-OAGW-Error-Source` on every response, including success responses. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.1 and assumptions 1 to 9
- **ADRs**: [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md) (`cpt-cf-oagw-adr-error-source-distinction`), [0008 OAuth2 Client Credentials Auth Plugin](../ADR/0008-oauth2-client-credentials-auth-plugin.md) (token-cache gear-level configuration)
- **Dependencies**: None — this feature has no feature-level dependencies; `cpt-cf-oagw-feature-upstream-route-management` and every later entry depend on it
- **Resolved gear dependencies**: `types-registry`, `tenant-resolver`, `credstore`, `authz-resolver` (declared on the gear, resolved by the toolkit host; assumption 8)
- **Platform baselines**: toolkit gear declaration convention (`name`, `capabilities`, `deps`, `lifecycle`), `RestApiCapability::register_rest` contract, `GearCtx::config_or_default` config binding keyed by gear name, and host router nesting under the api-gateway `prefix_path`; toolkit readiness surface (`libs/toolkit` `RestHealthcheckRegistry`, exposed to gears through `RestApiCapability::healthcheck`); host OpenAPI registry (`OpenApiRegistry` parameter of `register_rest`, populated by the api-gateway `openapi`/`enable_docs` configuration)

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the
end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to
actor actions.

**Use cases**: None in this feature — this feature delivers the mount point and the response contract
the PRD use cases run on; the use cases themselves are exercised by the entries that register the
endpoints they call.

**Referenced, not covered here**:

- `cpt-cf-oagw-usecase-proxy-request` — covered by DECOMPOSITION entry 2.4, which owns the proxy request path and the upstream passthrough this flow only declares.
- `cpt-cf-oagw-usecase-configure-upstream` — covered by DECOMPOSITION entry 2.2, which registers the management endpoint this use case calls.
- `cpt-cf-oagw-usecase-configure-route` — covered by DECOMPOSITION entry 2.2, which registers the management endpoint this use case calls.
- `cpt-cf-oagw-usecase-sse-streaming` — covered by DECOMPOSITION entry 2.6, which owns the streaming path this header layer already serves.
- `cpt-cf-oagw-usecase-rate-limit-exceeded` — covered by DECOMPOSITION entry 2.5, which owns the rate-limit policy and its canonical error.

### Gear Registration and Route Mounting

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-gear-registration`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Success Scenarios**:
- The operator starts the single host executable with the `oagw` feature enabled; the toolkit
  discovers the gear through link-time inventory registration, loads its configuration, and mounts
  its router so that paths under `/oagw/v1/...` are served gear-relative with no `/api` prefix.
- The gear reports readiness through the host readiness surface once the router is mounted, by
  returning a check from `RestApiCapability::healthcheck` into the toolkit's `RestHealthcheckRegistry`
  (platform baseline, section 1.4).
- The gear's operations and schemas appear in the host's OpenAPI document, registered through the
  `OpenApiRegistry` handed to `register_rest` and emitted under the api-gateway `openapi`/`enable_docs`
  configuration (platform baseline, section 1.4).

**Error Scenarios**:
- The `gears.oagw.config` section is absent, or a present key fails to deserialize or fails a
  recorded bound check: the gear `init` fails and host startup aborts instead of serving with a
  partially applied configuration.
- Route registration fails: the REST capability returns an error and host startup aborts with the
  gear named in the startup failure, so the operator can see which gear blocked the deployment.

**Steps**:
1. [x] - `p1` - Actor starts the single host executable with the `oagw` feature enabled - `inst-gf-reg-01`
2. [x] - `p1` - Toolkit discovers the gear declaration through link-time inventory registration (the host gear-linking module imports the crate for its registration side effect) - `inst-gf-reg-02`
3. [x] - `p1` - Gear `init` loads the `gears.oagw.config` section through the gear config provider, keyed by the gear name `oagw` - `inst-gf-reg-03`
4. [x] - `p1` - **IF** the config section is missing or fails to deserialize, or a value violates a recorded bound - `inst-gf-reg-04`
   1. [x] - `p1` - Apply the recorded defaults for absent optional keys; when a present key cannot be deserialized or validated, fail the gear `init` with an error naming the offending key and abort host startup - `inst-gf-reg-05`
5. [x] - `p1` - **ELSE** - `inst-gf-reg-06`
   1. [x] - `p1` - Store the loaded configuration on the gear struct for the lifetime of the process and log the effective key set without logging any secret material - `inst-gf-reg-07`
6. [x] - `p1` - API: the REST capability mounts the gear router on the host router so the host serves `/oagw/v1/...` gear-relative paths under the api-gateway `prefix_path` (empty in the graded configuration, so no `/api` prefix appears) - `inst-gf-reg-08`
7. [x] - `p1` - Register the gear's operation and schema entries in the host OpenAPI registry during the same mount step - `inst-gf-reg-09`
8. [x] - `p1` - Report gear readiness through the host readiness surface once the mount step returns - `inst-gf-reg-10`
9. [x] - `p1` - **RETURN** the merged router carrying the gear mount root `/oagw/v1` - `inst-gf-reg-11`

### Canonical Error Response and Error Source

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-canonical-error-response`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request to a gear path that fails inside gear code returns an RFC 9457
  `application/problem+json` body carrying the GTS `type` identifier, the HTTP status, the standard
  problem fields and the OAGW extension fields from `cpt-cf-oagw-interface-api`, with
  `X-OAGW-Error-Source: gateway`.
- A response produced by the upstream is passed through with its body unchanged and carries
  `X-OAGW-Error-Source: upstream`, so the caller can tell the two origins apart without parsing the
  body.

**Error Scenarios**:
- A gear response leaves the transport without the `X-OAGW-Error-Source` header: the response
  contract is broken and the acceptance criterion for the header layer fails.
- A gateway-originated failure is serialized with a content type other than
  `application/problem+json`, or with a `type` value outside the GTS identifier table fixed by
  `cpt-cf-oagw-interface-api`: the mapping layer is incomplete and the acceptance criterion fails.

**Steps**:
1. [x] - `p1` - Actor sends a request to a path under the gear mount root `/oagw/v1/...` - `inst-gf-err-01`
2. [x] - `p1` - Gear code produces an outcome: a domain error, or a response received from the upstream - `inst-gf-err-02`
3. [x] - `p1` - **IF** the failure originated inside the gear - `inst-gf-err-03`
   1. [x] - `p1` - Map the domain error to a canonical problem+json response using the GTS `type` identifier and HTTP status fixed by the error table in `cpt-cf-oagw-interface-api` - `inst-gf-err-04`
   1. [x] - `p1` - Classify the response error source as `gateway` - `inst-gf-err-05`
4. [x] - `p1` - **ELSE** (the response originates at the upstream; behaviour is exercised by entry 2.4) - `inst-gf-err-06`
   1. [x] - `p1` - Pass the upstream body through unchanged, classify the response error source as `upstream`, and add no gear-generated body - `inst-gf-err-07`
5. [x] - `p1` - Apply the error-source header layer to the outgoing response without overwriting a value already set by the producing path - `inst-gf-err-08`
6. [x] - `p1` - **RETURN** the response, carrying `X-OAGW-Error-Source` on every gear response whether it is a success, a gateway error or an upstream passthrough - `inst-gf-err-09`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are the
reusable building blocks called by the actor flows above and by the handler features that follow.

### Gear Config Load and Defaults

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-config-load`

**Input**: the `gears.oagw.config` section from the runtime configuration, as offered by the gear
context config provider. Keys: `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`,
`token_cache_ttl_secs`, `token_cache_capacity`.

**Output**: the typed `OagwConfig` value stored on the gear struct, with defaults applied for absent
optional keys.

**Recorded defaults** (the decomposition fixes the key set from the runtime configuration and the
token-cache keys from ADR 0008; the defaults themselves are fixed here):

| Key | Recorded default | Source |
|-----|------------------|--------|
| `proxy_timeout_secs` | `30` | Matches the toolkit HTTP client's own per-request timeout default of 30 seconds, so an unset key behaves like an unset upstream client timeout |
| `allow_http_upstream` | `false` | `cpt-cf-oagw-constraint-https-only` default posture (HTTPS-only); DECOMPOSITION assumption 2 keeps the default HTTPS-only and lets the runtime configuration opt in |
| `ssrf_policy.enabled` | `true` | DECOMPOSITION assumption 7: the default keeps the DESIGN's SSRF posture unconditional |
| `token_cache_ttl_secs` | `300` | ADR 0008 gear-level configuration table |
| `token_cache_capacity` | `10000` | ADR 0008 gear-level configuration table |

**Steps**:
1. [x] - `p1` - Read the gear config section keyed by the gear name `oagw` from the config provider; when the whole section is absent, start from the recorded default configuration - `inst-gf-cfg-01`
2. [x] - `p1` - **FOR EACH** key in the section - `inst-gf-cfg-02`
   1. [x] - `p1` - Deserialize the key into its typed field, falling back to the recorded default when the key is absent - `inst-gf-cfg-03`
3. [x] - `p1` - **TRY** - `inst-gf-cfg-04`
   1. [x] - `p1` - Complete the deserialization of the whole section into `OagwConfig` - `inst-gf-cfg-05`
4. [x] - `p1` - **CATCH** a config deserialization error - `inst-gf-cfg-06`
   1. [x] - `p1` - Fail gear `init` with an error naming the offending key and the expected shape, so host startup aborts instead of continuing with a partially applied configuration - `inst-gf-cfg-07`
5. [x] - `p1` - Validate the loaded values: the proxy timeout, the token-cache TTL and the token-cache capacity are strictly positive - `inst-gf-cfg-08`
6. [x] - `p1` - Store the validated configuration on the gear struct so every later feature reads the same effective values - `inst-gf-cfg-09`
7. [x] - `p1` - **RETURN** the stored configuration - `inst-gf-cfg-10`

### REST Route Mounting

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-rest-mount`

**Input**: the gear context, the host router as handed to the REST capability, and the host OpenAPI
registry.

**Output**: the merged router with the gear's routes attached under the mount root `/oagw/v1`, plus
the gear's OpenAPI operation and schema entries.

**Steps**:
1. [x] - `p1` - Obtain the initialized gear state (the stored configuration) and fail the mount step when `init` has not completed - `inst-gf-mnt-01`
2. [x] - `p1` - Build the gear router with the mount root `/oagw/v1`; in this feature no business route is attached, so the mounted subtree is the empty contract later features extend - `inst-gf-mnt-02`
3. [x] - `p1` - Register the gear's paths and schemas in the host OpenAPI registry (`OpenApiRegistry` parameter of `register_rest`, platform baseline in section 1.4) so the host emits a single document including the gear's operations - `inst-gf-mnt-03`
4. [x] - `p1` - Merge the gear router into the received router and hand the result back to the host - `inst-gf-mnt-04`
5. [x] - `p1` - **RETURN** the merged router; the host then applies its own `prefix_path` nesting, which is empty in the graded configuration - `inst-gf-mnt-05`

### Domain Error to Problem Details Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-mapping`

**Input**: a domain error raised by gear code, plus the request context available at the transport
layer (request path for `instance`, and `upstream_id`, `host`, `path` and `trace_id` when known).

**Output**: a canonical error response serialized as `application/problem+json` with a GTS `type`
identifier, the standard RFC 9457 fields and the OAGW extension fields.

**Steps**:
1. [x] - `p1` - Resolve the GTS `type` identifier and HTTP status for the domain error from the error table fixed by `cpt-cf-oagw-interface-api` - `inst-gf-map-01`
2. [x] - `p1` - Fill the standard problem fields: `type` with the GTS identifier, `title` with the human-readable summary, `status` with the HTTP status code, `detail` with the occurrence-specific explanation and `instance` with the request path - `inst-gf-map-02`
3. [x] - `p1` - **FOR EACH** extension value available in the request context - `inst-gf-map-03`
   1. [x] - `p1` - Attach the OAGW extension field (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`) and omit the ones that do not apply - `inst-gf-map-04`
4. [x] - `p1` - Serialize the problem document with the content type `application/problem+json` - `inst-gf-map-05`
5. [x] - `p1` - Never include credential material or resolved secret values in the document, per `cpt-cf-oagw-principle-cred-isolation` - `inst-gf-map-06`
6. [x] - `p1` - **RETURN** the canonical error response for the header layer to stamp - `inst-gf-map-07`

### Error Source Response Header Layer

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-source-header`

**Input**: an outgoing gear response and the error-source classification produced by the path that
created it.

**Output**: the same response carrying `X-OAGW-Error-Source: gateway|upstream`.

**Steps**:
1. [x] - `p1` - Inspect the outgoing response for an existing `X-OAGW-Error-Source` value - `inst-gf-hdr-01`
2. [x] - `p1` - **IF** the header is already present with a legal value - `inst-gf-hdr-02`
   1. [x] - `p1` - Leave the value untouched so the producing path keeps ownership of the classification - `inst-gf-hdr-03`
3. [x] - `p1` - **ELSE** - `inst-gf-hdr-04`
   1. [x] - `p1` - Set `gateway` when the response was generated by gear code, including every canonical error response - `inst-gf-hdr-05`
   1. [x] - `p1` - Set `upstream` when the response body and status were received from the upstream and passed through - `inst-gf-hdr-06`
4. [x] - `p1` - Apply the header to success responses as well as error responses, so a caller never has to infer the origin from the status code alone - `inst-gf-hdr-07`
5. [x] - `p1` - Apply the layer uniformly across response kinds, including streamed responses, so the contract holds for entry 2.6 without a second implementation - `inst-gf-hdr-08`
6. [x] - `p1` - **RETURN** the stamped response - `inst-gf-hdr-09`

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

The only lifecycle this feature owns is the gear's own runtime lifecycle; there is no resource state
machine, because this feature persists nothing (in-memory store, DECOMPOSITION assumption 3) and the
Upstream, Route and Plugin entities are declared here only — their lifecycles arrive with entries
2.2 and 2.3.

### Gear Runtime State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-gear-lifecycle`

**States**: `unregistered`, `registered`, `initialized`, `routes-mounted`, `ready`, `failed`

**Initial State**: `unregistered`

**Transitions**:
1. [x] - `p1` - **FROM** `unregistered` **TO** `registered` **WHEN** the host process links the crate and the toolkit discovers the gear declaration through link-time inventory registration - `inst-gf-st-01`
2. [x] - `p1` - **FROM** `registered` **TO** `initialized` **WHEN** gear `init` completes and the loaded configuration is stored on the gear struct - `inst-gf-st-02`
3. [x] - `p1` - **FROM** `initialized` **TO** `routes-mounted` **WHEN** the REST capability returns the merged router carrying the mount root `/oagw/v1` - `inst-gf-st-03`
4. [x] - `p1` - **FROM** `routes-mounted` **TO** `ready` **WHEN** the gear readiness check reports the mounted contract to the host readiness surface - `inst-gf-st-04`
5. [x] - `p1` - **FROM** `registered` **TO** `failed` **WHEN** the configuration cannot be loaded, deserialized or validated - `inst-gf-st-05`
6. [x] - `p1` - **FROM** `initialized` **TO** `failed` **WHEN** route registration or OpenAPI registration returns an error - `inst-gf-st-06`
7. [x] - `p1` - **FROM** `failed` **TO** `unregistered` **WHEN** the host process is stopped; recovery is a restart with corrected configuration, and no in-gear retry or rollback action exists because the gear holds no state - `inst-gf-st-07`
8. [x] - `p1` - **FROM** `ready` **TO** `unregistered` **WHEN** the host process is stopped - `inst-gf-st-08`
9. [x] - `p1` - **FROM** `routes-mounted` **TO** `unregistered` **WHEN** the host process is stopped - `inst-gf-st-09`

**Closed transition set**: the transitions above are the only ones possible. Any transition not
listed is impossible — no state is skipped, no state is re-entered on its own, and a state leaves the
lifecycle only by returning to `unregistered` when the host process is stopped.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Gear Declaration and Link-Time Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-gear-declaration`

The system **MUST** declare the gear in the `oagw` crate with the toolkit gear declaration using the
gear name `oagw`, the `rest` and `stateful` capabilities, and dependencies on the gears
`types-registry`, `tenant-resolver`, `credstore` and `authz-resolver` declared as resolvable, and the
crate **MUST** be reachable for link-time inventory registration from the host gear-linking module
(the server's `oagw` feature pulls in the crate, renamed from `cf-gears-oagw`, and imports it for its
registration side effect) so the declaration is discovered at startup.

**Implements**:
- `cpt-cf-oagw-flow-gear-registration`
- `cpt-cf-oagw-state-gear-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: gear mount root `/oagw/v1/...` (gear-relative; no business endpoints registered by this feature)
- DB: none — this feature persists nothing; the in-memory store is entry 2.2 (assumption 3)
- Entities: `OagwGear` (gear struct holding the loaded configuration)

### Gear Config Struct and Defaults

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-config-struct`

The system **MUST** define `OagwConfig` bound to the `gears.oagw.config` keys `proxy_timeout_secs`,
`allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs` and `token_cache_capacity`, with
the serde attributes and the recorded defaults of `cpt-cf-oagw-algo-config-load` (token-cache
defaults per ADR 0008), **MUST** load it through the gear context config provider so the section is
keyed by the gear name, **MUST** fail gear `init` when a present key cannot be deserialized or
violates a recorded validation bound, and **MUST** carry no secret material in the config surface.

**Implements**:
- `cpt-cf-oagw-algo-config-load`
- `cpt-cf-oagw-flow-gear-registration`
- `cpt-cf-oagw-state-gear-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Config: `gears.oagw.config` keys `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs`, `token_cache_capacity`
- Entities: `OagwConfig`

### DDD-Light Module Layout

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-module-layout`

The system **MUST** lay the crate out in the DDD-Light layers of `cpt-cf-oagw-design-layers` —
`api/rest` for transport (handlers, routes, DTOs, error response mapping, extractors), `domain` for
services, models, repository contracts and domain errors, and `infra` for the proxy engine, storage,
plugin registries and type provisioning — and **MUST** keep the dependency direction one-way:
the domain layer has no infrastructure dependency, infrastructure implements domain contracts, and
the transport layer maps between HTTP and domain types. The layout **MUST** be created as empty
module structure in this feature; only the error mapping and response-header pieces are populated
here.

**Implements**:
- `cpt-cf-oagw-algo-rest-mount`
- `cpt-cf-oagw-algo-error-mapping`
- `cpt-cf-oagw-state-gear-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- Entities: `Upstream`, `Route`, `Plugin` (declaration only — their models are entries 2.2 and 2.3)

### REST Capability Wiring and Gear Mount Root

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-rest-wiring`

The system **MUST** implement the toolkit REST capability for the gear so that `register_rest`
returns the merged router with the gear's routes attached under the mount root `/oagw/v1`, **MUST**
register the gear's operations and schemas in the host OpenAPI registry in the same step — the
`OpenApiRegistry` parameter of `register_rest`, emitted under the api-gateway `openapi`/`enable_docs`
configuration (platform baseline in section 1.4) — and
**MUST NOT** register any business endpoint in this feature and **MUST NOT** register paths under an
`/api` prefix — the gear-relative paths are what the host serves, because the api-gateway applies its
own `prefix_path`, which is empty in the graded configuration (assumption 1).

**Implements**:
- `cpt-cf-oagw-flow-gear-registration`
- `cpt-cf-oagw-algo-rest-mount`
- `cpt-cf-oagw-state-gear-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: gear mount root `/oagw/v1/...` (gear-relative)
- Entities: `OagwGear`

### Canonical Error Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-domain-error-mapping`

The system **MUST** provide a domain-error type and a single mapping layer that converts every domain
error into a canonical problem+json response with the GTS `type` identifier, HTTP status, standard
problem fields and OAGW extension fields fixed by `cpt-cf-oagw-interface-api`, and **MUST NOT** emit
gateway errors in any other body format. The mapping layer is the only place where a domain error
becomes an HTTP body, so entries 2.2 to 2.6 return errors through it instead of re-implementing the
table.

**Implements**:
- `cpt-cf-oagw-flow-canonical-error-response`
- `cpt-cf-oagw-algo-error-mapping`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Principles**: `cpt-cf-oagw-principle-rfc9457`

**Touches**:
- API: every gear response body for a gear-originated failure (`application/problem+json`)
- Entities: domain error type exposed to the later features

### Error Source Response Header Layer

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-source-header`

The system **MUST** apply the `X-OAGW-Error-Source` response header to every gear response — gateway
and upstream, success and error, including streamed responses — setting `gateway` for responses
generated by gear code and `upstream` for responses passed through from the upstream, and **MUST
NOT** overwrite a value already set by the producing path. This is the cross-cutting layer ADR 0007
specifies; entries 2.4 and 2.6 only classify, they do not re-implement the header.

**Implements**:
- `cpt-cf-oagw-flow-canonical-error-response`
- `cpt-cf-oagw-algo-error-source-header`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Principles**: `cpt-cf-oagw-principle-error-source`

**Touches**:
- API: every gear response under the mount root `/oagw/v1/...`

### Startup Observability and Rollback

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-startup-observability`

The system **MUST** emit structured startup log lines naming the gear when the declaration is
discovered, when the configuration is loaded (key set and effective values, never secret material)
and when the routes are mounted or the mount fails, and **MUST** report gear readiness through the
host readiness surface — the toolkit `RestHealthcheckRegistry` reached by returning a check from
`RestApiCapability::healthcheck` (platform baseline in section 1.4). The system **MUST NOT** expose a metrics endpoint of its own — OAGW
registers metric families on the host-provided metrics surface (assumption 9) — and **MUST NOT**
write audit records here (entry 2.7). Rollback is operational only: no persistence exists, so
recovery is redeploying the previous executable and restoring the previous configuration keys.

**Implements**:
- `cpt-cf-oagw-flow-gear-registration`
- `cpt-cf-oagw-state-gear-lifecycle`

**Constraints**: None

**Touches**:
- Entities: `OagwGear`

### Test Layering and Compile-Time Gate

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-test-layering`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside
`#[cfg(test)]` modules per layer for config load, defaults, error mapping and the header layer, and
integration tests under the crate's `tests/` directory that boot the gear router and assert the
gear-relative mount root and the response contract — and **MUST NOT** add an e2e suite under
`testing/e2e/gears/oagw/` (assumption 5). The compile-time gate **MUST** keep the gear out of the
host executable unless the host's `oagw` feature is enabled, **MUST** keep the FIPS build gate
enabling the gear together with FIPS-approved TLS cipher suites, and the tests **MUST** fail when
the gear declaration is not reachable for link-time registration, because an unlinked crate
registers no gear and fails silently otherwise.

**Implements**:
- `cpt-cf-oagw-flow-gear-registration`
- `cpt-cf-oagw-flow-canonical-error-response`
- `cpt-cf-oagw-algo-config-load`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:
- API: gear mount root `/oagw/v1/...` (asserted by the integration tests)
- Entities: `OagwConfig`

## 6. Acceptance Criteria

- [x] Starting the host executable with the `oagw` feature enabled registers the gear and serves paths under `/oagw/v1/...`; no registered path carries an `/api` prefix.
- [x] Starting the host executable without the `oagw` feature produces a build without the gear, and no `oagw` gear appears in the host gear list or OpenAPI document.
- [x] A `gears.oagw.config` section that is entirely absent yields the recorded defaults; a section with an unparseable key or an out-of-range value aborts startup with an error naming that key.
- [x] The loaded `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy.enabled`, `token_cache_ttl_secs` and `token_cache_capacity` values are observable by later features through the gear's stored configuration.
- [x] Every gear-generated response — a success response produced by gear code and every canonical `application/problem+json` error the gear maps — carries `X-OAGW-Error-Source` with the value `gateway`. The header layer also sets `upstream` for a passthrough response, but no upstream request path exists in this feature, so the upstream-passthrough leg of this criterion is not exercised here: it is verified by DECOMPOSITION entry 2.4, which owns the proxy path.
- [x] Every gear-originated failure returns `application/problem+json` with the GTS `type` identifier, `title`, `status`, `detail` and `instance` from the error table in `cpt-cf-oagw-interface-api`, and never a non-problem body.
- [x] Problem documents carry the OAGW extension fields (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`) when the request context provides them and omit the others.
- [x] No problem document and no startup log line contains credential material or a resolved secret value.
- [x] The crate layout keeps `domain` free of `infra` dependencies, with `api/rest` as the only layer that touches HTTP types.
- [x] The gear readiness check reports ready only after the REST mount step has returned the merged router.
- [x] The crate compiles and the in-crate unit and integration tests pass with the host feature set used by the graded configuration; no test artifact is added under `testing/e2e/gears/oagw/`.
