# PRD — OAGW Gear Implementation (Change Request)

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Module-Specific Environment Constraints](#31-module-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Gear Registration & Lifecycle](#51-gear-registration--lifecycle)
  - [5.2 Control-Plane Management](#52-control-plane-management)
  - [5.3 Data-Plane Proxying](#53-data-plane-proxying)
  - [5.4 Plugin System](#54-plugin-system)
  - [5.5 CORS](#55-cors)
  - [5.6 Error Semantics](#56-error-semantics)
  - [5.7 Security](#57-security)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 NFR Inclusions](#61-nfr-inclusions)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Traceability](#13-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

This PRD captures the requirements for a single change to the `oagw` gear (package `cf-gears-oagw`, library name `oagw`, v0.4.0) in the Constructor Fabric gears-rust workspace: **implement the gear so it registers with the host runtime and serves the API its specification describes.** Today the workspace member at `gears/system/oagw/oagw/` has an intact manifest but an empty `src/lib.rs` — nothing registers the gear with the host binary, nothing installs its routes, and every request to its API surface is answered with the host's generic "not found" response. The server still builds and starts, so the failure mode is silent: the platform builds, boots, and reports healthy while the Outbound API Gateway is entirely absent from the running process.

The authoritative product contract — the component's own `docs/PRD.md`, `docs/DESIGN.md`, the nine ADRs in `docs/ADR/`, and the `docs/schemas/` JSON Schemas — already defines what the Outbound API Gateway must do (control-plane management of upstreams, routes, and plugins; data-plane proxying to external services; credential injection; rate limiting; CORS; error semantics with gateway-vs-upstream source distinction; security constraints). This PRD does **not** restate that contract. It constrains the change dimension only: which of those behaviors this implementation must deliver, in what order, how the gear integrates with the host runtime, and what evidence proves the change is done. Where a behavior is already specified authoritatively, this PRD references the authoritative document as the source of truth instead of duplicating it.

### 1.2 Background / Problem Statement

The `oagw` workspace member is declared and wired: `config/e2e-features.txt` includes the `oagw` feature, `apps/cf-gears-example-server/src/registered_gears.rs` links the crate behind `#[cfg(feature = "oagw")]`, the workspace `Cargo.toml` maps `oagw = { package = "cf-gears-oagw", version = "0.4.0", path = "gears/system/oagw/oagw" }`, and the e2e configuration (`config/e2e-local.yaml`) already carries an `oagw` gear block with `proxy_timeout_secs`, `allow_http_upstream`, and an `ssrf_policy`. The only missing piece is the crate's source of truth: `src/lib.rs` is 0 bytes. As a result the gateway advertised by the product contract does not exist inside the server; the e2e acceptance surface would answer "not found" for every management and proxy operation, and platform gears would have no centralized outbound path (violating the platform rule that all outbound traffic routes through a gateway).

The pain this change removes is direct: a building, healthy-looking server whose single most security-relevant middleware (the choke point for outbound credentials, rates, and SSRF policy) is not running. The change is needed now because the crate, feature wiring, configuration slots, and authoritative specification are all already present; only the gear implementation is missing, and it is the last step before the gear can be exercised and accepted by the reserved acceptance suite under `testing/e2e/gears/oagw/`.

The definition of done for this change, used throughout this PRD as the acceptance backbone:

- The server builds: `cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"`.
- The server starts on `config/e2e-local.yaml`, binds :8086, and reports healthy via `/healthz`.
- The `oagw` gear serves its specification's routes, request/response bodies, status codes, validation, proxying, and error semantics.
- The implementation is covered by tests, and nothing under `testing/e2e/gears/oagw/` is authored by this change (that directory is reserved for acceptance).

### 1.3 Goals (Business Outcomes)

Each goal is measurable and maps to the change's definition of done.

- **G1 — Gear is registered and routable**: the `oagw` crate becomes a live gear in the host server when built with the e2e feature set; the host starts on the e2e config, reports healthy, and requests to the gear's API surface are served by the gear rather than answered as unhandled. Measured by the DoD build and startup steps plus a successful request against each of the gear's API surfaces.
- **G2 — Specified gateway behavior is delivered**: the implemented gear provides the outbound gateway behaviors its authoritative contract defines — control-plane management of upstreams, routes, and plugins with tenant scoping and validation; data-plane proxying with alias resolution, route matching, configuration layering, credential injection, rate limiting, header handling, CORS, streaming, and error semantics with gateway-vs-upstream source distinction. Measured by automated tests and acceptance exercising the contract's behaviors.
- **G3 — Implementation is tested without encroaching on acceptance**: the change ships with automated tests (unit and integration) at crate level that verify the implemented behavior; the reserved acceptance path `testing/e2e/gears/oagw/` receives no code from the implementation. Measured by test presence, test pass rate, and a diff check that the acceptance directory is untouched.
- **G4 — Contract consistency**: the implemented behavior matches the authoritative product contract (docs/PRD.md, docs/DESIGN.md, docs/ADR/, docs/schemas/) — the implementation is verified against it and does not drift from, duplicate, or contradict it. Measured by traceability from implementation tests back to the authoritative requirement IDs.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Gear | A self-contained service crate in the gears-rust workspace, composed into one server binary by the host runtime. |
| Host runtime | The `cf-gears-example-server` binary and its ToolKit framework that discover, load, configure, and route for compiled gears. |
| Gear registration | The act by which a compiled gear makes itself known to the host runtime (lifecycle hooks, route registration, capability declarations) so its API surface is reachable. |
| API surface | The complete set of operations a gear exposes at runtime, composed of the management surface (configuration CRUD) and the data-plane surface (proxying), as specified by the authoritative contract. |
| Control Plane (CP) | The part of the gear that manages configuration data (upstreams, routes, plugins) with tenant scoping and validation. |
| Data Plane (DP) | The part of the gear that orchestrates proxy requests: resolves configuration, executes plugin chains, and forwards calls to external services. |
| Upstream | External service target defined by server endpoints (scheme/host/port), protocol, authentication, headers, and rate limits (authoritative PRD §1.4). |
| Route | API path on an upstream that matches inbound requests by method, path, and query rules, mapping them to upstream behavior (authoritative PRD §1.4). |
| Plugin | Modular processor attached to upstreams or routes; three types — Auth, Guard, Transform (authoritative PRD §1.4). |
| Alias | Short identifier used in proxy URLs to reference an upstream; auto-derived or explicit per endpoint type (authoritative PRD §1.4). |
| Sharing mode | Hierarchical tenancy visibility setting: `private`, `inherit`, or `enforce` (authoritative PRD §1.4). |
| Error-source distinction | The contract-guaranteed way a caller can tell whether a failure originated at the gateway or at the upstream (authoritative ADR 0007). |
| SSRF policy | The gear's security controls that prevent the proxy from being abused to reach internal or disallowed targets (DNS/IP validation, scheme allowlist, header stripping). |
| DoD | Definition of done for this change — the build/startup/serving/test evidence listed in §1.2. |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level. This section documents actors (people and systems) that interact with the gear under change.

### 2.1 Human Actors

#### Gateway Operator

**ID**: `cpt-cf-oagw-actor-gateway-operator`

**Role**: Operates the outbound gateway by managing upstreams, routes, and plugins through the management surface, and by reviewing proxy traffic, errors, and metrics. Subsumes the platform-operator and tenant-administrator concerns from the authoritative contract (who may manage what at which tenancy level).

**Needs**: Create, read, update, and delete upstream and route configurations; register and bind plugin definitions; set rate limits, CORS, headers, and authentication on upstreams and routes; enforce configuration on descendant tenants where permitted; observe traffic, errors, and health; have every management operation validated and tenant-scoped.

#### Proxy Client

**ID**: `cpt-cf-oagw-actor-proxy-client`

**Role**: Any application developer or consuming service that sends requests through the gear's data-plane proxy to reach an external service without managing credentials, connection details, or outbound security policy directly.

**Needs**: A stable, simple proxy surface that resolves upstreams by alias, matches routes, injects credentials transparently, enforces limits, and returns responses whose errors can be attributed to the gateway or the upstream.

### 2.2 System Actors

#### Host Runtime

**ID**: `cpt-cf-oagw-actor-host-runtime`

**Role**: The `cf-gears-example-server` binary and ToolKit framework that build in the gear behind the `oagw` feature, invoke its lifecycle, mount its routes, inject configuration, and report overall health (including the health endpoint on :8086). Demands that the gear complete startup within the host's health window and register its API surface so the process serves it.

#### Authz Resolver

**ID**: `cpt-cf-oagw-actor-authz-resolver`

**Role**: Platform gear (`authz-resolver`, with its policy enforcer SDK) that evaluates authorization for inbound requests and grants or denies the exact operations the gateway's permission model defines. The gateway depends on it to enforce per-actor/operation permissions on the management and proxy surfaces.

#### Types Registry

**ID**: `cpt-cf-oagw-actor-gts-registry`

**Role**: Platform gear (`types-registry`, GTS) that hosts the schema and instance catalog. The OAGW gear registers its GTS type catalog (upstream, route, plugin, protocol identifiers) with it and validates identifiers against it, per the authoritative contract.

#### Tenant Resolver

**ID**: `cpt-cf-oagw-actor-tenant-resolver`

**Role**: Platform gear (`tenant-resolver`) that resolves the tenant hierarchy and ancestry used by configuration sharing modes, alias shadowing, and enforced ancestor constraints. The gateway consumes the tenant chain when resolving effective configuration and when walking aliases.

#### Credential Store

**ID**: `cpt-cf-oagw-actor-credential-store`

**Role**: Platform gear (`credstore`) that stores and retrieves secret material by reference. The gateway never holds credentials directly; it resolves secret references through this actor at credential-injection time.

## 3. Operational Concept & Environment

> **Note**: Project-wide runtime, OS, architecture, lifecycle policy, and integration patterns are defined at project level. Only gear/change-specific constraints are listed here.

### 3.1 Module-Specific Environment Constraints

- **Single-executable composition**: the gear is compiled into the `cf-gears-example-server` binary as a workspace member behind the `oagw` cargo feature. There is no separate process and no alternate deployment mode for the e2e target; the gear must live and serve within the host process.
- **Feature-gated build surface**: the gear must compile with the exact feature set in `config/e2e-features.txt` (which includes `oagw` alongside `credstore`, `static-credstore`, `tenant-resolver-rg`, `static-authz`, `tr-authz`, and friends), on the workspace's resolver/edition/rust-version (edition 2024, resolver 3, rust-version 1.95.0), under the workspace's lint denials (clippy::pedantic, unwrap/expect).
- **Runtime configuration slot**: the e2e config already reserves a gear block with `proxy_timeout_secs`, `allow_http_upstream`, and a nested `ssrf_policy` (with `enabled`). The gear must read its configuration from this host-provided slot with safe defaults and tolerate the test configuration as-is; any additional configuration keys must be additive and backward-compatible.
- **Test-only security allowances**: the e2e config sets `allow_http_upstream: true` and `ssrf_policy.enabled: false` — explicit test allowances for local e2e only. The production security posture (HTTPS-only upstreams by default, SSRF protection on by default) remains the contract, and the test allowances must never weaken the default-behavior requirement (see §5.7 `cpt-cf-oagw-fr-security-policy`).
- **Host-composed route mounting**: the host's API gateway composes gear-relative route prefixes. The authoritative docs spell the API surface in its api-gateway-nested form (`/api/oagw/v1/...`); at runtime with the e2e gateway's empty prefix the gear's routes are served gear-relative. The implementation must mount its routes in the host-composed form so that the authoritative contract's public paths are reachable end-to-end; the exact mount resolution is a design concern, not restated here.
- **Dependency availability in e2e**: all platform actors the gear depends on (authz-resolver, types-registry, tenant-resolver, credstore) are compiled and configured by the e2e feature set and config, so the gear can and must integrate with them in-process via their workspace SDKs rather than stubbing them.
- **Persistence capability**: per the authoritative design the gear persists configuration (upstreams, routes, plugins). Whether the e2e host provides a database capability slot for the gear (and which backend) is resolved during implementation; the change must not depend on persistence features unavailable in the e2e host (see §11 assumptions and §12 risks).

## 4. Scope

### 4.1 In Scope

- Gear registration with the host runtime and full lifecycle (startup, configuration load, any provisioning/registration steps, readiness contribution to host health), compiled behind the `oagw` feature.
- Serving the gear's complete API surface so that management and proxy requests are answered by the gear instead of the host's unhandled-path response.
- Control-plane management of upstreams, routes, and plugins: CRUD with validation, tenant scoping, alias enforcement, enable/disable semantics, and hierarchical configuration (sharing modes, enforced constraints), per the authoritative contract.
- Data-plane proxying: alias resolution with tenant-hierarchy shadowing, route matching, configuration layering (upstream < route < tenant), plugin-chain execution, outbound call with connection pooling/load balancing across endpoints, streaming (HTTP request/response, SSE, WebSocket, WebTransport per authority), and no automatic full-request retries (connector-level endpoint failover/connection retry permitted per authority).
- Credential injection via the built-in auth plugin set (noop, API key, OAuth2 Client Credentials Form and Basic variants) with credentials resolved by reference from the credential store.
- Rate limiting per the authoritative contract (token bucket, dual-rate, scope, strategy, cost, response headers, hierarchical stricter-wins semantics).
- Built-in plugin runtime: registration and execution of the built-in Auth/Guard/Transform plugins per the authoritative catalog (including required-headers guard and request-id transform), with deterministic execution order; plugin definition CRUD (create/read/delete, immutability, in-use protection).
- CORS handling as a built-in capability per the authoritative design (per-upstream/route configuration, preflight and actual-request enforcement, secure defaults).
- Configuration caching for hot lookups (in-memory, with explicit invalidation on writes) to meet the latency target, per the authoritative caching/state ADRs.
- Error semantics per the authoritative contract: gateway-generated errors versus upstream passthrough errors, with the error-source distinction mechanism (authoritative ADR 0007) on every response.
- Security constraints: HTTPS-only upstreams by default with an SSRF policy (scheme allowlist, header stripping/validation, request validation), secret resolution exclusively through the credential store, and zero credential material in logs/errors/responses.
- Inbound authorization enforced against the exact permission model of the authoritative contract (§5.6).
- Observability in scope of the contract: structured request logging with correlation IDs and Prometheus metrics per the authoritative metrics vocabulary, both without PII or secrets.
- Automated test coverage (unit and integration) at crate level for the implemented behavior; the reserved acceptance path `testing/e2e/gears/oagw/` stays untouched.

### 4.2 Out of Scope

- Custom Starlark plugin execution (sandboxed runtime) and plugin garbage collection: the crate's declared dependencies do not include a Starlark runtime, and no scriptable plugin execution is required to satisfy the DoD. Plugin definition storage/CRUD and the built-in plugin runtime are in scope; executing tenant-authored Starlark is deferred and tracked (see §11/§12).
- gRPC proxying (authoritative phase 3/4) — the authoritative design states no gRPC proxy code path is currently implemented or reachable; HTTP-family proxying (including streaming) is in scope.
- Distributed/Redis-backed rate limiting and the L2 (Redis) config-cache tier: per-instance in-memory enforcement is the authoritative MVP; distributed coordination is future work.
- Response caching and automatic request retries — explicitly the client's/upstream's responsibility per the authoritative contract; the gear performs no full client-request retries.
- DNS resolution and IP-pinning implementation details — authoritative out of scope (the SSRF policy consumes them, it does not re-implement them).
- TLS certificate pinning, mTLS, HTTP/3 (QUIC) — authoritative future work.
- Authoring the acceptance suite under `testing/e2e/gears/oagw/` — reserved for acceptance, intentionally out of scope for this change.
- Any change to the authoritative product contract (docs/PRD.md, docs/DESIGN.md, docs/ADR/, docs/schemas/) — this change implements it, and any contractual discrepancy found during implementation is reported rather than silently deviated from.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting the workspace's coverage expectations unless otherwise specified. The DoD additionally requires the implementation's own tests to live at crate level, not under `testing/e2e/gears/oagw/`. Verification method is documented per requirement only where the approach is non-standard.
>
> **Source-of-truth rule**: This section states WHAT the change must deliver. Where a behavior, schema, route, body, status code, or validation rule is already defined in the authoritative contract (`docs/PRD.md`, `docs/DESIGN.md`, `docs/ADR/`, `docs/schemas/`), the requirement references that source rather than restating it. No requirement here may contradict the authoritative contract.

### 5.1 Gear Registration & Lifecycle

#### Gear Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-gear-registration`

The gear **MUST** register itself with the host runtime so that, when the server is built with the `oagw` feature, the gear is discovered and loaded by the host's gear registry, its lifecycle is invoked, and its API surface becomes part of the running server. The crate **MUST** remain a valid library (lib name `oagw`) whose entry point the host can link through the workspace's gear-registration convention (as exercised in `apps/cf-gears-example-server/src/registered_gears.rs`).

**Rationale**: The empty `lib.rs` is the root cause of the change — nothing registers the gear today, so none of its behavior is reachable.

**Actors**: `cpt-cf-oagw-actor-host-runtime`

#### Gear Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-gear-lifecycle`

The gear **MUST** complete its full lifecycle within the host's startup sequence: load configuration from the host-provided slot, initialize its control-plane and data-plane services and their dependencies, perform any required registration/provisioning steps (including GTS catalog registration with the types registry per the authoritative contract), and reach a ready state. A gear that cannot initialize **MUST** fail startup loudly rather than booting into a half-configured state, and on successful startup the gear **MUST** contribute to host health such that the host's health endpoint (healthz on :8086) reports healthy.

**Rationale**: The DoD requires the server to start on the e2e config with a healthy health endpoint; lifecycle correctness is what makes that true without silently degrading.

**Actors**: `cpt-cf-oagw-actor-host-runtime`, `cpt-cf-oagw-actor-gts-registry`

**Verification Method**: automated integration test asserting startup completes and health is reported; demonstration against the e2e config.

#### Gear Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-gear-configuration`

The gear **MUST** read its runtime configuration from the host-injected gear block (currently carrying `proxy_timeout_secs`, `allow_http_upstream`, and `ssrf_policy`) and apply it to runtime behavior — proxy timeout, upstream-scheme allowance, and SSRF policy enablement — while every field **MUST** have a safe default so the gear works when the block is minimal or absent. Any configuration the authoritative contract defines beyond the present keys (e.g., token-cache settings per ADR 0008) **MUST** be additive and validated, and invalid configuration **MUST** be rejected at startup with a clear error rather than silently ignored.

**Rationale**: The e2e config already reserves these knobs (including test-only allowances); the gear must honor them, and operators must not be able to misconfigure it silently.

**Actors**: `cpt-cf-oagw-actor-host-runtime`, `cpt-cf-oagw-actor-gateway-operator`

#### Route Serving

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-route-serving`

The gear **MUST** serve its complete API surface at runtime per the authoritative contract (`docs/PRD.md` §7, `docs/DESIGN.md` §3.3): every management and proxy operation the contract defines is served by the gear with the authoritative request/response bodies, status codes, validation, and error semantics. No path owned by the gear's API surface **MAY** fall through to the host's unhandled-path response after the gear has started successfully.

**Rationale**: This is the change's central defect — every request to the gear today answers unhandled. Route serving flips that state and is the visible proof that registration worked; the DoD names route/body/status/validation/error behavior explicitly.

**Actors**: `cpt-cf-oagw-actor-host-runtime`, `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`

### 5.2 Control-Plane Management

#### Management Operations (Upstreams, Routes, Plugins)

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-control-plane-crud`

The gear **MUST** implement the management surface defined by the authoritative contract (`docs/PRD.md` §5.1 and §5.3, `docs/DESIGN.md` §3.3): create, read, update, and delete operations for upstreams and routes, and create/read/delete (immutable-update) operations for plugin definitions, with the authoritative validation rules, alias enforcement rules, enable/disable semantics (including ancestor-disable propagating to descendants), tenant scoping (ancestor resources are not addressable through management), and per-resource uniqueness constraints. All management operations **MUST** be tenant-scoped and validated before any state change, and the persistence model **MUST** satisfy the authoritative invariants (see `docs/DESIGN.md` §3.6).

**Rationale**: Control-plane management is the configuration backbone of the gateway; without it operators cannot express any upstream, route, or plugin, and the data plane has nothing to serve.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`

#### Hierarchical Configuration

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-config-hierarchy`

The gear **MUST** implement hierarchical configuration across the tenant chain per the authoritative contract (`docs/PRD.md` §5.5, `docs/DESIGN.md` §3.2): sharing modes (`private`/`inherit`/`enforce`) per configuration field; effective-value computation (auth override rules, rate-limit stricter-wins `min(ancestor, descendant)`, plugin chain concatenation, CORS origin union, tags add-only union); alias resolution walking descendant-to-root with closest-match shadowing while enforced ancestor constraints always remain in force; and descendant override gated on the authoritative permissions (`oagw:upstream:bind`, `oagw:upstream:override_auth`, `oagw:upstream:override_rate`, `oagw:upstream:add_plugins`).

**Rationale**: Multi-tenant hierarchical configuration is a core value proposition of the gear; the change must deliver it or the management surface would diverge from the contract.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-tenant-resolver`

#### Configuration Caching

- [x] `p2` - **ID**: `cpt-cf-oagw-fr-config-caching`

The gear **MUST** cache effective configuration for hot lookups in memory (control-plane and data-plane layers per the authoritative caching/state ADRs `0005`/`0006`) and **MUST** invalidate cached entries on configuration writes so a successful management write is visible to subsequent proxy requests without a stale window beyond the authoritative design.
The gear **MUST NOT** cache upstream responses (response caching is the client's/upstream's responsibility).

**Rationale**: The latency NFR (§6 `cpt-cf-oagw-nfr-proxy-overhead`) puts every proxy request on the hot path; config reads from persistent storage on every request would fail the target.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`

### 5.3 Data-Plane Proxying

#### Request Proxying

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-data-plane-proxy`

The gear **MUST** implement the data-plane proxy surface per the authoritative contract (`docs/PRD.md` §5.2 `cpt-cf-oagw-fr-request-proxy`, `docs/DESIGN.md` §3.2/§3.3, ADR 0001): resolve the upstream by alias (tenant-hierarchy walk with shadowing), match the route by method/path per the authoritative matching rules, merge effective configuration (upstream < route < tenant), execute the plugin chain in the authoritative order, and forward the request to the external service over a pooled, load-balanced connection layer, returning the upstream response to the caller. The gear **MUST** perform no automatic full client-request retries, while connector-level endpoint/connection failover or connection-retry attempts performed by the upstream connector are permitted. Multi-endpoint behavior (load-balance pool, target-host selection, header-required disambiguation, round-robin) **MUST** follow the authoritative ADR 0001 matrix.

**Rationale**: Proxying is the core value proposition and the largest missing surface; the DoD names proxying explicitly as behavior that must be served.

**Actors**: `cpt-cf-oagw-actor-proxy-client`

#### Credential Injection

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-credential-injection`

The gear **MUST** inject credentials into outbound requests through its built-in auth plugin set exactly as the authoritative contract defines (`docs/PRD.md` §5.2 `cpt-cf-oagw-fr-auth-injection`, ADR 0008): resolve secret material from the credential store by reference at request time, support the built-in methods (no authentication, API key header/query, OAuth2 Client Credentials Form and Basic variants with internal token caching), and never place secret material in logs, error messages, API responses, or cache keys. A credential resolution failure **MUST** surface as a gateway error consistent with the authoritative error contract (see `cpt-cf-oagw-fr-error-source-semantics`).

**Rationale**: Centralized, leak-free credential handling is the primary reason gears route outbound traffic through the gateway.

**Actors**: `cpt-cf-oagw-actor-proxy-client`, `cpt-cf-oagw-actor-credential-store`

#### Rate Limiting

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-rate-limit-enforcement`

The gear **MUST** enforce rate limits per the authoritative contract (`docs/PRD.md` §5.2 `cpt-cf-oagw-fr-rate-limiting`, ADR 0003): at upstream and route levels, with the authoritative algorithm (token-bucket default, sliding window optional), sustained/burst dual-rate fields, scope, cost, strategy (reject/queue/degrade), response headers per the authoritative headers convention, hierarchical stricter-wins semantics, and per-instance enforcement for the e2e change (distributed coordination out of scope). Exceeded limits **MUST** produce the authoritative rate-limit error with retry guidance.

**Rationale**: Rate limiting prevents abuse and cost overruns and protects external service agreements — a stated business goal of the gear.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`

#### Streaming Proxying

- [x] `p2` - **ID**: `cpt-cf-oagw-fr-stream-proxying`

The gear **MUST** support streaming per the authoritative contract (`docs/PRD.md` §5.4 `cpt-cf-oagw-fr-streaming`): HTTP request/response proxying and SSE streaming with correct connection lifecycle (open/close/error, client-disconnect propagation), and WebSocket/WebTransport session flows as the authoritative design defines them, with the error-source distinction carried on streaming responses.

**Rationale**: Many external APIs (e.g., LLM chat completions) stream; without streaming the gateway cannot serve a large class of upstreams the contract targets.

**Actors**: `cpt-cf-oagw-actor-proxy-client`

### 5.4 Plugin System

#### Plugin Execution

- [x] `p2` - **ID**: `cpt-cf-oagw-fr-plugin-execution`

The gear **MUST** implement the plugin runtime per the authoritative contract (`docs/PRD.md` §5.3 `cpt-cf-oagw-fr-plugin-system`/`cpt-cf-oagw-fr-builtin-plugins`, ADR 0002): three plugin types (Auth, Guard, Transform) with the authoritative deterministic execution order (Auth → Guards → Transform request → upstream call → Transform response/error; upstream plugins before route plugins); registration and resolution of the built-in plugin catalog as the authoritative design defines (auth: noop, apikey, oauth2_client_cred, oauth2_client_cred_basic; guard: required_headers; transform: request_id; with timeout/CORS/logging/metrics as core data-plane logic, and basic/bearer as catalog-only identifiers); plugin bindings on upstreams and routes with positional order; and plugin immutability with in-use protection on deletion. Circuit breaker is core resilience policy, not a plugin, per the authoritative design.

**Rationale**: The plugin system is the extensibility mechanism the contract promises; the change must at minimum deliver the built-in plugin runtime and binding model (custom Starlark execution is out of scope — see §4.2).

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-gts-registry`

### 5.5 CORS

#### CORS Enforcement

- [x] `p2` - **ID**: `cpt-cf-oagw-fr-cors-enforcement`

The gear **MUST** implement CORS as built-in behavior per the authoritative contract (ADR 0004): per-upstream/route CORS configuration (enabled, allowed origins, allowed methods, expose headers, allow-credentials with the authoritative validation that credentials cannot combine with a wildcard origin); preflight handling per the authoritative design (handled locally, permissive response echoing the request, no upstream resolution or tenant context required, bypassing per-request auth/plugin checks while remaining subject to infrastructure-level controls); actual-request enforcement of origin and method before forwarding; strict exact-match origin validation (no regex, protocol- and port-sensitive); deny-by-default (disabled unless configured); and the authoritative hierarchical merge (union under `inherit`, no additions under `enforce`) with the `Vary: Origin` guarantee.

**Rationale**: Browser-based clients are an explicit consumer class in the authoritative contract, and CORS misconfiguration is a leak vector; built-in, secure-by-default handling is required.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`

### 5.6 Error Semantics

#### Error Semantics and Source Distinction

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-error-source-semantics`

The gear **MUST** implement the authoritative error contract end to end (`docs/PRD.md` §5.6 `cpt-cf-oagw-fr-error-codes`, `docs/DESIGN.md` §3.3, ADR 0007): the authoritative error catalog (validation, authentication, not-found, payload-too-large, rate-limit, secret, downstream, circuit-breaker, timeout classes) with their authoritative retriable semantics; gateway-originated errors expressed per the authoritative problem-details format with GTS-typed identifiers; upstream-originated failures passed through unchanged; and every response carrying the authoritative error-source distinction mechanism so a caller can attribute a failure to the gateway or the upstream. Validation of inbound requests (path, query, headers, and body-size limits per the authoritative rules) **MUST** reject malformed requests with the authoritative validation error before any forwarding.

**Rationale**: Consistent, attributable errors are how clients implement correct retry and fallback behavior; the DoD names error semantics explicitly, and indistinguishability of gateway vs upstream failures would break consumers.

**Actors**: `cpt-cf-oagw-actor-proxy-client`

### 5.7 Security

#### Inbound Authorization

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-inbound-authz`

The gear **MUST** enforce the authoritative permission model on every inbound request to its API surface, with exact per-actor/operation permissions (see `docs/DESIGN.md` §3.2/§3.3):

- Gateway Operator on upstreams: create, override, read, and delete — `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}`.
- Gateway Operator on routes: create, override, read, and delete — `gts.cf.core.oagw.route.v1~:{create;override;read;delete}`.
- Gateway Operator on plugin definitions: create, read, delete for each plugin type — `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}`, `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}`.
- Proxy Client on the proxy surface: invoke — `gts.cf.core.oagw.proxy.v1~:invoke`, combined with the authoritative ownership rule (the target upstream must be owned by the caller's tenant or shared by an ancestor).
- Descendant override on ancestor configuration: gated on `oagw:upstream:bind`, `oagw:upstream:override_auth`, `oagw:upstream:override_rate`, `oagw:upstream:add_plugins` per the authoritative rules.
- CORS preflight: per the authoritative contract, handled without tenant context and without per-request authorization, while actual cross-origin requests remain subject to upstream resolution and the permission/ownership rules above.

Authorization **MUST** be enforced for the calling tenant (resource ownership and effective permissions) in addition to bearer authentication provided by the platform.

**Rationale**: The gateway manages credentials and outbound policy — authorization is precisely where misconfiguration causes cross-tenant exposure; the contract's exact permission model must be honored, not approximated.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`, `cpt-cf-oagw-actor-authz-resolver`

#### Outbound Security Policy

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-security-policy`

The gear **MUST** enforce an outbound security policy consistent with the authoritative contract (`docs/DESIGN.md` §2.2/§4.4): HTTPS-only upstream connections by default, with any HTTP-upstream allowance strictly config-gated (the e2e `allow_http_upstream: true` is an explicit test allowance, never a default); an SSRF policy that validates upstream targets (scheme allowlist, DNS/IP validation and IP-pinning rules consumed per authority, stripping of well-known internal headers, request path/query validation against route configuration); and body-size limits with rejection before buffering. When the SSRF policy is configured through the host slot (`ssrf_policy.enabled`), **MUST** honor it; the default (policy enabled) is the security baseline.

**Rationale**: As the platform's single outbound path, the gateway must not be usable as an SSRF vector; HTTPS-only and SSRF protection are the authoritative security baseline that the e2e allowances must never silently weaken in default operation.

**Actors**: `cpt-cf-oagw-actor-gateway-operator`, `cpt-cf-oagw-actor-proxy-client`

#### Secret Resolution

- [x] `p1` - **ID**: `cpt-cf-oagw-fr-secret-resolution`

The gear **MUST** resolve all secret material exclusively through the credential store by reference (authoritative `cred://` reference model and ADR 0008): never store, cache (outside the authoritative bounded token cache), log, or echo secret values; honor the credential store's tenant-access decisions (a secret inaccessible to the calling tenant fails closed as the authoritative authentication error); and treat credential-store unavailability safely (bounded cached-token serving for the OAuth2 plugins per ADR 0008, with failures surfaced as gateway errors on cache miss).

**Rationale**: Credential isolation and tenant isolation of secrets are stated NFRs of the gear; secret resolution is where both are realized at runtime.

**Actors**: `cpt-cf-oagw-actor-credential-store`, `cpt-cf-oagw-actor-proxy-client`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs (performance, security, reliability, scalability) are defined at project level. This section documents the NFRs that the change must meet or that deviate from/extend project defaults, all stated as business-level quality requirements consistent with the authoritative contract.
>
> **Testing strategy**: NFRs verified via automated benchmarks, security scans, and monitoring unless otherwise specified.

### 6.1 NFR Inclusions

#### Build Integration (DoD)

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-build-integration`

The workspace **MUST** build with the gear included: `cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"` **MUST** complete successfully, and the crate **MUST** compile without violating the workspace's lint denials (clippy::pedantic, unwrap/expect) on the workspace toolchain (edition 2024, resolver 3, rust-version 1.95.0).

**Threshold**: The DoD build command exits 0 with no warnings treated as errors and no dependency-resolution failures.

**Rationale**: A gear that does not build with its configured feature set can never be delivered; this is a hard gate for the change.

**Verification Method**: automated CI build of the DoD command.

#### Startup Health (DoD)

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-startup-health`

The server **MUST** start cleanly on `config/e2e-local.yaml`, bind :8086, and report healthy via the health endpoint (`/healthz`), remaining healthy across the change's runtime.

**Threshold**: Startup completes within the host's startup health window; the health endpoint reports healthy and stays healthy during a sustained smoke run.

**Rationale**: The DoD requires a running, healthy server on the e2e config; a gear that boots into a degraded or crash-looping state fails the change even if it compiles.

**Verification Method**: automated e2e smoke + integration test.

#### Proxy Overhead

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-proxy-overhead`

The gear **MUST** add less than 10ms of latency at p95 for proxy requests (excluding upstream response time), consistent with the authoritative latency NFR (`cpt-cf-oagw-nfr-low-latency`), and **MUST** enforce plugin-execution timeouts so a misbehaving plugin cannot arbitrarily inflate request latency.

**Threshold**: <10ms added latency at p95 measured over a representative proxy workload.

**Rationale**: The gateway sits on the hot path of every outbound call; the authoritative contract quantifies this target and the change must not regress it.

#### Availability

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-availability`

The gear **MUST** maintain 99.9% availability under normal operating conditions and **MUST** prevent cascade failures from unhealthy upstreams via the authoritative resilience mechanisms (circuit breaker per the authoritative thresholds; configurable timeouts).

**Threshold**: 99.9% uptime; circuit breaker trips per the authoritative failure thresholds.

**Rationale**: Gateway downtime blocks all outbound API calls — availability is the availability NFR of the authoritative contract carried by this change.

#### Concurrency Safety

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-concurrency-safety`

The gear **MUST** be safe and correct under concurrent load: concurrent proxy requests and management writes **MUST NOT** race, corrupt shared state (rate-limit counters, caches, registries, token cache), panic, or produce nondeterministic effective configuration, and the gear **MUST** shut down cleanly without data races.

**Threshold**: Concurrency tests exercising concurrent proxy + management + rate-limit + cache-invalidation traffic complete without panic or race (per the toolchain's race/UB checks where available, or stress tests otherwise).

**Rationale**: A gateway with thread-unsafe state is a reliability and security liability; the change ships a fresh implementation and must establish concurrency correctness from the start.

#### Secret Hygiene

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-secret-hygiene`

Credentials and secret material **MUST** never appear in logs, error messages, API responses, metrics, or cache keys, and all credential references **MUST** be handled per the authoritative isolation model (reference-based, tenant-isolated, zeroed where the implementation holds values).

**Threshold**: Zero credential material in any log, error, metric, or API output — asserted by tests.

**Rationale**: Credential leakage is the critical security risk of a gateway; the authoritative contract makes it an NFR and the change must verify it.

#### SSRF Safety

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-ssrf-safety`

The gear **MUST** be free of SSRF vulnerabilities as shipped: upstream targets validated against the SSRF policy, HTTPS-only default, internal-target protection active, and no path by which a caller can steer the proxy to disallowed targets.

**Threshold**: Zero SSRF vulnerabilities in security review/scan of the implemented surface.

**Rationale**: As the platform's outbound choke point the gateway must not be exploitable to reach internal hosts; the e2e test allowances never apply to this requirement's default baseline.

#### Observability

- [x] `p2` - **ID**: `cpt-cf-oagw-nfr-observability-metrics`

The gear **MUST** log proxy requests with correlation IDs and expose metrics consistent with the authoritative contract (`docs/DESIGN.md` §4.2/§4.3): request counts, durations, in-flight, error counts, circuit-breaker state, rate-limit state, routing and upstream-health signals, with the authoritative cardinality controls (no tenant labels, normalized route/method/status labels). Logs and metrics **MUST** contain no PII and no secrets, and high-volume logging **MUST** be rate-limited per the authoritative sampling guidance.

**Threshold**: 100% of proxy requests logged with correlation ID; metrics served per the authoritative vocabulary on the admin-only surface; log volume bounded per the authoritative sampling policy.

**Rationale**: Operators need full visibility into outbound traffic, errors, and performance; observability is an authoritative NFR delivered by this change.

#### Test Coverage (DoD)

- [x] `p1` - **ID**: `cpt-cf-oagw-nfr-test-coverage`

The change **MUST** be covered by automated tests (unit and integration) at crate level that verify the implemented behavior — registration/lifecycle, management operations, proxy execution, credential injection, rate limiting, CORS, caching/invalidation, and error semantics — and these tests **MUST** pass on the configured toolchain. No test code from the change **MUST** live under `testing/e2e/gears/oagw/`, which is reserved for acceptance.

**Threshold**: Implemented behavior exercised by green test suites; `testing/e2e/gears/oagw/` untouched by the change.

**Rationale**: The DoD makes test coverage a first-class deliverable and explicitly reserves the acceptance directory; shipping untested or acceptance-encroaching code fails the change.

### 6.2 NFR Exclusions

- **Accessibility (UX)**: not applicable — the gear is a server-side middleware with no end-user UI to make accessible.
- **Internationalization / localization (UX)**: not applicable — the API surface is operational/English per the authoritative contract; error text is defined by the authoritative catalog.
- **Regulatory / privacy compliance (GDPR/HIPAA/PCI)**: not applicable — the gear processes no end-user personal data, no healthcare data, and no payment data; platform-level compliance is handled at project level. Secret handling follows the authoritative credential-isolation model regardless.
- **Starlark sandbox / scriptable plugin isolation (authoritative p3 NFR)**: excluded from this change — custom Starlark execution is out of scope (§4.2); the requirement remains tracked on the authoritative roadmap.
- **Distributed rate-limit accuracy / Redis-backed enforcement**: excluded from this change — per-instance enforcement is the authoritative MVP and the e2e host provides no Redis tier; the authoritative distributed design is future work.
- **Offline capability (UX)**: not applicable — the gear is a networked proxy that must reach upstreams and the credential store; degraded-behavior expectations are captured in §5.7 (`cpt-cf-oagw-fr-secret-resolution`) and §12.
- All other project-default NFRs apply to this gear; the exclusions above are the only deviations for this change.

## 7. Public Library Interfaces

> The gear's externally observable REST surface (management and proxy APIs) is defined authoritatively as `cpt-cf-oagw-interface-management-api` and `cpt-cf-oagw-interface-proxy-api` in `docs/PRD.md` §7.1 with full detail in `docs/DESIGN.md` §3.3. That surface is NOT re-specified here; this section defines the crate-level interfaces this change introduces and the integration contracts the gear relies on.

### 7.1 Public API Surface

#### Gear Registration Entry Point

- [x] `p1` - **ID**: `cpt-cf-oagw-interface-gear-registration`

**Type**: Rust library crate (lib name `oagw`) — gear registration entry integrated by the host's gear registry.

**Stability**: stable within the workspace.

**Description**: The `cf-gears-oagw` crate's registration entry point, consumed by the host runtime via the workspace's gear-registration convention (linked in `apps/cf-gears-example-server/src/registered_gears.rs` behind the `oagw` feature). The crate must expose the gear definition (lifecycle, capabilities, route registration) such that the host discovers and loads it; the crate remains a normal Rust library with the workspace's `#[lib]` metadata intact (name `oagw`, path `src/lib.rs`).

**Breaking Change Policy**: Major version bump required (per workspace semver for `cf-gears-oagw`), and any change to the registration entry point must remain compatible with the host's convention at the version pinned in the workspace.

#### Configuration Model

- [x] `p1` - **ID**: `cpt-cf-oagw-interface-config-model`

**Type**: Rust config struct (serde-serializable) / data format.

**Stability**: stable within the workspace.

**Description**: The public configuration type the gear reads from the host-injected gear block (currently `proxy_timeout_secs`, `allow_http_upstream`, `ssrf_policy`), with safe defaults and additive, validated fields per the authoritative design (including token-cache settings per ADR 0008). The model is the contract between host configuration authors and the gear's runtime behavior.

**Breaking Change Policy**: Additive changes allowed within the current major version; removal or semantic change of an existing field requires a major version bump and coordination with any host configuration that sets it.

### 7.2 External Integration Contracts

#### Host Runtime Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-contract-host-runtime`

**Direction**: provided by gear to host runtime.

**Protocol/Format**: in-process ToolKit gear lifecycle, registration, route mounting, and configuration injection (workspace framework).

**Compatibility**: the gear must satisfy the host's lifecycle and loading contract at the workspace-pinned framework version, compiled behind the `oagw` feature; the host must be able to build and start with the gear present and absent (feature-gating must not break the server).

#### Authorization Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-contract-authz-resolver`

**Direction**: required from platform gear.

**Protocol/Format**: in-process Rust SDK call via the `authz-resolver` SDK (policy enforcer).

**Compatibility**: must match the `authz-resolver` SDK version in the workspace; permission identifiers consumed are those of the authoritative contract (§5.6 `cpt-cf-oagw-fr-inbound-authz`).

#### Types Registry Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-contract-gts-registry`

**Direction**: required from platform gear.

**Protocol/Format**: in-process Rust SDK call via the `types-registry` SDK (GTS registration/validation).

**Compatibility**: must match the `types-registry` SDK version in the workspace; the gear registers the authoritative GTS catalog identifiers and validates plugin/protocol identifiers against the registry.

#### Tenant Resolver Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-contract-tenant-resolver`

**Direction**: required from platform gear.

**Protocol/Format**: in-process Rust SDK call via the `tenant-resolver` SDK (tenant hierarchy resolution).

**Compatibility**: must match the `tenant-resolver` SDK version in the workspace; the gear consumes the tenant chain for sharing modes, alias shadowing, and enforced ancestor constraints.

#### Credential Store Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-contract-secret-store`

**Direction**: required from platform gear.

**Protocol/Format**: in-process Rust SDK call via the `credstore` SDK (secret retrieval by reference).

**Compatibility**: must match the `credstore` SDK version in the workspace; retrieval is by reference only, subject to the credential store's tenant-access decisions, and must support the e2e static-credstore setup.

## 8. Use Cases

#### Startup and Registration

- [x] `p1` - **ID**: `cpt-cf-oagw-usecase-startup-registration`

**Actor**: `cpt-cf-oagw-actor-host-runtime`

**Preconditions**:
- Server built with the e2e feature set, including the `oagw` feature.
- e2e configuration provides the `oagw` gear block.

**Main Flow**:
1. Host starts and discovers the compiled gear.
2. Host invokes the gear's lifecycle: configuration load, service initialization, capability registration.
3. Gear registers its API surface with the host's route table and its GTS catalog with the types registry.
4. Gear reaches ready state; host health reflects a healthy gateway.

**Postconditions**:
- The gear is loaded, configured, and routable; the host's health endpoint reports healthy.

**Alternative Flows**:
- **Misconfiguration**: invalid gear configuration is rejected at startup and startup fails loudly with a clear error rather than booting half-configured.
- **Dependency unavailable at startup**: a required platform dependency fails to initialize; startup fails loudly per lifecycle requirement.

#### Manage Upstream

- [x] `p1` - **ID**: `cpt-cf-oagw-usecase-manage-upstream`

**Actor**: `cpt-cf-oagw-actor-gateway-operator`

**Preconditions**:
- Operator authenticated and authorized with the upstream create/override/read/delete permission for their tenant scope.
- For descendant override or binding: the authoritative override permission (`oagw:upstream:bind` / `oagw:upstream:override_*`) and compatible sharing mode.

**Main Flow**:
1. Operator submits an upstream definition (endpoints, protocol, auth, headers, rate limits, CORS, plugins, tags) via the management surface.
2. Gear validates the definition per the authoritative rules (endpoint/scheme/host/port, alias derivation/enforcement, credential-reference validity, CORS credentials/origin constraints, uniqueness within tenant).
3. Gear persists the upstream, applies enable/disable and sharing semantics, and invalidates affected cached configuration.
4. Gear returns the created/updated upstream representation.

**Postconditions**:
- The upstream is available for alias resolution and proxying; configuration changes are visible to subsequent proxy requests without a stale window.

**Alternative Flows**:
- **Validation failure**: the operation is rejected with the authoritative validation error and no state change.
- **Alias/identity conflict or in-use conflict**: the operation is rejected with the authoritative conflict semantics.
- **Hierarchy constraint**: an ancestor-enforced constraint blocks the change (or a descendant without the override permission inherits ancestor configuration unchanged).

#### Proxy a Request

- [x] `p1` - **ID**: `cpt-cf-oagw-usecase-proxy-call`

**Actor**: `cpt-cf-oagw-actor-proxy-client`

**Preconditions**:
- Client authenticated and authorized with the proxy invoke permission; target upstream owned by the caller's tenant or shared by an ancestor.
- An enabled upstream and matching route exist.

**Main Flow**:
1. Client sends a request to the proxy surface identifying the upstream by alias.
2. Gear resolves the upstream by alias (tenant-hierarchy walk with shadowing) and matches the route.
3. Gear merges effective configuration (upstream < route < tenant), including enforced ancestor constraints.
4. Gear executes the plugin chain (auth → guards → request transform), resolving credentials by reference and applying rate limits and header/query/path validation.
5. Gear forwards the request to the external service over the pooled/load-balanced connection layer; upstream response (including streaming) is returned with the error-source distinction and response transforms applied.

**Postconditions**:
- The upstream response is returned to the caller; the request is logged with a correlation ID; rate-limit and metrics state updated.

**Alternative Flows**:
- **No route/upstream match or disabled upstream**: caller receives the authoritative gateway error.
- **Credential resolution failure**: caller receives the authoritative gateway error; no secret material is exposed.
- **Rate limit exceeded**: caller receives the authoritative rate-limit error with retry guidance per configured strategy.
- **Upstream failure**: upstream response/error is passed through unchanged and attributed via the error-source distinction.
- **CORS actual-request violation**: request rejected before forwarding per the authoritative CORS rules.

#### Report Health

- [x] `p1` - **ID**: `cpt-cf-oagw-usecase-health-reporting`

**Actor**: `cpt-cf-oagw-actor-host-runtime`

**Preconditions**:
- Server running on the e2e config with the gear loaded.

**Main Flow**:
1. A health probe is issued to the host's health endpoint (:8086).
2. Host aggregates gear readiness, including the gateway's ready state.
3. Host responds healthy when the gateway is up.

**Postconditions**:
- The health endpoint reports a healthy server including a registered, initialized gateway.

**Alternative Flows**:
- **Gear failed to initialize**: startup fails loudly (see lifecycle); a running-but-unhealthy gateway is not a supported state.

## 9. Acceptance Criteria

The change is accepted when all of the following are demonstrably true (traceable to the DoD in §1.2 and the requirements above):

- [x] The DoD build succeeds: `cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"` exits 0 (traces to `cpt-cf-oagw-nfr-build-integration`, `cpt-cf-oagw-fr-gear-registration`).
- [x] The server starts on `config/e2e-local.yaml`, binds :8086, and `/healthz` reports healthy (traces to `cpt-cf-oagw-nfr-startup-health`, `cpt-cf-oagw-usecase-startup-registration`).
- [x] Requests to the gear's management surface are served by the gear (not the host's unhandled-path response) and implement the authoritative CRUD, validation, tenant scoping, and hierarchy semantics (traces to `cpt-cf-oagw-fr-route-serving`, `cpt-cf-oagw-fr-control-plane-crud`, `cpt-cf-oagw-fr-config-hierarchy`).
- [x] Requests to the gear's proxy surface are served end to end: alias resolution, route matching, configuration layering, credential injection, rate limiting, header handling, CORS, and streaming per the authoritative contract (traces to `cpt-cf-oagw-fr-data-plane-proxy`, `cpt-cf-oagw-fr-credential-injection`, `cpt-cf-oagw-fr-rate-limit-enforcement`, `cpt-cf-oagw-fr-cors-enforcement`, `cpt-cf-oagw-fr-stream-proxying`).
- [x] Error semantics per the authoritative contract are demonstrated: authoritative error catalog and retriable semantics, problem-details gateway errors, upstream passthrough, and the error-source distinction on every response (traces to `cpt-cf-oagw-fr-error-source-semantics`).
- [x] Authorization per the authoritative permission model is enforced (exact per-actor/operation permissions of `cpt-cf-oagw-fr-inbound-authz`), verified for allowed and denied operations.
- [x] Security baseline holds: HTTPS-only by default, SSRF policy enforced when enabled, zero credential material in logs/errors/responses (traces to `cpt-cf-oagw-fr-security-policy`, `cpt-cf-oagw-fr-secret-resolution`, `cpt-cf-oagw-nfr-secret-hygiene`, `cpt-cf-oagw-nfr-ssrf-safety`).
- [x] The implementation ships with green automated tests (unit + integration) at crate level covering registration/lifecycle, management, proxy, credential injection, rate limiting, CORS, caching/invalidation, and error semantics; and `testing/e2e/gears/oagw/` contains no code authored by this change (traces to `cpt-cf-oagw-nfr-test-coverage`).
- [x] Concurrency-safety evidence exists: concurrent proxy + management + rate-limit + invalidation traffic runs without panic or race (traces to `cpt-cf-oagw-nfr-concurrency-safety`).
- [x] Observable behavior per contract: proxy requests logged with correlation ID and metrics exposed per the authoritative vocabulary, without PII or secrets (traces to `cpt-cf-oagw-nfr-observability-metrics`).

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Host runtime (cf-gears-example-server + ToolKit framework) | Gear discovery, lifecycle, configuration injection, route mounting, health aggregation | p1 |
| authz-resolver (gear + SDK) | Authorization of inbound requests against the authoritative permission model | p1 |
| types-registry (gear + SDK) | GTS catalog registration and identifier validation | p1 |
| tenant-resolver (gear + SDK) | Tenant hierarchy for sharing modes, alias shadowing, enforced constraints | p1 |
| credstore (gear + SDK) | Secret material retrieval by reference for credential injection | p1 |
| api-gateway (host surface) | Serves the host entry (including health on :8086) and composes gear-relative routes | p1 |
| Toolkit framework surface (`toolkit` gear macro, config injection, `OperationBuilder` route registration, capabilities) | The authoring surface the gear builds on to register with the host | p1 |
| Database capability (per authoritative design) | Persistence of upstream/route/plugin configuration; backend resolved at implementation (e2e host may supply a database-capability slot) | p2 |
| Pingora engine (per authoritative design) | Connection pooling / load balancing / streaming transport for outbound calls | p2 |

## 11. Assumptions

- The authoritative product contract (`docs/PRD.md`, `docs/DESIGN.md`, `docs/ADR/*.md`, `docs/schemas/*.json`) is binding for this change; this PRD constrains only the change dimension and never overrides it.
- All workspace wiring required for delivery already exists and is correct: the `oagw` feature in `config/e2e-features.txt`, the crate's feature-gated link in `apps/cf-gears-example-server/src/registered_gears.rs`, the `oagw = cf-gears-oagw v0.4.0` workspace mapping, and the `oagw` gear block in `config/e2e-local.yaml`. Only the crate source (`src/lib.rs`) and supporting in-crate modules need to be authored.
- The e2e config's `allow_http_upstream: true` and `ssrf_policy.enabled: false` are intentional test-only allowances; default behavior remains HTTPS-only with SSRF protection enabled, and the DoD tests exercise both the configured allowances and the default posture.
- The host composes gear-relative route prefixes; the authoritative `/api/oagw/v1/...` spelling is the api-gateway-nested form, and the runtime-visible paths must satisfy the contract once host composition is applied. Any residual ambiguity is resolved during design against the host's actual composition rather than by changing the contract.
- The e2e host provides the platform gears the change depends on (authz-resolver, types-registry, tenant-resolver, credstore, with static plugins) — the gear integrates with them via in-process SDKs and does not need to stub them for e2e.
- The crate's declared dependency set bounds what this change can deliver: custom Starlark plugin execution is not part of the change (no runtime dependency present); built-in plugin execution and plugin-definition management are. If contract conformance later requires scriptable plugins, that is a separate, tracked change.
- The gear's configuration persistence mechanism is resolved during design (authoritative design describes a relational persistence model); if persistence requires a host database-capability slot for the gear, the e2e `oagw` block will be extended by the implementation accordingly. A persistence-free MVP is acceptable only if it still satisfies the authoritative management semantics and the DoD.
- The e2e host has no Redis tier; rate limiting and config caching are per-instance in-memory for this change, per the authoritative MVP.
- The `oagw` acceptance path under `testing/e2e/gears/oagw/` is owned by acceptance; the implementation deliberately adds no files there, and doing so would be a scope violation.
- In-process SDK clients and workspace crates referenced by the change (credstore-sdk, authz-resolver-sdk, tenant-resolver-sdk, types-registry-sdk, toolkit-*, gts) compile and are available under the e2e feature set.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Scope exceeds implementable surface: full contract (Starlark execution, distributed rate limiting, L2 cache, gRPC) cannot be delivered by one change given declared dependencies | Change stalls or ships incomplete core behavior | Prioritize p1 DoD surface (registration, management, proxy, credential injection, rate limiting, CORS, error semantics); explicitly defer p2/p3 (Starlark, distributed coordination, gRPC) in scope as tracked follow-ups rather than silently dropping them |
| Route-prefix ambiguity (authoritative nested form vs gear-relative mount) mis-wired | API unreachable at the contract's public paths despite a "serving" gear | Resolve against the host's actual composition during design; verify reachability of the contract paths in acceptance-traceable tests; do not silently change the contract |
| e2e security allowances (HTTP upstream, SSRF disabled) mask security regressions | SSRF/plaintext flaws ship as "green" | Security-focused tests exercise the default posture (HTTPS-only, SSRF enabled) independent of the e2e allowances; security review of the outbound path gates acceptance |
| Credential store unavailability | Proxy requests fail when credentials cannot be resolved | Follow authoritative ADR 0008 bounded token caching; surface failures as gateway errors; alert on credential-store health |
| Concurrency defects in fresh implementation (limiters, caches, registries) | Races, panics, nondeterministic config under load | Concurrency-safety NFR with concurrent stress tests; shared-state ownership follows authoritative state-management ADR; workspace lint style enforcement |
| Configuration mismatch between new config keys and host block | Start fails or behavior silently differs from operator intent | Additive, validated, defaulted config fields; startup fails loudly on invalid values; e2e block extended only as needed and documented |
| Persistence dependency unavailable in e2e host | Management semantics cannot persist or startup fails | Decide persistence approach in design against host capabilities; if DB required, extend the e2e `oagw` block; keep authoritative management semantics intact |
| In-process SDK/contract drift with platform gears at pinned versions | Authorization, tenancy, or secret resolution breaks subtly | Integrate against workspace-pinned SDK versions; integration tests cover the real SDK paths used in e2e |
| Timebox on a large surface | Partial implementation that fails DoD | Milestone the DoD first (build → start → health → management → proxy → error semantics → tests); acceptance criteria in §9 are the gate |

## 13. Traceability

- **Authoritative product contract**: [docs/PRD.md](../PRD.md), [docs/DESIGN.md](../DESIGN.md), [docs/ADR/](../ADR/), [docs/schemas/](../schemas/)
- **Change surface implemented by this PRD**: gear registration and lifecycle (`cpt-cf-oagw-fr-gear-registration`, `cpt-cf-oagw-fr-gear-lifecycle`, `cpt-cf-oagw-fr-gear-configuration`), route serving (`cpt-cf-oagw-fr-route-serving`), control-plane management (`cpt-cf-oagw-fr-control-plane-crud`, `cpt-cf-oagw-fr-config-hierarchy`, `cpt-cf-oagw-fr-config-caching`), data-plane proxying (`cpt-cf-oagw-fr-data-plane-proxy`, `cpt-cf-oagw-fr-credential-injection`, `cpt-cf-oagw-fr-rate-limit-enforcement`, `cpt-cf-oagw-fr-stream-proxying`), plugin runtime (`cpt-cf-oagw-fr-plugin-execution`), CORS (`cpt-cf-oagw-fr-cors-enforcement`), error semantics (`cpt-cf-oagw-fr-error-source-semantics`), security (`cpt-cf-oagw-fr-inbound-authz`, `cpt-cf-oagw-fr-security-policy`, `cpt-cf-oagw-fr-secret-resolution`)
