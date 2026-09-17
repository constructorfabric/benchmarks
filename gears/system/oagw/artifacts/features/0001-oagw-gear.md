---
status: accepted
date: 2026-08-27
---

# FEATURE: OAGW Gear (Outbound API Gateway)


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows](#2-actor-flows)
  - [2.1 Gear Shell (F1) — Flow](#21-gear-shell-f1--flow)
  - [2.2 Control Plane (F2) — Flows](#22-control-plane-f2--flows)
  - [2.3 Proxy Data Plane (F3) — Flows](#23-proxy-data-plane-f3--flows)
  - [2.4 Plugin System (F4) — Flow (internal, no REST surface)](#24-plugin-system-f4--flow-internal-no-rest-surface)
  - [2.5 Rate Limit + CORS (F5) — Flows](#25-rate-limit--cors-f5--flows)
  - [2.6 Cross-Cutting (F6) — Flows](#26-cross-cutting-f6--flows)
  - [2.7 Tests (F7) — Verification only; no actor flows (DOC-FDESIGN-001: not applicable — this feature adds no user-facing behavior; its flows are the test programs themselves).](#27-tests-f7--verification-only-no-actor-flows-doc-fdesign-001-not-applicable--this-feature-adds-no-user-facing-behavior-its-flows-are-the-test-programs-themselves)
- [3. Processes / Business Logic](#3-processes--business-logic)
  - [3.1 Gear Shell (F1) — Algorithms](#31-gear-shell-f1--algorithms)
  - [3.2 Control Plane (F2) — Algorithms](#32-control-plane-f2--algorithms)
  - [3.3 Proxy Data Plane (F3) — Algorithms](#33-proxy-data-plane-f3--algorithms)
  - [3.4 Plugin System (F4) — Algorithms](#34-plugin-system-f4--algorithms)
  - [3.5 Rate Limit + CORS (F5) — Algorithms](#35-rate-limit--cors-f5--algorithms)
  - [3.6 Cross-Cutting (F6) — Algorithms](#36-cross-cutting-f6--algorithms)
  - [3.7 Tests (F7) — Verification-only; DOC-FDESIGN-001: no business algorithms — the coverage gate is specified in the F7 Definition of Done below.](#37-tests-f7--verification-only-doc-fdesign-001-no-business-algorithms--the-coverage-gate-is-specified-in-the-f7-definition-of-done-below)
- [4. States](#4-states)
  - [4.1 Gear Shell (F1) — States](#41-gear-shell-f1--states)
  - [4.2 Control Plane (F2) — States](#42-control-plane-f2--states)
  - [4.3 Proxy Data Plane (F3) — States](#43-proxy-data-plane-f3--states)
  - [4.4 Plugin System (F4) — States](#44-plugin-system-f4--states)
  - [4.5 Rate Limit + CORS (F5) — States](#45-rate-limit--cors-f5--states)
  - [4.6 Cross-Cutting (F6) — States](#46-cross-cutting-f6--states)
  - [4.7 Tests (F7) — States](#47-tests-f7--states)
- [5. Definitions of Done](#5-definitions-of-done)
  - [5.1 Gear Shell (F1) — `cpt-cf-oagw-feature-gear-shell`](#51-gear-shell-f1--cpt-cf-oagw-feature-gear-shell)
  - [5.2 Control Plane (F2) — `cpt-cf-oagw-feature-control-plane`](#52-control-plane-f2--cpt-cf-oagw-feature-control-plane)
  - [5.3 Proxy Data Plane (F3) — `cpt-cf-oagw-feature-proxy-data-plane`](#53-proxy-data-plane-f3--cpt-cf-oagw-feature-proxy-data-plane)
  - [5.4 Plugin System (F4) — `cpt-cf-oagw-feature-plugin-system`](#54-plugin-system-f4--cpt-cf-oagw-feature-plugin-system)
  - [5.5 Rate Limit + CORS (F5) — `cpt-cf-oagw-feature-rate-limit-cors`](#55-rate-limit--cors-f5--cpt-cf-oagw-feature-rate-limit-cors)
  - [5.6 Cross-Cutting (F6) — `cpt-cf-oagw-feature-cross-cutting`](#56-cross-cutting-f6--cpt-cf-oagw-feature-cross-cutting)
  - [5.7 Tests (F7) — `cpt-cf-oagw-feature-tests`](#57-tests-f7--cpt-cf-oagw-feature-tests)
- [6. Acceptance Criteria](#6-acceptance-criteria)
  - [6.1 Gear Shell (F1)](#61-gear-shell-f1)
  - [6.2 Control Plane (F2)](#62-control-plane-f2)
  - [6.3 Proxy Data Plane (F3)](#63-proxy-data-plane-f3)
  - [6.4 Plugin System (F4)](#64-plugin-system-f4)
  - [6.5 Rate Limit + CORS (F5)](#65-rate-limit--cors-f5)
  - [6.6 Cross-Cutting (F6)](#66-cross-cutting-f6)
  - [6.7 Tests (F7)](#67-tests-f7)
- [7. Scope](#7-scope)
  - [7.1 In Scope (REQUIRED — MVP)](#71-in-scope-required--mvp)
  - [7.2 Out of Scope (OPTIONAL / future per accepted docs and pipeline ADRs)](#72-out-of-scope-optional--future-per-accepted-docs-and-pipeline-adrs)
- [8. Test Scenarios](#8-test-scenarios)
  - [8.1 Unit Test Scenarios](#81-unit-test-scenarios)
  - [8.2 Integration Test Scenarios (httpmock)](#82-integration-test-scenarios-httpmock)
  - [8.3 Reserved Location](#83-reserved-location)
- [9. Non-Functional Requirements](#9-non-functional-requirements)
- [10. Traceability](#10-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-oagw-gear`

## 1. Feature Context

- [ ] `p1` - `cpt-cf-oagw-feature-gear-shell`
- [ ] `p2` - `cpt-cf-oagw-feature-control-plane`
- [ ] `p3` - `cpt-cf-oagw-feature-proxy-data-plane`
- [ ] `p4` - `cpt-cf-oagw-feature-plugin-system`
- [ ] `p5` - `cpt-cf-oagw-feature-rate-limit-cors`
- [ ] `p6` - `cpt-cf-oagw-feature-cross-cutting`
- [ ] `p7` - `cpt-cf-oagw-feature-tests`

### 1.1 Overview

This document is the acceptance-oriented FEATURE specification for implementing the `oagw` gear (crate `cf-gears-oagw`, at `/app/gears/system/oagw/oagw`, whose `src/lib.rs` is currently empty) in the Constructor Fabric gears-rust workspace. It elaborates the seven dependency-ordered work items decomposed in the pipeline DECOMPOSITION artifact into one specification that is precise enough to implement and test each work item independently, delivering the complete MVP for the gear's accepted PRD, DESIGN.md, and docs ADR set with inline `@cpt-*` traceability.

The gear is a standard `toolkit::gear` REST component (`capabilities = [rest]`, not `rest_host`) hosted on the `api-gateway` surface (`@cpt-cf-oagw-fr-gear-registration`). It realizes the Control Plane / Data Plane separation prescribed by DESIGN.md (§3 component model and §5 "Gear Structure" module tree) in a single crate using DDD-Light layering (`api -> domain <- infra`; the domain layer depends only on repository traits, never on infrastructure). Control-plane configuration (upstreams, routes, plugins) is held in tenant-scoped, in-memory repositories behind domain repository traits — DB-backed persistence and distributed state are out of scope for the MVP (`@cpt-cf-oagw-nfr-build-constraints`, `@cpt-cf-oagw-adr-gear-architecture`). The data plane is request-driven: an in-crate axum proxy handler on the host router invokes the data-plane service per request with no background task (`@cpt-cf-oagw-adr-proxy-data-plane`).

Every feature below marks its REQUIRED (MVP) content against the accepted `docs/ADR/` behavior records; OPTIONAL/future capabilities (Redis-backed distributed rate limiting, Starlark tenant-defined plugin execution, DB-backed repositories, WebSocket/WebTransport proxying, response caching, automatic retries) are explicitly declared out of scope in the features that touch them and are never claimed as delivered here (`@cpt-cf-oagw-nfr-build-constraints`, `@cpt-cf-oagw-adr-plugins-rate-limit-cors`).

The seven features are dependency-ordered: F1 Gear Shell -> F2 Control Plane -> F3 Proxy Data Plane -> (F4 Plugin System, F5 Rate Limit + CORS) -> F6 Cross-Cutting -> F7 Tests. Each later feature consumes the seams opened by earlier ones; the acceptance test scenarios in F7 are the aggregate gate over the whole feature set.

### 1.2 Purpose

Deliver the complete OAGW MVP in this repository, on schedule and within the workspace lockfile-only dependency constraint, so that an `api-gateway` deployment can: manage tenant-scoped upstreams, routes, and plugin records via a REST management API; proxy `/oagw/v1/proxy/{alias}[/{path_suffix}]` traffic to resolved upstreams with route matching, header/body validation, host semantics, plugin processing, rate limiting, and CORS handling; and surface every gateway-originated failure as an RFC 9457 problem+json response with the `X-OAGW-Error-Source` header per accepted ADR 0007. Each feature's Definition of Done below is the testable contract for that slice; the aggregate is confirmed by the F7 integration/e2e-adjacent suite (`@cpt-cf-oagw-fr-request-proxy`).

### 1.3 Actors

- [ ] `p2` - `cpt-cf-oagw-actor-platform-operator` — configures `oagw` section values via `OagwConfig` in the gear config file; no runtime surface is exposed to this actor beyond configuration parsing at startup.
- [ ] `p2` - `cpt-cf-oagw-actor-tenant-admin` — calls the tenant-scoped management endpoints to create/list/delete upstreams, routes, and plugin records; receives RFC 9457 errors on violations.
- [ ] `p2` - `cpt-cf-oagw-actor-app-developer` — consumes the gateway by issuing proxied requests (including SSE) and cross-origin browser requests; receives proxied responses or gateway errors with `X-OAGW-Error-Source`.
- [ ] `p2` - `cpt-cf-oagw-actor-upstream-service` — receives proxied requests and returns responses that pass through the data plane unmodified (`@cpt-cf-oagw-fr-passthrough`).
- [ ] `p2` - `cpt-cf-oagw-actor-cred-store` — supplies credentials (API keys, OAuth2 client-credentials secrets) to auth plugins by `cred_store` reference during proxied-request processing.
- [ ] `p2` - `cpt-cf-oagw-actor-types-registry` — supplies protocol/GTS type identifiers for gateway error `type` fields during data-plane error serialization.
- [ ] `p2` - `cpt-cf-oagw-actor-tenant-resolver` — supplies the calling tenant identity used to scope every management and proxy operation.

### 1.4 References

- `/app/gears/system/oagw/artifacts/PRD.md` — requirements, interfaces, contracts, actors, and acceptance criteria; source of all `@cpt-cf-oagw-*` ids without a `docs/` prefix.
- `/app/gears/system/oagw/artifacts/DESIGN.md` — gear module tree, `OagwConfig`, error table, header rules, management API, permissions, and configuration shape.
- `/app/gears/system/oagw/artifacts/DECOMPOSITION.md` — the seven work items this document elaborates.
- `/app/gears/system/oagw/artifacts/ADR/0001-oagw-gear-architecture.md`, `0002-proxy-data-plane.md`, `0003-plugins-rate-limit-cors.md` — in-repo pipeline ADRs binding control-plane shape, data-plane technology, and plugin/rate-limit/CORS scoping.
- `/app/gears/system/oagw/docs/DESIGN.md` and `/app/gears/system/oagw/docs/ADR/0001..0009` — the gear's authoritative accepted behavior records and REQUIRED markers.
- `/app/gears/system/oagw/oagw/Cargo.toml` — dependency universe (lockfile-only constraint).
- `/app/gears/system/oagw/docs/schemas/upstream.v1.schema.json`, `route.v1.schema.json` — management DTO validation contract.
- Platform worked examples: `/app/gears/system/types-registry/types-registry/src/gear.rs`, `config.rs`, `api/rest/error.rs`.
- Kit: `/app/.cf-studio/config/kits/sdlc/artifacts/FEATURE/template.md`, `rules.md`, `checklist.md`.

## 2. Actor Flows

The management flows (F2), proxy flow (F3), plugin/cors/rate-limit flows (F4/F5), and opportunities (F6) are elaborated under each feature heading below. F4 (plugin system) has no REST surface of its own beyond the F2 plugin-record endpoints; F7 (tests) is a verification-only work item with no actor flows. Non-applicability statements for DOC-FDESIGN-001 completeness appear inline in those features.

### 2.1 Gear Shell (F1) — Flow

- [ ] `p1` - `cpt-cf-oagw-flow-gear-shell-config` - IF - local run of the `oagw` section config is absent or partial - THEN - fall back to `Default` for `OagwConfig` per toolkit `config_or_default()` semantics; a missing section SHALL NOT fail startup.
- [ ] `p1` - `cpt-cf-oagw-flow-gear-shell-init-deps` - IF - `Gear::init` loads `OagwConfig` - THEN - resolve the dependencies named in the gear declaration `deps` (types-registry, cred-store, tenant-resolver, authz-resolver per DESIGN.md §3.4) through the `Deps` accessor and construct the capability wiring for the data plane.
- [ ] `p1` - `cpt-cf-oagw-flow-gear-shell-register` - IF - `RestApiCapability::register_rest` is invoked - THEN - return the host router carrying the management route group (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`) and the proxy route group (`/oagw/v1/proxy/{alias}[/{path_suffix}]`); the host applies `prefix_path`, the gear never nests its own router (`@cpt-cf-oagw-interface-host-registration`).
- [ ] `p1` - `cpt-cf-oagw-flow-gear-shell-build` - IF - the crate is built under the workspace e2e feature set - THEN - no change to the workspace lockfile occurs; `cf-gears-oagw` compiles against the declared dependency universe only (`@cpt-cf-oagw-nfr-build-constraints`).

### 2.2 Control Plane (F2) — Flows

- [ ] `p2` - `cpt-cf-oagw-flow-controller-upstream-mgmt` - FOR EACH - upstream create/list/delete request on `/oagw/v1/upstreams` - THEN - resolve the calling tenant via the tenant-resolver dependency, bind the operation to that tenant's repository scope, validate the payload against `upstream.v1.schema.json`, and return `201`/`200`/`204` on success or the RFC 9457 error body on violation (`@cpt-cf-oagw-fr-upstream-mgmt`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-upstream-alias-conflict` - IF - the requested alias already exists in the tenant scope - THEN - reject with `UpstreamAliasConflict` (`cf.oagw.upstream.alias_conflict.v1`, 409) and do not overwrite (`@cpt-cf-oagw-nfr-multi-tenancy`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-route-mgmt` - FOR EACH - route create/list/delete request on `/oagw/v1/routes` - THEN - validate against `route.v1.schema.json` (match oneOf http/grpc; path; methods allowlist subset of GET/POST/PUT/DELETE/PATCH; priority), scope to the calling tenant, and return the configured result shape (`@cpt-cf-oagw-fr-route-mgmt`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-route-conflict` - IF - the new route would conflict with an existing route on the same tenant/upstream (method set and path prefix order tie) - THEN - reject with `RouteMatchConflict` (`cf.oagw.route.conflict.v1`, 409) (`@cpt-cf-oagw-fr-route-matching`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-plugin-mgmt` - FOR EACH - plugin create/delete request on `/oagw/v1/plugins` - THEN - validate the plugin record (name, type category in `auth`/`guard`/`transform` catalog, config object), bind to the calling tenant, and keep the created record immutable (`@cpt-cf-oagw-fr-plugin-lifecycle`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-plugin-in-use` - IF - a delete targets a plugin record that is referenced by a route in the same tenant - THEN - reject with `PluginInUse` (`cf.oagw.plugin.in_use.v1`, 409) and leave the record (`@cpt-cf-oagw-fr-plugin-lifecycle`).
- [ ] `p2` - `cpt-cf-oagw-flow-controller-plugin-catalog-only` - IF - the plugin identifier is a catalog-only identifier (e.g. `basic`/`bearer` auth, `timeout`/`cors` guard, `logging`/`metrics` transform) - THEN - the record may be stored as a catalog reference in the repository but the identifier is never bound to an executable built-in (`@cpt-cf-oagw-fr-builtin-plugins`).

### 2.3 Proxy Data Plane (F3) — Flows

- [ ] `p3` - `cpt-cf-oagw-flow-proxy-execute` - IF - a request arrives on `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` - THEN - (1) resolve the upstream by alias with a tenant chain walk, (2) match a route by (upstream, method, longest path prefix, priority), (3) apply `X-OAGW-Target-Host` semantics (validate when present, round-robin when absent), (4) run the plugin chain (Auth -> Guards(request) -> Transform(request)), (5) apply rate-limit and CORS checks, (6) validate headers and body, (7) forward outbound to the resolved target, and (8) relay the upstream response with transform(response/error) applied (`@cpt-cf-oagw-fr-request-proxy`, `@cpt-cf-oagw-fr-alias-resolution`, `@cpt-cf-oagw-fr-route-matching`, `@cpt-cf-oagw-fr-target-host`).
- [ ] `p3` - `cpt-cf-oagw-flow-proxy-route-not-found` - IF - no matching route exists - THEN - respond with `RouteNotFound` (`cf.oagw.route.not_found.v1`, 404) (`@cpt-cf-oagw-fr-route-matching`).
- [ ] `p3` - `cpt-cf-oagw-flow-proxy-body-reject` - IF - the request fails header or body validation (Content-Length mismatch, over 100 MB ceiling, or non-chunked transfer encoding) - THEN - reject before any forwarding with `ValidationError` (400) or `PayloadTooLarge` (413, `cf.oagw.payload.too_large.v1`) (`@cpt-cf-oagw-fr-body-size`, `@cpt-cf-oagw-nfr-input-validation`).
- [ ] `p3` - `cpt-cf-oagw-flow-proxy-upstream-fail` - IF - an upstream is unavailable, a connection/request/idle timeout occurs, or the circuit breaker is open - THEN - respond with `LinkUnavailable` (503, `cf.oagw.link.unavailable.v1`), `timeout.*` (504), or `CircuitBreakerOpen` (503, `cf.oagw.circuit_breaker.open.v1`) respectively, all retriable (`@cpt-cf-oagw-fr-error-codes`, `@cpt-cf-oagw-fr-automatic-retries-disabled`).
- [ ] `p3` - `cpt-cf-oagw-flow-proxy-error-contract` - FOR EACH - gateway-originated failure - THEN - serialize an RFC 9457 `application/problem+json` body carrying the GTS `type` identifier and set `X-OAGW-Error-Source: gateway`; every proxied upstream response (including upstream errors) passes through unmodified with `X-OAGW-Error-Source: upstream` (`@cpt-cf-oagw-fr-error-source-distinction`, `@cpt-cf-oagw-adr-0007-error-source`).
- [ ] `p3` - `cpt-cf-oagw-flow-proxy-sse` - IF - the upstream responds with `text/event-stream` - THEN - stream events with an explicit open/close/error lifecycle; on upstream close or client disconnect, end the stream and record the terminal state without retry (`@cpt-cf-oagw-fr-streaming`).

### 2.4 Plugin System (F4) — Flow (internal, no REST surface)

- [ ] `p4` - `cpt-cf-oagw-flow-plugin-system-exec-chain` - FOR EACH - proxied request on a route referencing plugin records - THEN - compose the chain in deterministic order Auth -> Guards(request) -> Transform(request) -> upstream -> Transform(response/error), with upstream-assigned plugins executed before route-assigned plugins within each phase; a failing Auth SHALL short-circuit with `AuthenticationFailed` (401, `cf.oagw.auth.failed.v1`), a rejecting Guard with its phase-appropriate status, and a Transform error with `ProtocolError`/`DownstreamError` mapping (`@cpt-cf-oagw-fr-plugin-system`, `@cpt-cf-oagw-fr-builtin-plugins`).
- [ ] `p4` - `cpt-cf-oagw-flow-plugin-system-unknown-plugin` - IF - the chain references an unknown plugin identifier or a catalog-only identifier - THEN - fail resolution with the gateway "unknown plugin" condition mapped to `PluginNotFound` (503, `cf.oagw.plugin.not_found.v1`) (`@cpt-cf-oagw-fr-builtin-plugins`, `@cpt-cf-oagw-adr-plugins-rate-limit-cors`).

### 2.5 Rate Limit + CORS (F5) — Flows

- [ ] `p5` - `cpt-cf-oagw-flow-rate-limit-enforce` - IF - a proxied request maps to rate-limit configuration - THEN - evaluate the per-instance token bucket (sustained rate + burst capacity) for the configured scope, merge with ancestor (Upstream -> Route -> Tenant priority) caps via hierarchical `min()` on effective rates, and apply the configured reject/queue/degrade strategy (`@cpt-cf-oagw-fr-rate-limiting`, `@cpt-cf-oagw-fr-hierarchical-config`).
- [ ] `p5` - `cpt-cf-oagw-flow-rate-limit-reject` - IF - the reject strategy declares the bucket empty - THEN - respond with `RateLimitExceeded` (429, `cf.oagw.rate_limit.exceeded.v1`) carrying `X-RateLimit-*` headers and `Retry-After`, retriable (`@cpt-cf-oagw-fr-rate-limiting`, `@cpt-cf-oagw-usecase-rate-limit-exceeded`).
- [ ] `p5` - `cpt-cf-oagw-flow-cors-preflight` - IF - the request is a browser preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) - THEN - answer locally with a permissive 204 fast path — no upstream resolution and no per-request auth — echoing allowed origin/method/headers per configuration and always including `Vary: Origin` (`@cpt-cf-oagw-fr-cors`, `@cpt-cf-oagw-usecase-cors-preflight`).
- [ ] `p5` - `cpt-cf-oagw-flow-cors-actual-enforce` - IF - the actual (non-preflight) cross-origin request carries a disallowed origin or method after upstream resolution - THEN - respond with `OriginNotAllowed` (403, `cf.oagw.cors.origin_not_allowed.v1`) or `MethodNotAllowed` (403, `cf.oagw.cors.method_not_allowed.v1`) (`@cpt-cf-oagw-fr-cors`).

### 2.6 Cross-Cutting (F6) — Flows

- [ ] `p6` - `cpt-cf-oagw-flow-cross-cut-observability` - FOR EACH - management and proxy request - THEN - emit host-standard tracing (request ID propagation, tenant/principal extraction) and the gear's metrics vocabulary (`oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_errors_total` by error type, `oagw_circuit_breaker_state`, `oagw_rate_limit_exceeded_total`, per DESIGN.md) with the agreed log field set (`@cpt-cf-oagw-nfr-observability`).
- [ ] `p6` - `cpt-cf-oagw-flow-cross-cut-config-layering` - FOR EACH - configuration read during proxy processing - THEN - apply the merge order Upstream < Route < Tenant with child winning on equal priority and enforced ancestor caps honored (never bypassed by shadowing) (`@cpt-cf-oagw-fr-config-layering`, `@cpt-cf-oagw-fr-hierarchical-config`).

### 2.7 Tests (F7) — Verification only; no actor flows (DOC-FDESIGN-001: not applicable — this feature adds no user-facing behavior; its flows are the test programs themselves).

## 3. Processes / Business Logic

### 3.1 Gear Shell (F1) — Algorithms

- [ ] `p1` - **`cpt-cf-oagw-algo-gear-shell-init`** - `Gear::init` algorithm: read the `oagw` config section - IF - absent or partial - THEN - `Default::default()` - ELSE - decode into `OagwConfig` (fields per DESIGN.md: `proxy_timeout_secs` default 30 (e2e 2), `allow_http_upstream` default false (e2e true), `ssrf_policy.enabled` default true (e2e false), `token_cache.{ttl_secs=300, capacity=10000}`) - THEN - construct repositories, plugin registries, and services for the capability wiring (`@cpt-cf-oagw-fr-gear-registration`, `@cpt-cf-oagw-adr-gear-architecture`).
- [ ] `p1` - **`cpt-cf-oagw-algo-gear-shell-register`** - `RestApiCapability::register_rest` algorithm: mount `/oagw/v1` route groups (management + proxy) on the host router - IF - the host rejects a duplicate path - THEN - propagate the registration error - ELSE - return the router (`@cpt-cf-oagw-interface-management-api`, `@cpt-cf-oagw-interface-proxy-api`).

### 3.2 Control Plane (F2) — Algorithms

- [ ] `p2` - **`cpt-cf-oagw-algo-control-plane-alias-derive`** - Derived-alias algorithm: compute the tenant-derived alias for multi-endpoint common-suffix pools using the PSL (public suffix list) suffix match - IF - the alias hostname/IP violates hostname rules - THEN - reject with `ValidationError` (400) - ELSE - accept with the common suffix retained (`@cpt-cf-oagw-fr-alias-resolution`, `@cpt-cf-oagw-nfr-ssrf-protection`).
- [ ] `p2` - **`cpt-cf-oagw-algo-control-plane-route-conflict`** - Route-conflict algorithm: compare the candidate route against resident routes of the same tenant/upstream - IF - method sets intersect AND the path prefix ordering ties under priority - THEN - return `RouteMatchConflict` (409) - ELSE - persist (`@cpt-cf-oagw-fr-route-mgmt`, `@cpt-cf-oagw-fr-route-matching`).
- [ ] `p2` - **`cpt-cf-oagw-algo-control-plane-repo-scoping`** - Repository-scoping algorithm: every repository read/write receives the resolved tenant id and keys its internal maps by tenant - IF - the tenant is not resolvable - THEN - reject with the tenant-resolution error - ELSE - proceed scoped (`@cpt-cf-oagw-nfr-multi-tenancy`, `@cpt-cf-oagw-adr-gear-architecture`).

### 3.3 Proxy Data Plane (F3) — Algorithms

- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-alias-walk`** - Alias-resolution walk: starting from the request tenant, walk toward root - IF - a tenant in the chain defines the alias - THEN - select closest-match (descendant wins) - ELSE - fall through to root - IF - no tenant defines the alias - THEN - `RouteNotFound` (404) - ELSE - use the selected upstream; enforced ancestors are never bypassed by shadowing (`@cpt-cf-oagw-fr-alias-resolution`, `@cpt-cf-oagw-adr-0001-alias-shadowing`).
- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-route-match`** - Route-matching algorithm: collect candidate routes for (upstream, method) - THEN - score by path prefix against the proxied path - THEN - select the longest path prefix, breaking ties by configured priority - IF - no candidate matches - THEN - `RouteNotFound` (404) - ELSE - proceed; check the query allowlist if configured (`@cpt-cf-oagw-fr-route-matching`, `@cpt-cf-oagw-nfr-ssrf-protection`).
- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-target-host`** - Target-host algorithm: - IF - `X-OAGW-Target-Host` absent - THEN - select a pool member round-robin (for multi-endpoint pools) - ELSE - validate format and membership - IF - invalid or unknown - THEN - `InvalidTargetHost`/`UnknownTargetHost` (400) - ELSE - use the named member (`@cpt-cf-oagw-fr-target-host`, `@cpt-cf-oagw-adr-0001-target-host-matrix`).
- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-headers`** - Header-transformation algorithm: consume routing headers (without forwarding); strip hop-by-hop headers; replace request `Host` with the upstream host (and `:authority` on HTTP/2); apply passthrough/transform rules from upstream/route header configuration (`@cpt-cf-oagw-fr-header-transform`, `@cpt-cf-oagw-adr-0001-headers`).
- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-body-validate`** - Body-validation algorithm: - IF - `Content-Length` present - THEN - parse as integer - IF - it diverges from the actual size - THEN - `ValidationError` (400) - IF - size exceeds the 100 MB ceiling - THEN - `PayloadTooLarge` (413) before buffering - IF - transfer encoding is not `chunked` (and not absent) - THEN - `ValidationError` (400) (`@cpt-cf-oagw-fr-body-size`, `@cpt-cf-oagw-nfr-input-validation`).
- [ ] `p3` - **`cpt-cf-oagw-algo-proxy-data-plane-timeouts`** - Timeout algorithm: apply `proxy_timeout_secs` (connection/request/idle) from `OagwConfig` - IF - a timeout fires - THEN - map to `timeout.*` (504, retriable) with no automatic retry (`@cpt-cf-oagw-fr-error-codes`, `@cpt-cf-oagw-principle-no-retry`).

### 3.4 Plugin System (F4) — Algorithms

- [ ] `p4` - **`cpt-cf-oagw-algo-plugin-system-chain-compose`** - Chain-composition algorithm: order all referenced plugins Auth -> Guards(request) -> Transform(request) -> upstream -> Transform(response/error) - FOR EACH - phase - IF - upstream and route both assign plugins for the phase - THEN - upstream first, then route - ELSE - deterministic catalog order within phase (`@cpt-cf-oagw-fr-plugin-system`, `@cpt-cf-oagw-adr-0002-plugin-exec-order`).
- [ ] `p4` - **`cpt-cf-oagw-algo-plugin-system-auth-oauth2-cache`** - OAuth2 client-credentials token-cache algorithm (Form and Basic variants): key = `(subject_tenant_id, subject_id, auth_method, config_hash)`; - IF - key present in `pingora-memory-cache` - THEN - verify the cached token via the `CachedToken` key-verification wrapper (hash-collision safety) - IF - verification fails - THEN - treat as miss - ELSE - use the token - IF - miss - THEN - fetch from the identity provider - IF - fetch fails - THEN - do not cache, propagate `AuthenticationFailed` (401) - ELSE - cache with TTL `min(config_ttl, expires_in - 30s)` (`@cpt-cf-oagw-fr-oauth2-token-cache`, `@cpt-cf-oagw-adr-0008-oauth2-cache`).
- [ ] `p4` - **`cpt-cf-oagw-algo-plugin-system-guard-required-headers`** - Required-headers guard algorithm (per accepted ADR 0009): presence-only, case-insensitive matching; request phase rejects with 400 and response phase rejects with 502; first missing header reported; fail-open (allow) when the guard is unconfigured (`@cpt-cf-oagw-fr-required-headers-guard`, `@cpt-cf-oagw-adr-0009-required-headers`).
- [ ] `p4` - **`cpt-cf-oagw-algo-plugin-system-request-id`** - Request-ID transform algorithm: inject `X-Request-ID` when absent; propagate an existing value; stable within a request across chain and error paths (`@cpt-cf-oagw-fr-build-headers`, `@cpt-cf-oagw-fr-request-proxy`).

### 3.5 Rate Limit + CORS (F5) — Algorithms

- [ ] `p5` - **`cpt-cf-oagw-algo-rate-limit-cors-bucket`** - Token-bucket algorithm (per-instance, data-plane-owned): maintain sustained-rate refill and burst-capacity ceiling for scope `global|tenant|user|ip|route`; hierarchical merge applies `min(parent_effective, child)` - IF - tokens < cost - THEN - apply the configured strategy (`reject` -> 429 with `X-RateLimit-*` + `Retry-After`; `queue` -> bounded wait then proceed or 429; `degrade` -> proceed without enforcement) (`@cpt-cf-oagw-fr-rate-limiting`, `@cpt-cf-oagw-adr-0003-rate-limit`).
- [ ] `p5` - **`cpt-cf-oagw-algo-rate-limit-cors-preflight`** - Preflight algorithm: - IF - `OPTIONS` AND `Origin` present AND `Access-Control-Request-Method` present - THEN - answer 204 locally with permissive headers per configuration, always `Vary: Origin`; no upstream resolution, no per-request auth (`@cpt-cf-oagw-fr-cors`, `@cpt-cf-oagw-adr-0004-cors-fast-path`).
- [ ] `p5` - **`cpt-cf-oagw-algo-rate-limit-cors-actual`** - Actual-request algorithm: after upstream resolution, - IF - origin absent or disallowed (exact port- and protocol-sensitive match) - THEN - `OriginNotAllowed` (403) - ELSE - IF - method disallowed - THEN - `MethodNotAllowed` (403) - ELSE - proceed with `Vary: Origin`; sharing-mode merge: origins unioned on `inherit`, no additions on `enforce`; `allow_credentials` with wildcard origin rejected at configuration time (`@cpt-cf-oagw-fr-cors`, `@cpt-cf-oagw-feature-rate-limit-cors`).

### 3.6 Cross-Cutting (F6) — Algorithms

- [ ] `p6` - **`cpt-cf-oagw-algo-cross-cutting-config-merge`** - Configuration-merge algorithm: layered merge Upstream < Route < Tenant for plugin chains, rate-limit caps, and CORS settings; child values win on equal priority except where an ancestor cap is enforced (rate caps) - `min`-merged (`@cpt-cf-oagw-fr-config-layering`, `@cpt-cf-oagw-fr-hierarchical-config`).
- [ ] `p6` - **`cpt-cf-oagw-algo-cross-cutting-type-registration`** - Type-registration algorithm: at gear init, register protocol/GTS identifiers required by the error table with types-registry - IF - a registration conflicts - THEN - map to the types-registry error surfaced through the host - ELSE - complete (`@cpt-cf-oagw-fr-error-codes`, `@cpt-cf-oagw-adr-0007-error-source`).
- [ ] `p6` - **`cpt-cf-oagw-algo-cross-cutting-metrics`** - Metrics algorithm: attribute every request with method/route/status dimensions; increment `oagw_requests_total` and `oagw_request_duration_seconds` histograms; increment `oagw_errors_total` per error type and `oagw_rate_limit_exceeded_total` per strategy; reflect circuit-breaker state in `oagw_circuit_breaker_state` (`@cpt-cf-oagw-nfr-observability`, `@cpt-cf-oagw-adr-0006-state-management`).

### 3.7 Tests (F7) — Verification-only; DOC-FDESIGN-001: no business algorithms — the coverage gate is specified in the F7 Definition of Done below.

## 4. States

### 4.1 Gear Shell (F1) — States

- [ ] `p1` - **`cpt-cf-oagw-state-gear-shell-uninitialized`** - entered at process start - applies to the gear object - on exit -> `registered` - the gear is not yet wired to the host.
- [ ] `p1` - **`cpt-cf-oagw-state-gear-shell-registered`** - entered when `register_rest` returns - applies to the gear object - routes are live under `/oagw/v1`; configuration and repositories are constructed.

### 4.2 Control Plane (F2) — States

- [ ] `p2` - **`cpt-cf-oagw-state-controller-upstream-created`** - terminal for create - applies to an upstream record - entered when validation passes and the record is persisted; the record is immutable thereafter.
- [ ] `p2` - **`cpt-cf-oagw-state-controller-plugin-created`** - terminal for create - applies to a plugin record - entered when validation passes and the record is persisted; the record is immutable and protected from deletion while referenced.

### 4.3 Proxy Data Plane (F3) — States

- [ ] `p3` - **`cpt-cf-oagw-state-proxy-resolving`** - entered on proxy request arrival - applies to the request - on exit -> `matched` | `failed(gateway)` - the alias walk and route match are in progress.
- [ ] `p3` - **`cpt-cf-oagw-state-proxy-matched`** - entered when a route and target are selected - applies to the request - on exit -> `forwarding` | `rejected(gateway)` - plugin, rate-limit, and CORS checks run here.
- [ ] `p3` - **`cpt-cf-oagw-state-proxy-forwarding`** - entered on outbound dispatch - applies to the request - on exit -> `relayed(upstream)` | `upstream_failed` - headers/body validated before dispatch.
- [ ] `p3` - **`cpt-cf-oagw-state-proxy-relayed`** - terminal for success - applies to the request - the upstream response passes through unmodified with `X-OAGW-Error-Source: upstream`.
- [ ] `p3` - **`cpt-cf-oagw-state-proxy-upstream-unavailable`** - terminal for failure - applies to the request - upstream transport failure; maps to `LinkUnavailable` (503) / `timeout.*` (504) with no retry (`@cpt-cf-oagw-principle-no-retry`).
- [ ] `p3` - **`cpt-cf-oagw-state-dp-circuit-breaker`** - applies to the data plane - closed -> open on repeated upstream failures, half-open on recovery probe - open responses map to `CircuitBreakerOpen` (503) (`@cpt-cf-oagw-fr-error-codes`, `@cpt-cf-oagw-adr-0006-state-management`).
- [ ] `p3` - **`cpt-cf-oagw-state-proxy-stream`** - applies to an SSE session - open -> events -> closed | aborted; upstream close and client disconnect both end the session without retry (`@cpt-cf-oagw-fr-streaming`).

### 4.4 Plugin System (F4) — States

- [ ] `p4` - **`cpt-cf-oagw-state-plugin-chain-await-upstream`** - applies to the chain of a proxied request - entered after Transform(request) completes - on exit -> `complete(response)` | `failed` - guards/transforms of the response phase run after upstream returns.

### 4.5 Rate Limit + CORS (F5) — States

- [ ] `p5` - **`cpt-cf-oagw-state-rate-limit-bucket`** - applies to a scope's token bucket - per-instance; refill continuous at sustained rate, ceiling at burst capacity - empty triggers the configured strategy (`@cpt-cf-oagw-fr-rate-limiting`).

### 4.6 Cross-Cutting (F6) — States

- [ ] `p6` - **`cpt-cf-oagw-state-cross-cut-circuit-breaker`** - shared with F3 `cpt-cf-oagw-state-dp-circuit-breaker` (same object; no duplicate state machine — DOC-FDESIGN-001: F6 owns no additional states beyond the observability wiring of the circuit-breaker state). States for F6 are otherwise not applicable: configuration layering is stateless by design.

### 4.7 Tests (F7) — States

DOC-FDESIGN-001: not applicable — the test suite owns no runtime state machines; the coverage gate is evaluated at build/test time.

## 5. Definitions of Done

### 5.1 Gear Shell (F1) — `cpt-cf-oagw-feature-gear-shell`

#### Implements

- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-module-tree` - The crate exposes a `lib.rs` with the DESIGN.md module tree (`gear.rs`, `config.rs`, `api/rest/{handlers,routes,dto,error,extractors}`, `domain/{services,plugin,dto,repo,error}`, `infra/{proxy,storage,plugin,type_provisioning}`).
- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-registration` - `lib.rs` declares the gear with `#[toolkit::gear(name = "oagw", capabilities = [rest])]` plus a `deps` set naming types-registry, cred-store, tenant-resolver, and authz-resolver; `Gear::init` loads `OagwConfig` via `config_or_default()`; `RestApiCapability::register_rest` returns the host router with management plus proxy routes registered.
- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-deps` - The `oagw` config section is parsed (proxy timeout, http-upstream flag, SSRF policy, token cache settings) with the documented defaults; declared dependencies (types-registry, cred-store, tenant-resolver, authz-resolver) are resolved.
- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-lockfile` - Building under the workspace e2e feature set introduces no change to the workspace lockfile.

#### Constraints

- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-const-deps` - Only dependencies already declared in `oagw/Cargo.toml` are used (`@cpt-cf-oagw-nfr-build-constraints`).
- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-const-host` - No nested router; the gear participates in the host's auth, tracing, and metrics middleware; `prefix_path` is applied by the host (`@cpt-cf-oagw-interface-host-registration`).

#### Touches

- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-touch-lib` - `/app/gears/system/oagw/oagw/src/lib.rs` gains the gear declaration and module wiring.
- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-touch-config` - `oagw` config section decoding is live and covered by unit tests.

#### Covers

- [ ] `p1` - `cpt-cf-oagw-dod-gear-shell-covers-reg` - `cpt-cf-oagw-fr-gear-registration`, `cpt-cf-oagw-interface-host-registration`, `cpt-cf-oagw-adr-gear-architecture`.

### 5.2 Control Plane (F2) — `cpt-cf-oagw-feature-control-plane`

#### Implements

- [ ] `p2` - `cpt-cf-oagw-dod-controller-upstream-crud` - `GET/POST/DELETE /oagw/v1/upstreams` work tenant-scoped with `upstream.v1.schema.json` validation; create returns 201, list respects `$top` (default 50, max 100), delete returns 204; alias conflicts return `UpstreamAliasConflict` (409).
- [ ] `p2` - `cpt-cf-oagw-dod-controller-route-crud` - `GET/POST/DELETE /oagw/v1/routes` are validated against `route.v1.schema.json` (match oneOf http/grpc, methods allowlist, priority); match conflicts return `RouteMatchConflict` (409).
- [ ] `p2` - `cpt-cf-oagw-dod-controller-plugin-crud` - `GET/POST/DELETE /oagw/v1/plugins` manage type-catalog plugin records (auth/guard/transform); records are immutable; in-use delete protection returns `PluginInUse` (409).
- [ ] `p2` - `cpt-cf-oagw-dod-controller-repos` - `UpstreamRepository`, `RouteRepository`, and plugin storage are in-memory, tenant-scoped, and exposed behind domain repository traits (`@cpt-cf-oagw-adr-gear-architecture`).

#### Constraints

- [ ] `p2` - `cpt-cf-oagw-dod-controller-const-tenancy` - Every read/write is scoped to the resolved calling tenant (`@cpt-cf-oagw-nfr-multi-tenancy`).
- [ ] `p2` - `cpt-cf-oagw-dod-controller-const-schema` - Management DTOs reject `additionalProperties` and invalid enum/method/path per the JSON schemas (`@cpt-cf-oagw-nfr-input-validation`).
- [ ] `p2` - `cpt-cf-oagw-dod-controller-const-no-db` - No DB-backed persistence; repositories are in-memory only (DOC-FDESIGN-001: DB-backed repositories are OPTIONAL/future per accepted ADR 0001, not implemented here) (`@cpt-cf-oagw-adr-gear-architecture`).

#### Touches

- [ ] `p2` - `cpt-cf-oagw-dod-controller-touch-api` - `api/rest/handlers/*`, `api/rest/routes/*`, `api/rest/dto/*` host the management surface.
- [ ] `p2` - `cpt-cf-oagw-dod-controller-touch-domain` - `domain/services/*`, `domain/repo/*` implement repository-trait-backed services.

#### Covers

- [ ] `p2` - `cpt-cf-oagw-dod-controller-covers-mgmt` - `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-plugin-lifecycle`, `cpt-cf-oagw-interface-management-api`, `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`, `cpt-cf-oagw-usecase-manage-plugin`.

### 5.3 Proxy Data Plane (F3) — `cpt-cf-oagw-feature-proxy-data-plane`

#### Implements

- [ ] `p3` - `cpt-cf-oagw-dod-proxy-forward` - The proxy handler serves `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` end to end: alias walk, route match, `X-OAGW-Target-Host` matrix, header/host transformation (hop-by-hop strip, `Host`/`:authority` replacement, routing-header consumption), body validation, outbound forwarding with the declared HTTP stack, and unmodified passthrough of upstream responses.
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-error-contract` - Every gateway failure returns `application/problem+json` with a GTS `type` plus `X-OAGW-Error-Source: gateway`; every upstream response carries `X-OAGW-Error-Source: upstream` (`@cpt-cf-oagw-fr-error-source-distinction`).
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-error-table` - The DESIGN.md error table is realized: 400/401/403/404/409/413/429/502/503/504 rows with the documented retriable markers and GTS identifiers.
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-timeouts-bb` - Timeout (connection/request/idle) handling maps to `timeout.*` (504, retriable) with no automatic retry; circuit-breaker state machine opens on repeated failures and maps to `CircuitBreakerOpen` (503) (`@cpt-cf-oagw-fr-error-codes`, `@cpt-cf-oagw-principle-no-retry`).
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-streaming` - SSE (`text/event-stream`) streams with an explicit open/close/error lifecycle; upstream-close and client-disconnect both terminate the session without retry (`@cpt-cf-oagw-fr-streaming`).

#### Constraints

- [ ] `p3` - `cpt-cf-oagw-dod-proxy-const-https` - Production forwards HTTPS-only; plaintext only under the test-only `allow_http_upstream` flag (`@cpt-cf-oagw-nfr-ssrf-protection`).
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-const-body` - Content-Length must match actual size; 100 MB ceiling rejected before buffering; only `chunked` transfer encoding accepted (`@cpt-cf-oagw-fr-body-size`).
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-const-no-cache` - No response caching (DOC-FDESIGN-001: response caching is OPTIONAL/future per PRD out-of-scope, not implemented) (`@cpt-cf-oagw-fr-passthrough`).

#### Touches

- [ ] `p3` - `cpt-cf-oagw-dod-proxy-touch-infra` - `infra/proxy/*` implements the forwarding client, header/body transformations, and SSE relay.
- [ ] `p3` - `cpt-cf-oagw-dod-proxy-touch-domain` - `domain/services/proxy_service*` orchestrates resolution -> match -> chain -> forward.

#### Covers

- [ ] `p3` - `cpt-cf-oagw-dod-proxy-covers-proxy` - `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-route-matching`, `cpt-cf-oagw-fr-target-host`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-passthrough`, `cpt-cf-oagw-fr-body-size`, `cpt-cf-oagw-fr-streaming`, `cpt-cf-oagw-fr-error-codes`, `cpt-cf-oagw-fr-error-source-distinction`, `cpt-cf-oagw-interface-proxy-api`, `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-sse-streaming`, `cpt-cf-oagw-adr-proxy-data-plane`.

### 5.4 Plugin System (F4) — `cpt-cf-oagw-feature-plugin-system`

#### Implements

- [ ] `p4` - `cpt-cf-oagw-dod-plugin-traits` - `AuthPlugin`, `GuardPlugin`, `TransformPlugin` traits plus per-type registries (`AuthPluginRegistry`, `GuardPluginRegistry`, `TransformPluginRegistry`) per accepted ADR 0002.
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-builtins` - Built-ins registered: auth `noop`, `apikey`, `oauth2_client_cred` (Form), `oauth2_client_cred_basic` (Basic); guard `required_headers`; transform `request_id` (`@cpt-cf-oagw-fr-builtin-plugins`).
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-exec-order` - Deterministic execution Auth -> Guards(request) -> Transform(request) -> upstream -> Transform(response/error); upstream plugins before route plugins in each phase.
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-oauth2-cache` - OAuth2 token cache: `(tenant, subject, auth_method, config_hash)` key isolation, `CachedToken` key verification, TTL `min(config_ttl, expires_in - 30s)`, failed fetches never cached (`@cpt-cf-oagw-fr-oauth2-token-cache`, `@cpt-cf-oagw-nfr-credential-isolation`).
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-guard-fail-open` - `required_headers` is presence-only, case-insensitive, request phase 400 / response phase 502, first missing header reported, fail-open when unconfigured (`@cpt-cf-oagw-fr-required-headers-guard`).
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-catalog-only` - Catalog-only identifiers (`basic`/`bearer` auth, `timeout`/`cors` guard, `logging`/`metrics` transform) are not registered; referencing one fails resolution with `PluginNotFound` (503) (`@cpt-cf-oagw-fr-builtin-plugins`).
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-cred-store` - Auth plugins inject credentials resolved via `cred_store` references; secrets are never embedded in plugin records (`@cpt-cf-oagw-fr-auth-injection`, `@cpt-cf-oagw-nfr-credential-isolation`).

#### Constraints

- [ ] `p4` - `cpt-cf-oagw-dod-plugin-const-no-sandbox` - No Starlark runtime; tenant-defined source is not executed in the MVP (DOC-FDESIGN-001: deferred per accepted ADR 0002 and the Starlark NFR) (`@cpt-cf-oagw-nfr-starlark-sandbox`).
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-const-no-dep` - Only `pingora-memory-cache` (declared) backs the token cache; no new dependencies (`@cpt-cf-oagw-nfr-build-constraints`).

#### Touches

- [ ] `p4` - `cpt-cf-oagw-dod-plugin-touch-domain` - `domain/plugin/*` hosts traits, registries, and built-in implementations.
- [ ] `p4` - `cpt-cf-oagw-dod-plugin-touch-infra` - `infra/plugin/*` backends memory-cache-backed OAuth2 token storage.

#### Covers

- [ ] `p4` - `cpt-cf-oagw-dod-plugin-covers-plugin` - `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-fr-plugin-lifecycle`, `cpt-cf-oagw-fr-plugin-source`, `cpt-cf-oagw-fr-required-headers-guard`, `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-oauth2-token-cache`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-adr-plugins-rate-limit-cors`.

### 5.5 Rate Limit + CORS (F5) — `cpt-cf-oagw-feature-rate-limit-cors`

#### Implements

- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-bucket` - Per-instance dual-rate token bucket (sustained rate + burst capacity) for scopes `global|tenant|user|ip|route` with per-request cost; hierarchical `min(parent_effective, child)` merge (`@cpt-cf-oagw-fr-rate-limiting`, `@cpt-cf-oagw-fr-hierarchical-config`).
- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-strategies` - `reject` -> 429 with `X-RateLimit-*` and `Retry-After`; `queue` -> bounded wait; `degrade` -> proceed without enforcement (`@cpt-cf-oagw-usecase-rate-limit-exceeded`).
- [ ] `p5` - `cpt-cf-oagw-dod-cors-preflight` - Local preflight 204 fast path (no upstream resolution, no per-request auth) with permissive headers and always `Vary: Origin` (`@cpt-cf-oagw-usecase-cors-preflight`).
- [ ] `p5` - `cpt-cf-oagw-dod-cors-actual` - Actual-request enforcement after upstream resolution: exact (port/protocol-sensitive) origin matching; disallowed origin -> 403 `OriginNotAllowed`; disallowed method -> 403 `MethodNotAllowed`; sharing-mode merge (union on `inherit`, no additions on `enforce`); `allow_credentials` with wildcard origin rejected at configuration time (`@cpt-cf-oagw-fr-cors`).

#### Constraints

- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-const-local` - Enforcement is data-plane-owned and per-instance; no distributed synchronization (DOC-FDESIGN-001: Redis-backed sync is OPTIONAL/future per accepted ADR 0003, not implemented) (`@cpt-cf-oagw-adr-plugins-rate-limit-cors`).
- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-const-no-dep` - Token buckets are in-memory (`dashmap`/`parking_lot` only); no new dependencies (`@cpt-cf-oagw-nfr-build-constraints`, `@cpt-cf-oagw-nfr-low-latency`).

#### Touches

- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-touch-infra` - `infra/` bucket store and CORS evaluator wired into the data-plane pipeline.
- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-touch-domain` - Rate-limit and CORS logic lives in domain services under the merge rules.

#### Covers

- [ ] `p5` - `cpt-cf-oagw-dod-rate-limit-covers-rc` - `cpt-cf-oagw-fr-rate-limiting`, `cpt-cf-oagw-fr-cors`, `cpt-cf-oagw-fr-hierarchical-config`, `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-nfr-low-latency`, `cpt-cf-oagw-usecase-rate-limit-exceeded`, `cpt-cf-oagw-usecase-cors-preflight`, `cpt-cf-oagw-adr-plugins-rate-limit-cors`.

### 5.6 Cross-Cutting (F6) — `cpt-cf-oagw-feature-cross-cutting`

#### Implements

- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-observability` - Host-standard tracing (request ID propagation, tenant/principal extraction) and the DESIGN.md metrics vocabulary (`oagw_requests_total`, `oagw_request_duration_seconds`, `oagw_errors_total`, `oagw_circuit_breaker_state`, `oagw_rate_limit_exceeded_total`) are emitted with the agreed log field set (`@cpt-cf-oagw-nfr-observability`).
- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-config-layering` - Merged plugin/rate/CORS configuration per Upstream < Route < Tenant with child-over-parent and enforced ancestor caps (`@cpt-cf-oagw-fr-config-layering`).
- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-error-type-reg` - GTS identifiers used by gateway errors are registered with types-registry at init (`@cpt-cf-oagw-fr-error-codes`).

#### Constraints

- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-const-alignment` - Metrics and log fields match the DESIGN.md vocabulary exactly (no new metric names), so host dashboards remain aligned (`@cpt-cf-oagw-nfr-observability`).
- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-const-no-secrets` - Logs and error bodies never carry credentials or secrets (DOC-FDESIGN-001: secrets exposure is prohibited; credential material stays behind `cred_store`) (`@cpt-cf-oagw-nfr-credential-isolation`).

#### Touches

- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-touch-gate` - The gear's shared middleware/tracing integration points in `lib.rs`/`gear.rs`.

#### Covers

- [ ] `p6` - `cpt-cf-oagw-dod-cross-cut-covers-cc` - `cpt-cf-oagw-nfr-observability`, `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-hierarchical-config`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-adr-0006-state-management`.

### 5.7 Tests (F7) — `cpt-cf-oagw-feature-tests`

#### Implements

- [ ] `p7` - `cpt-cf-oagw-dod-tests-coverage-gate` - The acceptance gate: unit + integration coverage that exercises every REQUIRED behavior from F1-F6 (alias resolution with shadowing, route matching, target-host matrix, header/body validation, error contract, plugin order, OAuth2 cache, required_headers, rate limit, CORS preflight/actual), plus the SSE lifecycle test, all green under the e2e feature set (`@cpt-cf-oagw-nfr-testability`).
- [ ] `p7` - `cpt-cf-oagw-dod-tests-httpmock` - Integration tests use `httpmock` (dev-dependency) to stand up mock upstreams and verify proxying, passthrough, error-source attribution, and rate-limit/CORS/plugin behavior without external services.
- [ ] `p7` - `cpt-cf-oagw-dod-tests-location` - The gear's own tests live with the crate; `/app/gears/system/oagw/testing/e2e/gears/oagw/` does NOT contain this gear's tests (that location is reserved for the acceptance suite) (`@cpt-cf-oagw-nfr-testability`).
- [ ] `p7` - `cpt-cf-oagw-dod-tests-no-external` - No test depends on a live identity provider, Redis, or a database; all external interactions are mocked/in-process (`@cpt-cf-oagw-nfr-build-constraints`).

#### Constraints

- [ ] `p7` - `cpt-cf-oagw-dod-tests-const-lockfile` - Test additions are limited to declared dev-dependencies (`httpmock` already declared); no lockfile change (`@cpt-cf-oagw-nfr-build-constraints`).
- [ ] `p7` - `cpt-cf-oagw-dod-tests-const-deterministic` - Integration tests are deterministic (no sleeps, no wall-clock flakiness); rate-limit tests control time/refill explicitly.

#### Touches

- [ ] `p7` - `cpt-cf-oagw-dod-tests-touch-crate` - Unit tests in crate modules; integration tests under `oagw/tests/`.

#### Covers

- [ ] `p7` - `cpt-cf-oagw-dod-tests-covers-tests` - `cpt-cf-oagw-nfr-testability`, `cpt-cf-oagw-nfr-build-constraints`, and the confirmation criteria of accepted ADRs 0001/0002/0003 and docs ADRs 0001/0002/0003/0004/0008/0009.

## 6. Acceptance Criteria

### 6.1 Gear Shell (F1)

- [ ] `p1` - `cpt-cf-oagw-ac-gear-shell-init` - Given a config without an `oagw` section, the gear initializes with `Default` values and the management and proxy routes are reachable under `/oagw/v1/*` (`@cpt-cf-oagw-fr-gear-registration`).
- [ ] `p1` - `cpt-cf-oagw-ac-gear-shell-build` - The workspace e2e build succeeds with `cf-gears-oagw` included and no lockfile addition (`@cpt-cf-oagw-nfr-build-constraints`).

### 6.2 Control Plane (F2)

- [ ] `p2` - `cpt-cf-oagw-ac-controller-upstream` - Creating an upstream with a duplicate alias returns 409 `UpstreamAliasConflict`; a valid create returns 201 and the record lists back (`@cpt-cf-oagw-fr-upstream-mgmt`).
- [ ] `p2` - `cpt-cf-oagw-ac-controller-route` - Creating a route that conflicts with an existing one returns 409 `RouteMatchConflict`; a valid route returns 201 (`@cpt-cf-oagw-fr-route-mgmt`).
- [ ] `p2` - `cpt-cf-oagw-ac-controller-plugin` - Deleting a plugin record referenced by a route returns 409 `PluginInUse` and the record remains (`@cpt-cf-oagw-fr-plugin-lifecycle`).
- [ ] `p2` - `cpt-cf-oagw-ac-controller-tenancy` - Tenant A cannot see or mutate tenant B's upstreams/routes/plugins (`@cpt-cf-oagw-nfr-multi-tenancy`).

### 6.3 Proxy Data Plane (F3)

- [ ] `p3` - `cpt-cf-oagw-ac-proxy-alias` - A request to `/oagw/v1/proxy/{alias}` resolves the closest ancestor-bound alias (shadowing verified: descendant wins, enforced ancestors never bypassed) and reaches the right mock upstream (`@cpt-cf-oagw-fr-alias-resolution`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-route` - Longest-path-prefix (then priority) matching selects the right route; an unmatched path returns 404 `RouteNotFound` (`@cpt-cf-oagw-fr-route-matching`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-target-host` - Missing/invalid/unknown `X-OAGW-Target-Host` are handled per the matrix (round-robin when absent; 400 on invalid/unknown) (`@cpt-cf-oagw-fr-target-host`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-headers` - Hop-by-hop headers are stripped, `Host`/`:authority` replaced with the upstream host, and routing headers never reach the upstream (`@cpt-cf-oagw-fr-header-transform`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-body` - A mismatched `Content-Length` returns 400, a payload over the 100 MB ceiling returns 413 before buffering, and a rejected transfer encoding returns 400 (`@cpt-cf-oagw-fr-body-size`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-error-source` - Gateway errors return `application/problem+json` with a GTS `type` and `X-OAGW-Error-Source: gateway`; upstream errors pass through with `X-OAGW-Error-Source: upstream` (`@cpt-cf-oagw-fr-error-source-distinction`).
- [ ] `p3` - `cpt-cf-oagw-ac-proxy-timeout` - A timing-out upstream yields a retriable `timeout.*` 504 with no automatic retry; the circuit breaker maps repeated failures to 503 `CircuitBreakerOpen` (`@cpt-cf-oagw-fr-error-codes`).

### 6.4 Plugin System (F4)

- [ ] `p4` - `cpt-cf-oagw-ac-plugin-order` - A proxied request configured with auth, guard, and transform plugins shows evidence of Auth -> Guard -> Transform(request) before the upstream call and Transform(response) after it (`@cpt-cf-oagw-fr-plugin-system`).
- [ ] `p4` - `cpt-cf-oagw-ac-plugin-oauth2` - Two tenants with different OAuth2 configs get distinct cached tokens; a failed identity-provider fetch is not cached and yields 401 `AuthenticationFailed` (`@cpt-cf-oagw-fr-oauth2-token-cache`).
- [ ] `p4` - `cpt-cf-oagw-ac-plugin-required-headers` - A missing required request header yields 400 and a missing required response header yields 502; with no configuration the guard is fail-open (`@cpt-cf-oagw-fr-required-headers-guard`).
- [ ] `p4` - `cpt-cf-oagw-ac-plugin-catalog-only` - Referencing a catalog-only identifier (e.g. `bearer`) fails resolution with 503 `PluginNotFound` (`@cpt-cf-oagw-fr-builtin-plugins`).

### 6.5 Rate Limit + CORS (F5)

- [ ] `p5` - `cpt-cf-oagw-ac-rate-limit-429` - Exceeding the bucket under `reject` returns 429 with `X-RateLimit-*` and `Retry-After` headers; `queue` waits then proceeds and `degrade` proceeds (`@cpt-cf-oagw-fr-rate-limiting`).
- [ ] `p5` - `cpt-cf-oagw-ac-rate-limit-hierarchy` - An enforced ancestor rate cap is honored by a child route that would otherwise allow more (`min`-merge) (`@cpt-cf-oagw-fr-hierarchical-config`).
- [ ] `p5` - `cpt-cf-oagw-ac-cors-preflight` - A preflight `OPTIONS` with `Origin` + `Access-Control-Request-Method` receives a local 204 with allowed headers and `Vary: Origin`, with no upstream call (`@cpt-cf-oagw-usecase-cors-preflight`).
- [ ] `p5` - `cpt-cf-oagw-ac-cors-actual` - A cross-origin actual request with a disallowed origin or method returns 403 (`OriginNotAllowed`/`MethodNotAllowed`); configuration with `allow_credentials` + wildcard origin is rejected at configuration time (`@cpt-cf-oagw-fr-cors`).

### 6.6 Cross-Cutting (F6)

- [ ] `p6` - `cpt-cf-oagw-ac-cross-cut-metrics` - A proxied request increments `oagw_requests_total` and records duration; a failed request increments `oagw_errors_total` with the right error type dimension (`@cpt-cf-oagw-nfr-observability`).
- [ ] `p6` - `cpt-cf-oagw-ac-cross-cut-config` - A route inheriting tenant-level config reflects the merged (child-over-parent) plugin/rate/CORS settings with enforced caps applied (`@cpt-cf-oagw-fr-config-layering`).

### 6.7 Tests (F7)

- [ ] `p7` - `cpt-cf-oagw-ac-tests-gate-green` - The full crate test suite (unit + httpmock integration, including the SSE lifecycle test) passes under the e2e feature set in CI, covering all acceptance criteria above (`@cpt-cf-oagw-nfr-testability`).
- [ ] `p7` - `cpt-cf-oagw-ac-tests-reservation` - No test of this gear is placed under `/app/gears/system/oagw/testing/e2e/gears/oagw/` (reserved for the acceptance suite) (`@cpt-cf-oagw-nfr-testability`).

## 7. Scope

### 7.1 In Scope (REQUIRED — MVP)

- [ ] `p1` - `cpt-cf-oagw-scope-registration` - Gear registration, config loading, module tree, host-router registration (F1).
- [ ] `p2` - `cpt-cf-oagw-scope-cp-crud` - Tenant-scoped in-memory upstream/route/plugin management (F2).
- [ ] `p3` - `cpt-cf-oagw-scope-dp-proxy` - Request-driven proxy: alias resolution, route matching, target-host, header/body validation, error contract, timeouts/circuit breaker, SSE (F3).
- [ ] `p4` - `cpt-cf-oagw-scope-plugins` - Three plugin traits + registries + built-ins (auth noop/apikey/oauth2 form+basic, guard required_headers, transform request_id), catalog-only identifiers unbound (F4).
- [ ] `p5` - `cpt-cf-oagw-scope-rate-cors` - Per-instance dual-rate token buckets with strategies and hierarchical merge; built-in CORS preflight fast path and actual-request enforcement (F5).
- [ ] `p6` - `cpt-cf-oagw-scope-cc` - Observability (metrics/log/tracing), config layering, error-type registration (F6).
- [ ] `p7` - `cpt-cf-oagw-scope-tests` - Unit + httpmock integration tests and the aggregate coverage gate (F7).

### 7.2 Out of Scope (OPTIONAL / future per accepted docs and pipeline ADRs)

- [ ] `p7` - `cpt-cf-oagw-scope-future-redis-rate-limit` - Redis-backed distributed/global rate limiting (accepted ADR 0003 OPTIONAL/future; per-instance enforcement is the MVP REQUIRED behavior).
- [ ] `p7` - `cpt-cf-oagw-scope-future-starlark` - Starlark sandbox execution of tenant-defined custom plugins (accepted ADR 0002 keeps custom plugins catalog-level; execution violates the no-new-dependencies constraint; `@cpt-cf-oagw-nfr-starlark-sandbox`).
- [ ] `p7` - `cpt-cf-oagw-scope-future-db` - DB-backed control-plane repositories and migrations (accepted ADR 0001; no scaffold exists, deferred behind repository traits).
- [ ] `p7` - `cpt-cf-oagw-scope-future-websocket` - WebSocket/WebTransport proxying (SSE only in the MVP per `@cpt-cf-oagw-fr-streaming`).
- [ ] `p7` - `cpt-cf-oagw-scope-future-caches-retry` - Response caching and automatic retries (PRD out-of-scope; `@cpt-cf-oagw-principle-no-retry`, `@cpt-cf-oagw-fr-passthrough`).

## 8. Test Scenarios

### 8.1 Unit Test Scenarios

- [ ] `p1` - `cpt-cf-oagw-test-gear-shell-config` - `OagwConfig` defaulting (missing section, partial section, explicit overrides) and the registration of management + proxy route groups.
- [ ] `p2` - `cpt-cf-oagw-test-cp-alias-derive` - Derived-alias computation with PSL common-suffix pools; hostname/IP rule rejection returns 400 `ValidationError`.
- [ ] `p2` - `cpt-cf-oagw-test-cp-conflict` - Route-conflict detection (method intersection + prefix tie under priority) and plugin in-use protection.
- [ ] `p3` - `cpt-cf-oagw-test-dp-walk` - Alias chain walk, closest-match selection, enforced-ancestor never-bypassed rule, and longest-path-prefix/priority route scoring.
- [ ] `p3` - `cpt-cf-oagw-test-dp-headers-body` - Hop-by-hop strip list, host replacement, routing-header consumption, Content-Length/match/ceiling/transfer-encoding validation branches.
- [ ] `p3` - `cpt-cf-oagw-test-dp-target-host` - Target-host matrix: missing -> round-robin, invalid format -> 400, unknown member -> 400.
- [ ] `p3` - `cpt-cf-oagw-test-dp-timeouts` - Timeout mapping to retriable 504 and circuit-breaker open -> 503 (deterministic, injected timers).
- [ ] `p3` - `cpt-cf-oagw-test-dp-sse` - SSE relay open/close/error lifecycle with upstream-close and client-disconnect termination (unit-level with a scripted stream).
- [ ] `p4` - `cpt-cf-oagw-test-plugin-chain` - Chain composition order (upstream before route within phase) and short-circuit on auth failure / guard rejection / transform error.
- [ ] `p4` - `cpt-cf-oagw-test-plugin-oauth2-cache` - Cache key isolation per `(tenant, subject, auth_method, config_hash)`, TTL = `min(config_ttl, expires_in - 30s)`, failed fetches not cached, `CachedToken` verification on collision.
- [ ] `p4` - `cpt-cf-oagw-test-plugin-guard` - `required_headers` presence-only/case-insensitive matching, first-missing-header reporting, fail-open unconfigured, request 400 / response 502.
- [ ] `p4` - `cpt-cf-oagw-test-plugin-catalog-only` - Catalog-only identifiers are not registered and fail resolution with 503.
- [ ] `p5` - `cpt-cf-oagw-test-rate-bucket` - Refill/ceiling arithmetic, cost accounting, empty-bucket branch for reject/queue/degrade, hierarchical `min` merge.
- [ ] `p5` - `cpt-cf-oagw-test-cors` - Preflight detection (OPTIONS+Origin+ACRM), exact origin matching (port/protocol), sharing-mode union/enforce, credentials+wildcard config rejection.
- [ ] `p6` - `cpt-cf-oagw-test-cc-metrics` - Metric increments and dimensions for success/error/rate-limit paths; log field set completeness; GTS error-type registration.
- [ ] `p6` - `cpt-cf-oagw-test-cc-config-merge` - Upstream < Route < Tenant merge order with child-wins and enforced caps.

### 8.2 Integration Test Scenarios (httpmock)

- [ ] `p3` - `cpt-cf-oagw-it-proxy-basic` - Proxy a GET through a mock upstream: upstream receives the request with replaced `Host`, stripped hop-by-hop and routing headers; response passes through unmodified with `X-OAGW-Error-Source: upstream`.
- [ ] `p3` - `cpt-cf-oagw-it-proxy-errors` - Unmatched route -> 404; oversized/mismatched body -> 413/400; unreachable upstream -> 503 `LinkUnavailable`; timing-out upstream -> 504; all with problem+json and `X-OAGW-Error-Source: gateway`.
- [ ] `p3` - `cpt-cf-oagw-it-proxy-sse` - SSE e2e: events forwarded, upstream close and client disconnect end the session cleanly.
- [ ] `p3` - `cpt-cf-oagw-it-proxy-target-host` - Round-robin across a two-member pool when `X-OAGW-Target-Host` absent; honored when valid; 400 when invalid/unknown.
- [ ] `p4` - `cpt-cf-oagw-it-plugin-order` - A route with auth + guard + transform plugins proves the documented execution order and response-phase transform against mock upstreams.
- [ ] `p4` - `cpt-cf-oagw-it-plugin-oauth2` - Mock OAuth2 token endpoint: cache hit served without a second fetch, per-tenant isolation, failed fetch not cached.
- [ ] `p4` - `cpt-cf-oagw-it-plugin-guard` - Missing required request header -> 400; missing required response header -> 502 (mock upstream omits it).
- [ ] `p5` - `cpt-cf-oagw-it-rate-limit` - Burst exceeded under `reject` -> 429 with `X-RateLimit-*` + `Retry-After`; `queue` then proceed; `degrade` then proceed; ancestor cap enforced over child.
- [ ] `p5` - `cpt-cf-oagw-it-cors` - Preflight -> 204 with no upstream call (mock upstream records zero hits); disallowed origin/method on the actual request -> 403.
- [ ] `p2` - `cpt-cf-oagw-it-cp-tenancy` - Two tenants: CRUD on upstreams/routes/plugins is isolated; cross-tenant conflicts never surface.

### 8.3 Reserved Location

- [ ] `p7` - `cpt-cf-oagw-test-reserved-e2e` - `/app/gears/system/oagw/testing/e2e/gears/oagw/` is reserved for the acceptance suite; development and integration tests live under the crate (`unit` in modules, `integration` in `oagw/tests/` with `httpmock`) (`@cpt-cf-oagw-nfr-testability`).

## 9. Non-Functional Requirements

- [ ] `p1` - `cpt-cf-oagw-nfr-doc-no-ops` - No operational procedures, deployment steps, or secrets are specified in this document (DOC-FDESIGN-001: coverage via reference to `@cpt-cf-oagw-nfr-observability` and the host's runtime docs).
- [ ] `p1` - `cpt-cf-oagw-nfr-doc-lockfile` - The implementation introduces no dependency beyond the workspace lockfile (DOC-FDESIGN-001: enforced by F1 `cpt-cf-oagw-dod-gear-shell-lockfile` and F7 `cpt-cf-oagw-dod-tests-const-lockfile`) (`@cpt-cf-oagw-nfr-build-constraints`).

## 10. Traceability

This FEATURE elaborates the DECOMPOSITION work items `cpt-cf-oagw-feature-gear-shell` (F1), `cpt-cf-oagw-feature-control-plane` (F2), `cpt-cf-oagw-feature-proxy-data-plane` (F3), `cpt-cf-oagw-feature-plugin-system` (F4), `cpt-cf-oagw-feature-rate-limit-cors` (F5), `cpt-cf-oagw-feature-cross-cutting` (F6), and `cpt-cf-oagw-feature-tests` (F7), and traces to the following pipeline requirement/design ids (full requirement ids appear inline throughout this document; representative set):

- `cpt-cf-oagw-fr-gear-registration`, `cpt-cf-oagw-fr-upstream-mgmt`, `cpt-cf-oagw-fr-route-mgmt`, `cpt-cf-oagw-fr-plugin-lifecycle`, `cpt-cf-oagw-fr-plugin-source`, `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-fr-required-headers-guard`, `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-oauth2-token-cache`, `cpt-cf-oagw-fr-rate-limiting`, `cpt-cf-oagw-fr-cors`, `cpt-cf-oagw-fr-hierarchical-config`, `cpt-cf-oagw-fr-config-layering`, `cpt-cf-oagw-fr-request-proxy`, `cpt-cf-oagw-fr-alias-resolution`, `cpt-cf-oagw-fr-route-matching`, `cpt-cf-oagw-fr-target-host`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-fr-passthrough`, `cpt-cf-oagw-fr-body-size`, `cpt-cf-oagw-fr-streaming`, `cpt-cf-oagw-fr-error-codes`, `cpt-cf-oagw-fr-error-source-distinction`.
- `cpt-cf-oagw-nfr-build-constraints`, `cpt-cf-oagw-nfr-low-latency`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-nfr-observability`, `cpt-cf-oagw-nfr-testability`, `cpt-cf-oagw-nfr-multi-tenancy`, `cpt-cf-oagw-nfr-input-validation`, `cpt-cf-oagw-nfr-ssrf-protection`, `cpt-cf-oagw-nfr-starlark-sandbox`.
- `cpt-cf-oagw-interface-host-registration`, `cpt-cf-oagw-interface-management-api`, `cpt-cf-oagw-interface-proxy-api`.
- `cpt-cf-oagw-usecase-configure-upstream`, `cpt-cf-oagw-usecase-configure-route`, `cpt-cf-oagw-usecase-manage-plugin`, `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-sse-streaming`, `cpt-cf-oagw-usecase-rate-limit-exceeded`, `cpt-cf-oagw-usecase-cors-preflight`.
- `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`, `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-upstream-service`, `cpt-cf-oagw-actor-cred-store`, `cpt-cf-oagw-actor-types-registry`, `cpt-cf-oagw-actor-tenant-resolver`.
- `cpt-cf-oagw-adr-gear-architecture`, `cpt-cf-oagw-adr-proxy-data-plane`, `cpt-cf-oagw-adr-plugins-rate-limit-cors` (in-repo pipeline ADRs); accepted docs ADRs represented as `@cpt-cf-oagw-adr-0001-alias-shadowing`, `@cpt-cf-oagw-adr-0002-plugin-exec-order`, `@cpt-cf-oagw-adr-0003-rate-limit`, `@cpt-cf-oagw-adr-0004-cors-fast-path`, `@cpt-cf-oagw-adr-0006-state-management`, `@cpt-cf-oagw-adr-0007-error-source`, `@cpt-cf-oagw-adr-0008-oauth2-cache`, `@cpt-cf-oagw-adr-0009-required-headers`, `@cpt-cf-oagw-principle-no-retry`.
