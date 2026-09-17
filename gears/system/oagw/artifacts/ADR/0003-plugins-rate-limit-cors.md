---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# ADR-0003: Built-in Plugin System with DP-Owned Rate Limiting and Built-in CORS Handler


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Rust trait-based plugin system with built-ins, DP-owned token buckets, built-in CORS handler](#rust-trait-based-plugin-system-with-built-ins-dp-owned-token-buckets-built-in-cors-handler)
  - [Starlark sandbox for tenant-defined custom plugins in the MVP](#starlark-sandbox-for-tenant-defined-custom-plugins-in-the-mvp)
  - [Redis-backed distributed rate limiting in the MVP](#redis-backed-distributed-rate-limiting-in-the-mvp)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-plugins-rate-limit-cors`
## Context and Problem Statement

The gear must deliver the plugin system, rate limiting, and CORS behavior mandated by the accepted docs ADRs 0002, 0003, 0004, 0008, and 0009: Auth/Guard/Transform plugin traits with registries and built-ins, data-plane-owned per-instance token-bucket rate limiting with hierarchical min-merge, and a built-in CORS handler with a local preflight fast path. The questions are how much of the custom-plugin story and the distributed rate-limiting story belong in the MVP, given the no-new-dependencies constraint.

## Decision Drivers

* Accepted ADR 0002: three plugin types with separate traits and deterministic execution order (Auth → Guards → Transform(request) → upstream call → Transform(response/error); upstream plugins before route plugins); custom plugins are schema/catalog-level for now
* Accepted ADR 0008: OAuth2 client-credentials auth plugin (Form and Basic variants) with an internal `pingora-memory-cache` token cache; accepted ADR 0009: `required_headers` guard with fail-open semantics
* Accepted ADR 0003: per-instance (data-plane-owned) rate-limit enforcement is REQUIRED for the MVP; Redis-backed sync is OPTIONAL/future
* Accepted ADR 0004: CORS is a built-in data-plane capability — permissive preflight 204 fast path, actual-request 403 enforcement, secure defaults
* Constraint `cpt-cf-oagw-nfr-build-constraints`: no new dependencies — the Starlark runtime is not in the lockfile; catalog-only plugin identifiers must not bind

## Considered Options

1. **Rust trait-based plugin system with built-ins, DP-owned in-memory token buckets, and a built-in CORS handler** — the MVP delivers all required built-in behavior in-process
2. **Starlark sandbox for tenant-defined custom plugins in the MVP** — execute Starlark plugin source at request time
3. **Redis-backed distributed rate limiting in the MVP** — synchronize rate-limit counters across instances

## Decision Outcome

Chosen option: "Rust trait-based plugin system with built-ins, DP-owned in-memory token buckets, and a built-in CORS handler", because it delivers every behavior the accepted ADRs mark REQUIRED while honoring the no-new-dependencies constraint; distributed and sandboxed-extension features are explicitly deferred by the accepted ADRs themselves.

**Plugin system**: `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` traits plus per-type registries (`AuthPluginRegistry`, `GuardPluginRegistry`, `TransformPluginRegistry`) per accepted ADR 0002. Built-in plugins:

* Auth: `noop`, `apikey`, `oauth2_client_cred` (Form) and `oauth2_client_cred_basic` (Basic) per accepted ADR 0008 — token fetched on cache miss for a `(tenant, subject, config)` tuple, cached in `pingora-memory-cache` with TTL `min(config_ttl, expires_in − 30s)`, and a `CachedToken` key-verification wrapper for hash-collision safety
* Guard: `required_headers` per accepted ADR 0009 — presence-only, case-insensitive, independently configurable request/response phases, fail-open when unconfigured
* Transform: `request_id` — `X-Request-ID` injection/propagation

Catalog-only identifiers (`basic`/`bearer` auth, `timeout`/`cors` guard, `logging`/`metrics` transform) remain type-catalog entries only and are not bound via a registry — referencing them fails resolution with an "unknown plugin" condition, per DESIGN.md.

**Rate limiting**: data-plane-owned, per-instance token-bucket enforcement per accepted ADR 0003 — dual-rate (sustained + burst capacity) configuration, hierarchical min-merge (`min(parent_effective, child)`), reject strategy producing 429 with `X-RateLimit-*` and `Retry-After` headers; `queue` and `degrade` strategies per configuration. Redis-backed distributed synchronization is deferred as OPTIONAL/future per accepted ADR 0003 and is NOT in the MVP.

**CORS**: built-in data-plane handler per accepted ADR 0004 — browser preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) answered locally with a permissive 204 fast path (no upstream resolution or per-request auth), actual cross-origin requests validated after upstream resolution with 403 on disallowed origin/method, exact (port- and protocol-sensitive) origin matching, `allow_credentials` with wildcard origin rejected at configuration time, `Vary: Origin` always present, and sharing-mode merge (origins unioned on `inherit`, no additions on `enforce`).

Alternatives rejected:

* **Starlark sandbox for custom plugins in the MVP**: deferred because accepted ADR 0002 places tenant-defined custom plugins at the schema/catalog level for now, and no Starlark runtime exists in the workspace lockfile — adding one would violate `cpt-cf-oagw-nfr-build-constraints`. Plugin lifecycle CRUD and source retrieval still ship via the control-plane repository (ADR 0001); executing tenant-defined source is a future iteration.
* **Redis-backed rate limiting now**: rejected because accepted ADR 0003 explicitly marks distributed/global synchronization OPTIONAL/future and per-instance enforcement as the REQUIRED MVP behavior; it would also introduce a new dependency.

### Consequences

* Good, because every REQUIRED plugin/rate-limit/CORS behavior from accepted ADRs 0002/0003/0004/0008/0009 is delivered with deterministic, testable semantics
* Good, because rate checks stay in-memory on the data-plane hot path (low latency) and cached OAuth2 tokens avoid per-request identity-provider calls
* Good, because no new dependencies are introduced and catalog-only identifiers fail deterministically
* Bad, because per-instance rate limiting is not globally accurate across instances (accepted trade-off of ADR 0003 for the MVP)
* Bad, because tenant-defined Starlark custom plugins and their sandbox constraints are not executable in the MVP — only lifecycle management ships
* Neutral, because the plugin traits/registries and repository boundaries leave Starlark execution and Redis sync as additive future changes behind existing seams

### Confirmation

Confirmed when:

* Code review verifies the three plugin traits and per-type registries, the built-in plugin set (auth: noop/apikey/oauth2 form+basic with the ADR 0008 cache key and TTL rule; guard: required_headers fail-open; transform: request_id), and that catalog-only identifiers are not registered
* Integration tests verify execution order (Auth → Guards → Transform(request) → upstream → Transform(response/error)), OAuth2 token isolation per `(tenant, subject, config)` and non-caching of failed fetches, and required_headers request/response rejection with phase-appropriate status
* Integration tests verify 429 with `X-RateLimit-*` and `Retry-After`, hierarchical `min()` merge under `enforce`, preflight 204 fast path, actual-request 403 for disallowed origin/method, and configuration-time rejection of `allow_credentials` with wildcard origin

## Pros and Cons of the Options

### Rust trait-based plugin system with built-ins, DP-owned token buckets, built-in CORS handler

* Good, because all REQUIRED behavior ships in-process with no new dependencies
* Good, because native Rust plugins have zero interpreter overhead on the hot path (accepted ADR 0002 rationale)
* Good, because deterministic execution order is enforced by the chain composition
* Bad, because custom tenant code cannot run until a sandboxed runtime is added later

### Starlark sandbox for tenant-defined custom plugins in the MVP

* Good, because operators could write and deploy custom guards/transforms without recompilation
* Good, because sandboxing (no network/file I/O, time/memory limits) is the intended end state per `cpt-cf-oagw-nfr-starlark-sandbox`
* Bad, because no Starlark runtime is in the workspace lockfile — adding one violates the no-new-dependencies NFR
* Bad, because it is not needed to satisfy any REQUIRED MVP behavior; accepted ADR 0002 keeps custom plugins at schema/catalog level for now

### Redis-backed distributed rate limiting in the MVP

* Good, because globally accurate enforcement across instances
* Good, because it matches the accepted hybrid local + periodic sync direction of ADR 0003
* Bad, because accepted ADR 0003 marks it OPTIONAL/future and per-instance enforcement as the MVP requirement
* Bad, because it adds a Redis dependency and a sync protocol for no REQUIRED behavior gain

## More Information

These in-repo pipeline ADRs complement — and do not supersede — the gear's accepted behavioral records in `docs/ADR/`:

- [ADR 0002: Plugin System](../../docs/ADR/0002-plugin-system.md) — authoritative three-trait plugin model and execution order this decision implements
- [ADR 0003: Rate Limiting](../../docs/ADR/0003-rate-limiting.md) — authoritative token-bucket dual-rate configuration, hierarchical budget allocation, and per-instance MVP stance this decision implements
- [ADR 0004: CORS](../../docs/ADR/0004-cors.md) — authoritative built-in CORS handler, preflight fast path, and secure-default semantics this decision implements
- [ADR 0008: OAuth2 Client Credentials Auth Plugin](../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md) — authoritative token-cache design this decision relies on
- [ADR 0009: Required Headers Guard Plugin](../../docs/ADR/0009-required-headers-guard-plugin.md) — authoritative fail-open guard behavior this decision relies on
- [DESIGN.md](../../docs/DESIGN.md) — plugin identification model, built-in vs catalog-only plugin inventory, and CORS/rate-limit configuration shape

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-plugin-system` — Three plugin types with deterministic execution order and upstream-before-route composition
* `cpt-cf-oagw-fr-builtin-plugins` — Built-in auth/guard/transform plugins registered; catalog-only identifiers not bound
* `cpt-cf-oagw-fr-plugin-lifecycle` — Plugin CRUD and in-use protection via the in-memory plugin repository (execution of tenant-defined source deferred)
* `cpt-cf-oagw-fr-plugin-source` — Custom plugin source retrieval served by the control plane
* `cpt-cf-oagw-fr-required-headers-guard` — Presence-only, case-insensitive, fail-open guard with independent request/response phases
* `cpt-cf-oagw-fr-auth-injection` — Credential injection via built-in auth plugins with `cred_store` references
* `cpt-cf-oagw-fr-oauth2-token-cache` — Cache-miss fetch, `min(config_ttl, expires_in − 30s)` TTL, per-tenant/subject isolation, no caching of failed fetches
* `cpt-cf-oagw-fr-rate-limiting` — Dual-rate token bucket, per-instance enforcement, hierarchical min-merge, 429 with retry guidance
* `cpt-cf-oagw-fr-cors` — Local preflight 204 fast path, actual-request origin/method enforcement, credentials+wildcard rejected at config time
* `cpt-cf-oagw-fr-hierarchical-config` — CORS union/inherit and enforced ancestor rate caps honored
* `cpt-cf-oagw-fr-config-layering` — Merged plugin/rate/CORS configuration per Upstream < Route < Tenant priority
* `cpt-cf-oagw-nfr-low-latency` — In-memory token buckets and cached tokens keep the hot path fast
* `cpt-cf-oagw-nfr-credential-isolation` — Credentials resolved via `cred_store` references; cached OAuth2 tokens tenant/subject-isolated
* `cpt-cf-oagw-nfr-build-constraints` — No new dependencies (Starlark and Redis not introduced)
* `cpt-cf-oagw-nfr-starlark-sandbox` — Deferred with the custom-plugin runtime; not in the MVP per accepted ADR 0002
* `cpt-cf-oagw-usecase-rate-limit-exceeded` — Rate-limit condition with retry guidance per configured strategy
* `cpt-cf-oagw-usecase-cors-preflight` — Local preflight answer; enforcement deferred to the actual request
