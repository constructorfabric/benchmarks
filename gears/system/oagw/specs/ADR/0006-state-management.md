---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# State Management — Data Plane L1 Cache and Rate Limiter Ownership

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [DP State](#dp-state)
  - [CP State](#cp-state)
  - [Request Flow with Caching](#request-flow-with-caching)
  - [Cache Invalidation](#cache-invalidation)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
  - [Rationale](#rationale)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [DP stateless](#dp-stateless)
  - [DP with L1 cache + rate limiters](#dp-with-l1-cache--rate-limiters)
  - [CP owns rate limiters](#cp-owns-rate-limiters)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-state-management`

**Priority**: p6

## Context and Problem Statement

With the Data Plane (DP) calling the Control Plane (CP) for configuration resolution, OAGW needs to decide how state is managed: whether the DP should cache frequently-accessed configs, where rate limiters should live, and how to balance performance against consistency.

## Decision Drivers

* Minimize latency on the proxy hot path
* Reduce CP calls for frequently-accessed configs
* Simple rate limiting for MVP (no distributed coordination)
* Clear ownership of state between CP and DP

## Considered Options

* DP stateless (call CP for every request)
* DP with L1 cache + rate limiters owned by DP
* CP owns rate limiters (DP calls CP for rate checks)

## Decision Outcome

Chosen option: "DP with L1 cache + rate limiters owned by DP", because the DP handles every proxy request and needs fast access to hot configs and rate limit state.

### DP State

- **L1 Cache**: Small in-memory LRU (1000 entries, no TTL, explicit invalidation), caching upstream and route configs resolved from the CP. Configurable via environment variable.
- **HTTP Client**: Shared client for calls to external (upstream) services.
- **Rate Limiters**: Per-instance token buckets owned by the DP — the DP holds the full request context (tenant, upstream, route).

### CP State

- **L1 Cache**: Larger in-memory LRU (10,000 entries) — the authoritative cache, backed by an optional L2 (Redis) and the database.
- **L2 Cache**: Optional Redis layer shared across instances.
- **Database Pool**: Connection pool for persistent storage.

### Request Flow with Caching

The DP checks its L1 cache for the resolved (upstream, route) configuration: on a hit the cached config is used (<1us); on a miss the DP calls the CP to resolve the proxy target (tenant hierarchy walk with alias shadowing and route matching, effective config merge upstream < route < tenant), and the resolved pair is cached in the DP L1. The chain then executes: auth plugin, rate limit check (DP-owned), guard/transform plugins, HTTP call to the external service, and response return.

### Cache Invalidation

On a configuration write: the CP writes to the database, flushes its own caches, and returns success. The API Handler notifies the DP to flush its L1 cache (or the DP periodically syncs).

### Consequences

* Good, because fast path — the DP serves hot configs from L1 (<1us)
* Good, because reduced CP calls (only for cache misses)
* Good, because simple rate limiting (no distributed coordination for MVP)
* Bad, because the DP L1 can temporarily diverge from the CP (stale data)
* Bad, because per-instance rate limiting is not globally accurate

### Confirmation

Integration tests verify: a DP L1 cache hit avoids a CP call, a configuration write triggers a DP cache flush, and the rate limiter correctly counts per-instance requests.

### Rationale

**Why the DP has an L1 cache**: The DP handles every proxy request, so caching hot configs reduces CP calls and provides <1us access; a small 1000-entry cache has negligible memory overhead.

**Why rate limiters live in the DP**: The DP already has the request context, this avoids an extra CP call per request, and per-instance limiting is acceptable for the MVP.

**Why the CP is the authoritative cache**: The CP owns database access and can optimize cache invalidation during writes; the DP L1 is only an optimization layer.

## Pros and Cons of the Options

### DP stateless

The DP makes a CP call for every request (no L1 cache).

* Good, because always consistent
* Bad, because too many CP calls, adds latency for hot configs

### DP with L1 cache + rate limiters

The DP owns a small L1 cache and its own rate limiters.

* Good, because fast reads (<1us for cached configs)
* Good, because the rate limiter has full request context
* Bad, because cache consistency lag after writes

### CP owns rate limiters

The DP calls the CP to check rate limits.

* Good, because centralized rate limit state
* Bad, because an extra CP call per request is not worth the overhead for the MVP

## More Information

**Risk**: The DP L1 cache becomes stale after a configuration write. **Mitigation**: Explicit cache invalidation from the CP on config writes (no TTL; entries persist until invalidated).

**Risk**: Per-instance rate limiting is less accurate than distributed limiting. **Mitigation**: Acceptable for the MVP; future work can add a Redis-backed distributed rate limiter as a DP extension.

- [ADR: Control Plane Caching](./0005-data-plane-caching.md)
- [ADR: Request Routing](./0001-request-routing.md)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-nfr-low-latency` — DP L1 cache provides <1us config lookups on the hot path
* `cpt-cf-oagw-fr-rate-limiting` — Rate limiters owned by the DP for per-instance enforcement
* `cpt-cf-oagw-fr-request-proxy` — Caching strategy optimizes proxy request execution
