---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# Rate Limiting — Token Bucket with Dual-Rate Configuration and Hierarchical Budget Allocation

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
  - [Algorithm Comparison](#algorithm-comparison)
- [Decision Outcome](#decision-outcome)
  - [1. Algorithm: Token Bucket (default), Sliding Window (optional)](#1-algorithm-token-bucket-default-sliding-window-optional)
  - [2. Configuration: Dual-Rate](#2-configuration-dual-rate)
  - [3. Inheritance: Hierarchical Budget Allocation](#3-inheritance-hierarchical-budget-allocation)
  - [4. Distribution: Hybrid Local + Periodic Sync](#4-distribution-hybrid-local--periodic-sync)
  - [Rejection Response Contract](#rejection-response-contract)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Token Bucket with dual-rate configuration and hierarchical budget allocation](#token-bucket-with-dual-rate-configuration-and-hierarchical-budget-allocation)
  - [Flat configuration with independent per-node limits](#flat-configuration-with-independent-per-node-limits)
  - [Sliding Window with centralized Redis synchronization](#sliding-window-with-centralized-redis-synchronization)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-rate-limiting`

**Priority**: p3

## Context and Problem Statement

OAGW needs a rate limiting strategy that addresses three concerns: (1) algorithm selection for controlling request rates, (2) hierarchical inheritance of limits through the tenant hierarchy (system → partner → tenant), and (3) distributed state synchronization of rate limit counters across OAGW nodes. The question is which algorithm, configuration model, inheritance model, and distribution strategy to adopt.

## Decision Drivers

* Low latency impact (<1ms for a rate check)
* Accurate enforcement across distributed nodes
* Hierarchical budget allocation (a parent can cap its children)
* Fair sharing among tenants
* Burst handling without starving steady traffic
* Observability (remaining quota, reset time)

## Considered Options

* Token Bucket with dual-rate configuration, hierarchical budget allocation, and hybrid local + periodic sync (chosen)
* Flat configuration with independent per-node limits — simplest possible model
* Sliding Window with centralized Redis synchronization — most accurate, highest cost

### Algorithm Comparison

| Algorithm | Pros | Cons | Use Case |
|---|---|---|---|
| Token Bucket | Allows bursts, simple, memory efficient | Burst at window boundary | Default for most APIs |
| Leaky Bucket | Smooth output rate | No burst tolerance, queue management | Steady-rate backends |
| Fixed Window | Simple, predictable reset | 2x burst at boundary | Simple quotas |
| Sliding Window | No boundary burst, accurate | Slightly more compute | Strict rate enforcement |

## Decision Outcome

Chosen option: "Token Bucket with dual-rate configuration, hierarchical budget allocation, and hybrid local + periodic sync", because it separates sustained rate from burst capacity, enforces parent capacity constraints on children through the `min()` hierarchy, and balances accuracy against performance for the low-latency proxy hot path.

### 1. Algorithm: Token Bucket (default), Sliding Window (optional)

Token bucket is the industry-standard algorithm (AWS API Gateway, Kong, Envoy), handles bursts up to bucket capacity, and is simple and memory-efficient. Sliding window remains available as an optional configuration choice where strict, boundary-free enforcement is required.

### 2. Configuration: Dual-Rate

Configuration separates the sustained rate from burst capacity using human-readable window units:

| Field | Description |
|---|---|
| `algorithm` | `token_bucket` (default) or `sliding_window` |
| `sustained` | Replenishment rate expressed as `rate` + `window` (`second`, `minute`, `hour`, `day`) |
| `burst` | Bucket capacity — maximum burst allowance, defaults to the sustained rate |
| `scope` | Counter scope: `global`, `tenant`, `user`, `ip`, `route` |
| `strategy` | `reject` (429), `queue`, `degrade` |
| `cost` | Tokens consumed per request (default 1) — enables cost-based rate limiting per route |
| `response_headers` | Include `X-RateLimit-*` headers (default true) |

### 3. Inheritance: Hierarchical Budget Allocation

Parents allocate budgets to children; children cannot exceed their allocation. Effective limit = `min(own_limit, parent_effective_limit)`.

**Budget modes**:

| Mode | Description |
|---|---|
| `unlimited` | No budget tracking (default for leaf tenants) |
| `allocated` | Parent allocates a fixed budget to children |
| `shared` | Children share the parent's budget (first-come-first-served) |

**Inheritance rules** (sharing modes `private` / `inherit` / `enforce`):

| Parent Sharing | Child Specifies | Effective Limit |
|---|---|---|
| `private` | any | Child's limit only |
| `inherit` | none | Parent's limit |
| `inherit` | own limit | `min(parent, child)` |
| `enforce` | any | `min(parent, child)` — cannot exceed parent |

**Budget validation on child creation**: The sum of child allocations may not exceed the parent's budget `total`, subject to a configurable `overcommit_ratio` (default 1.0, range 1.0–2.0). With `overcommit_ratio = 1.0` an over-subscribed child is rejected; with a higher ratio it is allowed with a warning.

### 4. Distribution: Hybrid Local + Periodic Sync

For the MVP, rate limiting is per-instance in the Data Plane (no distributed coordination). A Redis-backed distributed mode is planned as a future extension: each node maintains a local token bucket and periodically (configurable, default 100ms) pushes local consumption and pulls the global counter, adjusting the local bucket. If Redis is unavailable, the node falls back to local-only (degraded accuracy). Rate limiting executes in the Data Plane; configuration is resolved from Control Plane caches during upstream/route resolution. In the worst case a burst can exceed the limit by `burst_capacity × node_count`.

### Rejection Response Contract

When the `reject` strategy is applied, the response is a 429 that communicates retry timing to the caller via `Retry-After` and the standard `X-RateLimit-*` response headers (RFC 6585 / draft-ietf-httpapi-ratelimit-headers):

```http
X-RateLimit-Limit: 100
X-RateLimit-Remaining: 0
X-RateLimit-Reset: 1706626800
Retry-After: 30
```

### Consequences

* Good, because clear separation of sustained rate vs burst capacity
* Good, because hierarchical budget prevents a child from exceeding parent allocation
* Good, because hybrid sync balances accuracy vs performance
* Good, because standard response headers enable client integration
* Bad, because Redis dependency required for distributed accuracy
* Bad, because complexity in budget allocation validation
* Bad, because the sync interval introduces an accuracy trade-off
* Risk, Redis failure degrades to local-only (less accurate)
* Risk, overcommit can lead to contention under high load
* Risk, clock skew between nodes affects sliding window accuracy

### Confirmation

Integration tests verify: (1) token bucket allows bursts up to capacity, (2) hierarchical limits enforce `min(parent, child)`, (3) 429 responses include `X-RateLimit-*` and `Retry-After` headers.

## Pros and Cons of the Options

### Token Bucket with dual-rate configuration and hierarchical budget allocation

The chosen strategy: token bucket algorithm, dual-rate sustained/burst configuration, hierarchical budget allocation with `min()` inheritance, and hybrid local + periodic sync distribution.

* Good, because allows bursts up to bucket capacity
* Good, because simple and memory efficient
* Good, because industry standard
* Good, because separates sustained rate from burst capacity
* Good, because enforces parent capacity constraints on children
* Bad, because burst possible at window boundary
* Bad, because more complex validation on child creation
* Bad, because sync interval introduces an accuracy trade-off in distributed mode

### Flat configuration with independent per-node limits

A single rate/window configured at each level with no sharing and no distributed state.

* Good, because simple implementation and no external dependency
* Bad, because no burst/sustained distinction
* Bad, because no algorithm choice
* Bad, because a partner's tenants can exceed the partner's allocation without enforcement
* Bad, because inaccurate if traffic is not evenly distributed across nodes

### Sliding Window with centralized Redis synchronization

Strict enforcement with a centralized counter in Redis for every request.

* Good, because no boundary burst and accurate
* Good, because accurate global enforcement across nodes
* Bad, because slightly more compute per check
* Bad, because added latency (~1–2ms per request) and Redis becomes a single point of failure

## More Information

- [ADR: State Management](./0006-state-management.md) — rate limiter ownership and per-instance enforcement
- [ADR: Error Source Distinction](./0007-error-source-distinction.md) — rate-limit exceeded gateway error with `Retry-After`
- [Kong Rate Limiting](https://konghq.com/blog/engineering/how-to-design-a-scalable-rate-limiting-algorithm)
- [Envoy Global Rate Limiting](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/other_features/global_rate_limiting)
- [AWS API Gateway Throttling](https://docs.aws.amazon.com/apigateway/latest/developerguide/api-gateway-request-throttling.html)
- [IETF RateLimit Headers Draft](https://datatracker.ietf.org/doc/draft-ietf-httpapi-ratelimit-headers/)

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-rate-limiting` — Rate limiting algorithm, configuration, and enforcement
* `cpt-cf-oagw-fr-hierarchical-config` — Hierarchical budget allocation and sharing modes
* `cpt-cf-oagw-nfr-low-latency` — <1ms rate check latency via local token bucket
* `cpt-cf-oagw-usecase-rate-limit-exceeded` — 429 response with `Retry-After` header
