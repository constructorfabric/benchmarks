---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# Control Plane Caching — Multi-Layer L1/L2 Strategy

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Cache Layers](#cache-layers)
  - [Lookup Flow](#lookup-flow)
  - [Cache Keys](#cache-keys)
  - [Cache Invalidation](#cache-invalidation)
  - [Deployment Modes](#deployment-modes)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
  - [Risks](#risks)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [L1 only (in-memory)](#l1-only-in-memory)
  - [L2 only (Redis)](#l2-only-redis)
  - [Multi-layer L1 + L2 + Database](#multi-layer-l1--l2--database)
  - [Write-through cache](#write-through-cache)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-data-plane-caching`

**Priority**: p5

## Context and Problem Statement

The Control Plane handles configuration resolution for the Data Plane during proxy requests. Configuration data (upstreams, routes, plugins) is read-heavy and changes infrequently. A caching strategy is needed that minimizes database load, provides fast lookups (<1ms for hot configs), supports both single-exec and microservice modes, and handles cache invalidation on configuration writes.

## Decision Drivers

* Fast lookups for hot configs (<1us L1, ~1-2ms L2)
* Reduced database load (queries only on cache miss)
* Support for both single-exec (no Redis) and microservice (shared L2) deployment modes
* Correct cache invalidation on config writes

## Considered Options

* L1 only (in-memory)
* L2 only (Redis)
* Multi-layer L1 + optional L2 + Database
* Write-through cache

## Decision Outcome

Chosen option: "Multi-layer caching: L1 (in-memory) + optional L2 (Redis) + Database", because it provides the fastest reads for hot configs while supporting both deployment modes.

### Cache Layers

| Layer | Scope | Capacity | TTL | Access Time | Notes |
|---|---|---|---|---|---|
| L1 (In-Memory) | Per-instance LRU | 10,000 entries | No TTL (LRU eviction) | <1us | Hot configs |
| L2 (Redis, optional) | Shared across instances | Unbounded | 5 minutes | ~1-2ms | MessagePack serialization |
| Database (PostgreSQL) | Source of truth (JSON text) | Unlimited | N/A | ~5-10ms | Queried only on L1+L2 miss |

### Lookup Flow

On a lookup, L1 is checked first (<1us); on a miss the optional L2 is checked (~1-2ms) and the result backfills L1; on an L1+L2 miss the database is queried (~5-10ms) and both L1 and L2 are populated. Caches are lazily populated on read — there is no proactive warming.

### Cache Keys

| Key | Value |
|---|---|
| `upstream:{tenant_id}:{alias}` | Upstream configuration |
| `route:{upstream_id}:{method}:{path_prefix}` | Matched route configuration |
| `plugin:{plugin_id}` | Plugin definition |

### Cache Invalidation

On a configuration write (e.g., `PUT /upstreams/{id}`): (1) the Control Plane writes to the database, (2) it flushes L1 for affected keys, (3) it flushes L2 (if enabled), and (4) the Data Plane flushes its own L1 cache (notified by the Control Plane or periodic sync).

### Deployment Modes

- **Single-exec**: L1 only (no Redis needed)
- **Microservice**: L1 + L2 (Redis shared across instances)

### Consequences

* Good, because fast lookups for hot configs (<1us L1)
* Good, because reduced database load
* Good, because shared cache in microservice mode (L2)
* Good, because simple deployment in single-exec mode (no Redis)
* Bad, because cache invalidation complexity (must flush L1 and L2)
* Bad, because Redis dependency in microservice mode
* Bad, because potential stale data during the cache TTL window

### Confirmation

Integration tests verify: an L1 cache hit returns the correct config, an L1 miss falls through to L2/DB, a config write flushes both L1 and L2, and single-exec mode works without Redis.

### Risks

- **Risk**: Redis unavailability causes an L2 miss and increased database load. **Mitigation**: L1 cache stays active (10k entries) and the database connection pool limits concurrent queries.

## Pros and Cons of the Options

### L1 only (in-memory)

A per-instance in-memory cache with no shared layer.

* Good, because the simplest implementation and the fastest reads
* Bad, because in microservice mode each instance hits the DB independently (high load)

### L2 only (Redis)

A shared Redis cache with no in-memory layer.

* Good, because shared across instances
* Bad, because slower than L1 (serialization overhead) and unnecessary for single-exec

### Multi-layer L1 + L2 + Database

The chosen option: L1 for speed, optional L2 for sharing, database as the source of truth.

* Good, because optimal read performance (L1 for speed, L2 for sharing)
* Good, because optional L2 keeps single-exec simple
* Bad, because invalidation must cover multiple layers

### Write-through cache

Writes go through the cache to keep it always consistent.

* Good, because the cache is always consistent
* Bad, because it complicates writes and does not significantly help a read-heavy workload

## More Information

- [ADR: State Management](./0006-state-management.md) — cache and rate limiter ownership

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-nfr-low-latency` — L1 cache provides <1us config lookups on the hot path
* `cpt-cf-oagw-fr-request-proxy` — Config resolution during proxy request execution
