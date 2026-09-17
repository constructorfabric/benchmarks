---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# Request Routing — Path-Based Routing Between API Handler, Control Plane, and Data Plane

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Routing Rules](#routing-rules)
  - [Request Flows](#request-flows)
  - [X-OAGW-Target-Host Behavior Matrix](#x-oagw-target-host-behavior-matrix)
  - [Request Classification (HTTP vs gRPC)](#request-classification-http-vs-grpc)
  - [Plugin Deletion Behavior](#plugin-deletion-behavior)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Path-based routing](#path-based-routing)
  - [Control Plane handles everything](#control-plane-handles-everything)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-request-routing`

**Priority**: p1

## Context and Problem Statement

With three logical components (API Handler, Data Plane, Control Plane), OAGW needs to define how inbound requests are routed between them. Management operations (CRUD for upstreams, routes, and plugins) modify configuration, while proxy operations execute calls to external services. The question is: which component handles which operations?

## Decision Drivers

* Clear separation of management vs proxy concerns
* Minimal latency for management operations (no unnecessary hops)
* Control Plane focus on configuration data ownership, database access, and cache invalidation
* Data Plane focus on proxy orchestration and plugin execution
* Simple, deterministic routing rules

## Considered Options

* Path-based routing: the API Handler routes requests based on the URL path prefix
* Control Plane handles everything: all requests go to the Control Plane, which calls the Data Plane as needed

## Decision Outcome

Chosen option: "Path-based routing", because it provides the shortest path for each operation type and maintains clear separation of concerns between configuration management and request execution.

### Routing Rules

| Path Pattern | Routed To | Purpose |
|---|---|---|
| `/api/oagw/v1/upstreams/*` | Control Plane | Upstream CRUD |
| `/api/oagw/v1/routes/*` | Control Plane | Route CRUD |
| `/api/oagw/v1/plugins/*` | Control Plane | Plugin CRUD |
| `/api/oagw/v1/proxy/*` | Data Plane | Proxy requests |

### Request Flows

**Management operations** (e.g., create an upstream) flow: Client → API Handler (auth, rate limit) → Control Plane (validate, write database, invalidate cache) → Response.

**Proxy operations** (e.g., `GET /api/oagw/v1/proxy/{alias}/...`): Client → API Handler (auth, rate limit) → Data Plane (orchestrate), which resolves the upstream and route configuration through the Control Plane, executes the plugin chain (auth, guard, transform), performs the HTTP call to the external service, and returns the response.

### X-OAGW-Target-Host Behavior Matrix

For multi-endpoint upstreams, the `X-OAGW-Target-Host` header selects a specific endpoint within the pool:

| Scenario | Endpoints | Alias Type | Header Present | Behavior |
|---|---|---|---|---|
| Single endpoint | 1 | Any | No | Route to endpoint (no change) |
| Single endpoint | 1 | Any | Yes | Validate and route (header optional but validated if present) |
| Multi-endpoint | 2+ | Explicit (no common suffix) | No | Round-robin load balancing |
| Multi-endpoint | 2+ | Explicit (no common suffix) | Yes | Route to specific endpoint (bypass load balancing) |
| Multi-endpoint | 2+ | Common suffix | No | 400 Bad Request (missing required header) |
| Multi-endpoint | 2+ | Common suffix | Yes | Route to specific endpoint |

### Request Classification (HTTP vs gRPC)

At request time, the proxy handler resolves the upstream alias first, then uses the upstream's protocol to select the route match strategy: HTTP upstreams match routes using HTTP match keys (method allowlist + longest path prefix); gRPC upstreams would match routes using `(service, method)` parsed from the gRPC request path. gRPC route matching is a Phase 3 capability and is **not implemented** — only HTTP proxy routing is implemented today.

### Plugin Deletion Behavior

Deleting a plugin that is referenced by any upstream or route fails with `409 Conflict`; the problem-detail response carries a `referenced_by` shape listing the referencing upstream and route identifiers. A plugin that is not in use is deleted successfully with `204 No Content`.

### Consequences

* Good, because clear separation: Control Plane = data management, Data Plane = request execution
* Good, because a shorter path for management operations (API Handler → Control Plane direct)
* Good, because the Data Plane remains focused on proxy logic
* Good, because the Control Plane can optimize cache invalidation during writes
* Bad, because the Data Plane depends on the Control Plane for every proxy request on a cache miss
* Neutral, mitigated by the Data Plane L1 cache for hot configurations

### Confirmation

Verified by inspecting REST handler registration: management endpoints route to `ControlPlaneService` trait methods, proxy endpoints route to `DataPlaneService` trait methods.

## Pros and Cons of the Options

### Path-based routing

The API Handler routes requests based on the URL path prefix.

* Good, because deterministic routing with no ambiguity
* Good, because management operations take the shortest path to the data owner
* Good, because proxy operations go directly to the orchestrator
* Good, because configuration resolution is separated from request orchestration (separation of concerns)
* Bad, because the routing logic is hardcoded in the API Handler

### Control Plane handles everything

All requests go to the Control Plane, which calls the Data Plane as needed.

* Good, because a single entry point simplifies routing
* Bad, because management operations do not need the Control Plane's orchestration logic
* Bad, because it adds an unnecessary hop for configuration CRUD
* Bad, because the Control Plane becomes a bottleneck for all operations

## More Information

* [ADR: Control Plane Caching](./0005-data-plane-caching.md) — L1 cache mitigates the Data Plane → Control Plane dependency
* [ADR: State Management](./0006-state-management.md) — cache and rate limiter ownership

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-upstream-mgmt` — Management operations routed to the Control Plane
* `cpt-cf-oagw-fr-route-mgmt` — Management operations routed to the Control Plane
* `cpt-cf-oagw-fr-request-proxy` — Proxy operations routed to the Data Plane
* `cpt-cf-oagw-fr-alias-resolution` — Alias-based upstream resolution and target-host selection in the proxy path
* `cpt-cf-oagw-interface-management-api` — Management API endpoint routing
* `cpt-cf-oagw-interface-proxy-api` — Proxy API endpoint routing
