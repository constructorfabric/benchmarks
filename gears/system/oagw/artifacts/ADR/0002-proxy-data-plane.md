---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# ADR-0002: In-Crate Axum Proxy Data Plane with Gateway Error Semantics


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [In-crate axum proxy handler under the management router](#in-crate-axum-proxy-handler-under-the-management-router)
  - [pingora-proxy service wrapper as the request path](#pingora-proxy-service-wrapper-as-the-request-path)
  - [External forward-proxy delegation](#external-forward-proxy-delegation)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-proxy-data-plane`
## Context and Problem Statement

The proxy capability (`{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`) must be implemented in-repo: resolve the upstream by alias (tenant chain walk with shadowing), match a route by (upstream, method, longest path prefix, priority), apply `X-OAGW-Target-Host` selection semantics, transform and validate headers and body, and produce RFC 9457 `application/problem+json` errors with the `X-OAGW-Error-Source` header on every response. The question is which request-path technology to use when the crate already declares both the axum/hyper/hyper-util stack and the pingora-* proxy engine dependencies.

## Decision Drivers

* The gear is a plain REST gear on the `api-gateway` (axum) surface — the proxy path must integrate with the axum host router and middleware pipeline
* Low latency on the hot path (`cpt-cf-oagw-nfr-low-latency`) and no automatic retries (design principle `cpt-cf-oagw-principle-no-retry`)
* No new dependencies: use only the HTTP stack already declared in `oagw/Cargo.toml`
* Correct, consistent error semantics mandated by accepted ADR 0007 (error-source header) and DESIGN.md (RFC 9457 error table)
* Streaming support for SSE with proper connection lifecycle per `cpt-cf-oagw-fr-streaming`

## Considered Options

1. **In-crate axum proxy handler under the management router** — the data-plane service is invoked directly from an axum handler on the host router and performs outbound forwarding with the declared HTTP stack (hyper/hyper-util/toolkit-http)
2. **pingora-proxy service wrapper as the request path** — a `pingora_proxy::ProxyHttp` service handles the proxying on behalf of the handler
3. **External forward-proxy delegation** — delegate outbound forwarding to a separate forward proxy process/service

## Decision Outcome

Chosen option: "In-crate axum proxy handler under the management router", because it integrates directly with the axum host surface, keeps error attribution under the gear's control, and uses dependencies already declared in `oagw/Cargo.toml` — no additional dependency or integration surface.

The proxy handler (`api/rest/handlers/`) calls the data-plane service which:

* Resolves the upstream by alias with a tenant chain walk (descendant to root, closest match wins, enforced ancestors never bypassed by shadowing) per accepted ADR 0001 and `cpt-cf-oagw-fr-alias-resolution`
* Matches the route by `(upstream, method, longest path prefix, priority)` per accepted ADR 0001 and `cpt-cf-oagw-fr-route-matching`; no matching route yields the gateway "route not found" condition
* Applies `X-OAGW-Target-Host` semantics per the accepted ADR 0001 behavior matrix: required for common-suffix multi-endpoint pools, validated when present (format and membership), round-robin selection when absent
* Applies header rules per accepted ADR 0001/DESIGN.md: consuming routing headers without forwarding, stripping hop-by-hop headers, replacing the request `Host` with the upstream host (and `:authority` on HTTP/2), plus passthrough/transform rules from upstream/route header configuration
* Validates the body per DESIGN.md: `Content-Length` must be a valid integer matching the actual size, a hard 100 MB ceiling rejected before buffering, only `chunked` transfer encoding accepted
* Forwards the request outbound using the declared hyper/hyper-util/toolkit-http stack (HTTPS-only in production; plaintext only under the test-only `allow_http_upstream` flag)

All gateway-originated failures are mapped through `toolkit-canonical-errors` (a `#[resource_error(gts_id!(...))]` resource error type plus `From<DomainError> for CanonicalError`, following the `types-registry` rest error pattern) to RFC 9457 `application/problem+json` with GTS type identifiers. Every response carries `X-OAGW-Error-Source: gateway|upstream` per accepted ADR 0007 — gateway errors carry `gateway` with a problem+json body; upstream responses (including errors) pass through unmodified and carry `upstream`. Streaming response handlers forward SSE with explicit open/close/error lifecycle; WebSocket/WebTransport are considered future work unless trivially supported by the same handler.

Alternatives rejected:

* **pingora-proxy service wrapper**: rejected for the MVP because hosting a `ProxyHttp` service alongside the axum handler is a heavier integration surface (its own event loops and error mapping) that does not yet earn its keep; the pingora-* dependencies stay declared in the crate and remain available for a later data-plane maturation.
* **External forward-proxy delegation**: rejected because it is out of contract — the gear is a single-executable host component (`cpt-cf-oagw-constraint-toolkit-deploy`), owns its error semantics, and a separate forward proxy would add a process boundary with no requirement supporting it.

### Consequences

* Good, because the proxy path is request-driven end to end with no extra process or background task — low latency and simple lifecycle
* Good, because gateway/upstream error attribution and RFC 9457 formatting are fully under the gear's control, satisfying accepted ADR 0007 on every response
* Good, because no new dependencies are introduced and the handler participates in the host's auth, tracing, and metrics middleware
* Bad, because the in-crate path is initially simpler and less tuned than a dedicated Pingora service (connection pooling and adaptive HTTP/2 detection are future work)
* Bad, because SSE streaming and hop-by-hop handling must be implemented explicitly by the handler rather than delegated to a proxy engine
* Neutral, because pingora deps remain declared, enabling a later in-crate migration without dependency changes

### Confirmation

Confirmed when:

* Integration tests verify alias resolution with shadowing, the `X-OAGW-Target-Host` behavior matrix (missing/invalid/unknown/round-robin cases), longest-path-prefix route matching, hop-by-hop stripping and host replacement, and body-validation rejection paths
* Tests verify gateway errors return `application/problem+json` with GTS `type` plus `X-OAGW-Error-Source: gateway`, and upstream errors pass through with `X-OAGW-Error-Source: upstream`
* An SSE e2e test verifies event forwarding and upstream-close/client-disconnect lifecycle handling

## Pros and Cons of the Options

### In-crate axum proxy handler under the management router

The data-plane service runs directly behind an axum handler registered on the host router, forwarding with the declared hyper/hyper-util/toolkit-http stack.

* Good, because tight integration with the axum host — shared middleware (auth, request ID, metrics) and error mapping
* Good, because uses only dependencies already in the crate's lockfile
* Good, because error semantics and header/body handling are explicit and testable
* Bad, because some proxy-engine conveniences (pooling, adaptation) must be built or deferred

### pingora-proxy service wrapper as the request path

A `pingora_proxy::ProxyHttp` component performs the proxying for the data-plane service.

* Good, because mature connection pooling, load balancing, and HTTP adaptation come out of the box
* Good, because the pingora-* deps are already declared in the crate
* Bad, because it adds a heavier integration surface with the axum host (separate event loops and error mapping)
* Bad, because folding Pingora semantics into the required RFC 9457/error-source and header-rule behavior is more complex for the MVP

### External forward-proxy delegation

Forward outbound calls through a separately deployed forward proxy.

* Good, because it would offload connection handling entirely
* Bad, because it contradicts the single-executable deployment constraint and adds an unrequired process boundary
* Bad, because error attribution and body/header semantics would no longer be under the gear's control
* Bad, because it is out of the accepted contract (the gear is the proxy)

## More Information

These in-repo pipeline ADRs complement — and do not supersede — the gear's accepted behavioral records in `docs/ADR/`:

- [ADR 0001: Request Routing](../../docs/ADR/0001-request-routing.md) — authoritative path-based routing, `X-OAGW-Target-Host` behavior matrix, and head-transformation rules this decision implements
- [ADR 0007: Error Source Distinction](../../docs/ADR/0007-error-source-distinction.md) — authoritative `X-OAGW-Error-Source: gateway|upstream` contract this decision satisfies
- [DESIGN.md](../../docs/DESIGN.md) — body-validation rules, header categories, error table, and proxy sequence this decision follows
- Platform worked example: `/app/gears/system/types-registry/types-registry/src/api/rest/error.rs` — `#[resource_error(gts_id!(...))]` and `From<DomainError> for CanonicalError` mapping pattern

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-request-proxy` — Unified proxy capability with upstream resolution, route matching, config merge, plugin chain, and forwarding
* `cpt-cf-oagw-fr-alias-resolution` — Tenant chain walk from descendant to root with shadowing and enforced ancestor limits
* `cpt-cf-oagw-fr-route-matching` — Match by method allowlist and longest path prefix with priority; query-allowlist validation; route-not-found condition
* `cpt-cf-oagw-fr-target-host` — `X-OAGW-Target-Host` validation and round-robin pool selection
* `cpt-cf-oagw-fr-header-transform` — Hop-by-hop stripping, routing-header consumption, host replacement, and passthrough control
* `cpt-cf-oagw-fr-passthrough` — Upstream responses relayed unmodified; no response caching
* `cpt-cf-oagw-fr-body-size` — Content-Length/match validation, 100 MB ceiling before buffering, transfer-encoding rejection
* `cpt-cf-oagw-fr-streaming` — SSE streaming with connection lifecycle handling; WebSocket/WebTransport future work
* `cpt-cf-oagw-fr-error-codes` — Consistent error conditions with explicit retriable semantics and resource identification
* `cpt-cf-oagw-fr-error-source-distinction` — `X-OAGW-Error-Source` on every response; RFC 9457 problem+json for gateway errors
* `cpt-cf-oagw-nfr-low-latency` — Request-driven path with no added process hop
* `cpt-cf-oagw-nfr-ssrf-protection` — Path/query validation against route configuration and HTTPS-only default
* `cpt-cf-oagw-nfr-input-validation` — Body/header validation before any forwarding
* `cpt-cf-oagw-nfr-observability` — Proxy path participates in host tracing and error-source attribution
* `cpt-cf-oagw-interface-proxy-api` — Proxy endpoint implemented under the `/oagw/v1` base path
* `cpt-cf-oagw-usecase-proxy-request` — Main proxy flow and its alternative flows supported
* `cpt-cf-oagw-usecase-sse-streaming` — SSE event forwarding with lifecycle handling
