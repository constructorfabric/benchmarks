---
title: "PRD — Outbound API Gateway (OAGW) Gear"
system: "cf-oagw"
version: "1.0"
kind: "PRD"
status: "draft"
created: "2026-08-27"
---

# PRD — Outbound API Gateway (OAGW) Gear

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
  - [5.1 Gear Registration & Configuration](#51-gear-registration--configuration)
  - [5.2 Upstreams Management](#52-upstreams-management)
  - [5.3 Routes Management](#53-routes-management)
  - [5.4 Plugins Management](#54-plugins-management)
  - [5.5 Proxy Data Plane](#55-proxy-data-plane)
  - [5.6 Authentication Injection](#56-authentication-injection)
  - [5.7 Rate Limiting](#57-rate-limiting)
  - [5.8 CORS](#58-cors)
  - [5.9 Error Semantics](#59-error-semantics)
  - [5.10 Configuration Hierarchy](#510-configuration-hierarchy)
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
- [Traceability](#traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

The Outbound API Gateway (OAGW) is a self-contained service component (gear) in the Constructor Fabric gears-rust workspace that manages all outbound API requests from gears to external services. It acts as a centralized proxy layer that handles credential injection, rate limiting, header transformation, and security enforcement for every external call made by the platform.

OAGW provides a unified interface for application gears to reach external APIs without managing credentials, connection details, or security policies directly. Gears send requests to OAGW's proxy capability, and OAGW resolves the target upstream by alias, matches a route, injects authentication, applies policy plugins, and forwards the request to the external service while observing strict, consistent error semantics.

### 1.2 Background / Problem Statement

Gears need to communicate with external third-party services (for example OpenAI, Stripe, or payment gateways). Without a centralized gateway, each gear must independently manage credentials, rate limits, error handling, and security policies for outbound calls. This leads to credential sprawl, inconsistent error handling, and no unified observability.

OAGW solves these problems by providing a single outbound proxy layer with pluggable authentication, configurable rate limiting, header transformation, and security policies. All external calls flow through OAGW, ensuring consistent credential isolation, audit trails, and policy enforcement across the platform. Because OAGW sits on the hot path for every outbound call, its behavior must be strictly specified (WHAT) so the technical design, ADRs, and implementation can deliver consistent proxy and error semantics.

### 1.3 Goals (Business Outcomes)

- Centralize all outbound API credential management — zero credential exposure in application code, logs, or error messages.
- Provide a unified proxy interface for external services with consistent error handling and observability.
- Enforce rate limiting and security policies (SSRF protection, header validation) to prevent abuse and cost overruns.
- Support multi-tenant hierarchical configuration with sharing, inheritance, and enforcement semantics across a tenant tree.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Upstream | External service target defined by one or more server endpoints (scheme/host/port), protocol, authentication configuration, default headers, and rate limits. |
| Route | An API path on an upstream that matches inbound proxy requests by method, path, and (for HTTP) query allowlist. Routes map proxy requests to specific upstream behaviors and may carry their own rate limit, CORS, and plugin overrides. |
| Plugin | Modular processor attached to upstreams or routes. Three types: Auth (credential injection), Guard (validation/policy enforcement), Transform (request/response mutation). |
| Data Plane | The internal service that orchestrates proxy requests: resolves configuration, executes the plugin chain, and forwards calls to external services. |
| Control Plane | The internal service that manages configuration data (upstreams, routes, plugins) with repository access. |
| Alias | Short identifier used in proxy traffic to reference an upstream. Auto-derived from hostname for hostname-based endpoints (user-provided alias rejected when it differs); explicit alias required for IP-based or non-derivable endpoints. Normalized to lowercase; resolution is case-insensitive. |
| Sharing Mode | Configuration visibility setting for hierarchical tenancy: `private` (owner only), `inherit` (descendants can override), `enforce` (descendants cannot override). |
| GTS | Global Type System — the platform's schema and instance registration system used for plugin and resource type identification. |
| Tenant Hierarchy | The parent/descendant tree of tenants; OAGW resolves configuration from descendant to root, with the closest match winning (shadowing). |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by steering committee. This section documents actors (users, systems) that interact with this gear.

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-oagw-actor-platform-operator`

**Role**: Manages global (root/own tenant) configuration: upstreams, routes, system-wide plugins, and security policies. May configure `enforce`-mode sharing to constrain descendants.
**Needs**: CRUD operations for upstreams, routes, and plugins; ability to enforce configuration on descendant tenants; visibility into all proxy traffic and errors.

#### Tenant Administrator

**ID**: `cpt-cf-oagw-actor-tenant-admin`

**Role**: Manages tenant-specific settings: credentials, rate limits, custom plugins, and configuration overrides within the limits granted by ancestor sharing policies and permissions.
**Needs**: Override inherited configurations where permitted; manage tenant-scoped credentials; set stricter rate limits for their tenant sub-tree; never inadvertently exceed ancestor-enforced limits.

#### Application Developer

**ID**: `cpt-cf-oagw-actor-app-developer`

**Role**: Consumes external APIs via the OAGW proxy capability without managing credentials or external service details.
**Needs**: A simple proxy address keyed on upstream alias (`/api/oagw/v1/proxy/{alias}/{path}`) with transparent credential injection; clear, reliable error responses that distinguish gateway failures from upstream failures so the application can retry or fall back correctly.

### 2.2 System Actors

#### Credential Store

**ID**: `cpt-cf-oagw-actor-cred-store`

**Role**: Secure storage and retrieval of secrets (API keys, OAuth tokens, passwords) by reference (UUID/URI). OAGW never stores credentials directly — it references them via the credential store and resolves them at request time.

#### Types Registry

**ID**: `cpt-cf-oagw-actor-types-registry`

**Role**: GTS schema and instance registration and validation. OAGW registers its plugin type schemas and upstream/route type definitions in the types registry.

#### Tenant Resolution / Authorization Subsystem

**ID**: `cpt-cf-oagw-actor-tenant-resolver`

**Role**: Resolves the tenant hierarchy and supplies tenant identity and permission context (via the security context) for every inbound request to OAGW. OAGW consumes tenant identity and authorization decisions from this subsystem.

#### Upstream Service

**ID**: `cpt-cf-oagw-actor-upstream-service`

**Role**: External third-party service (for example OpenAI or a payment gateway) that OAGW proxies requests to. OAGW treats upstream services as opaque HTTP endpoints.

## 3. Operational Concept & Environment

> **Note**: Project-wide runtime, OS, architecture, lifecycle policy, and integration patterns are defined in the root PRD. Only gear-specific constraints are documented here.

### 3.1 Module-Specific Environment Constraints

- The gear MUST register as a host component of the Constructor Fabric gears-rust workspace host runtime and expose its management and proxy capabilities under the `/oagw/v1` base API path.
- The gear MUST build under the workspace e2e feature set (`config/e2e-features.txt` includes `oagw`).
- The gear MUST run against the workspace e2e deployment configuration, reading its runtime settings from the gear config section `oagw` (`config/e2e-local.yaml`), including proxy timeout, SSRF policy, and an upstream transport policy flag that permits plaintext upstreams only for testing.
- The gear MUST NOT introduce any new dependencies beyond those already present in the workspace lockfile.
- The gear follows standard Gears ToolKit gear conventions (single executable; in-process SDK calls to platform services; no direct external infrastructure management).
- Production upstream connections are HTTPS-only; plaintext upstream transport is permitted only under an explicit testing configuration.

## 4. Scope

### 4.1 In Scope

- CRUD management of upstreams, routes, and plugins (create, read, replace, delete), including retrieval of custom plugin source content.
- HTTP/HTTPS proxy with alias-based upstream resolution, route matching, and multi-endpoint target selection.
- Credential injection via auth plugins (API Key, HTTP Basic, OAuth2 Client Credentials, Bearer Token) with credentials sourced from the credential store at request time.
- Rate limiting at upstream and route levels with configurable dual-rate (sustained/burst) strategies and hierarchical budget inheritance.
- Header transformation (set/add/remove, passthrough control, automatic hop-by-hop header stripping, host handling, request correlation ID propagation).
- Plugin system with three types — Auth, Guard, Transform — built-in and externally registered, plus tenant-defined custom plugins with source retrieval.
- Streaming support: HTTP request/response proxying, Server-Sent Events (SSE), WebSocket, and WebTransport session flows.
- Multi-tenant hierarchical configuration with sharing modes (private/inherit/enforce), alias resolution with shadowing, and enforced ancestor limits.
- Built-in CORS handling per upstream/route, including local preflight processing.
- Error source distinction (gateway-originated vs upstream-originated) with consistent error semantics.
- Metrics collection and audit logging with correlation IDs.

### 4.2 Out of Scope

- DNS resolution and IP pinning rule implementation details (a security concern of the design; the PRD only states the SSRF-protection outcome).
- Plugin versioning and lifecycle management platform details beyond the required immutability, unlink-before-delete, and garbage-collection behavior.
- Response caching (client/upstream responsibility).
- Automatic full-request retries (client responsibility; only connector-level endpoint/connection failover is permitted).
- gRPC proxying (planned for a later phase; no gRPC proxy path is required now).
- Registry-only deployment mode (future platform-level consideration — all upstreams, routes, and plugins configured exclusively from the type registry with no management API CRUD).
- Distributed or shared rate limiting and L2 caching (OPTIONAL / future per accepted ADRs; see Assumptions).

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches (analysis, inspection, demonstration).

Functional requirements define WHAT the system must do, grouped by feature area.

### 5.1 Gear Registration & Configuration

#### Gear Registration with Host Runtime

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-gear-registration`

The gear MUST register itself as a host component of the Constructor Fabric gears-rust host runtime, participate in the host lifecycle (startup and shutdown), and expose its management and proxy capabilities under the `/oagw/v1` base API path. Runtime settings MUST be read from the gear configuration section `oagw` at startup.

**Rationale**: The gear is a self-contained service component; registration is the precondition for every other capability (management and proxy) to be reachable.

**Actors**: `cpt-cf-oagw-actor-platform-operator`

### 5.2 Upstreams Management

#### Upstream Management

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-upstream-mgmt`

The system MUST provide create, read, replace, and delete operations for upstream configurations. Each upstream defines one or more server endpoints (scheme/host/port), protocol, authentication configuration, header rules, rate limits, and CORS configuration. All operations MUST be tenant-scoped: a caller can only create, view, replace, or delete upstreams it owns. The identifier and owning tenant of an upstream MUST be immutable.

**Rationale**: Upstreams are the fundamental configuration unit — every proxy request targets an upstream.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Enable/Disable Semantics

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-enable-disable`

The system MUST support an enable/disable flag (enabled by default) on upstreams and routes. A disabled upstream MUST cause all proxy requests to it to be rejected with a gateway-originated, non-retriable "service unavailable" condition. A disabled route MUST be excluded from route matching. If an ancestor tenant disables an upstream, it MUST be disabled for all descendants, and a descendant MUST NOT be able to re-enable an ancestor-disabled resource.

**Rationale**: Enables temporary maintenance, emergency circuit breaks, and gradual rollouts without deleting configuration.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Multi-Endpoint Pooling Constraints

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-upstream-pooling`

When an upstream defines multiple endpoints, the system MUST treat them as a load-balancing pool and reject configuration that is invalid per pool rules: all endpoints in a pool MUST use the same protocol, the same scheme, and the same port; heterogeneous pools MUST NOT be created or updated.

**Rationale**: Ensures every endpoint in a pool is interchangeable so request distribution is deterministic and safe.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

### 5.3 Routes Management

#### Route Management

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-route-mgmt`

The system MUST provide create, read, replace, and delete operations for routes. A route MUST reference an upstream owned by the calling tenant, MUST define matching rules (HTTP method allowlist, path pattern, and query parameter allowlist; gRPC service/method matching keys are reserved for a later phase), and MAY carry route-level rate limit, CORS, and plugin overrides. The system MUST validate that match rules are deterministic within an upstream (no two enabled routes under the same upstream may share the same path, priority, and method). A route's upstream reference MUST be immutable after creation.

**Rationale**: Routes control which requests reach which upstream endpoints and with what transformations; deterministic matching is required for predictable proxy behavior.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

### 5.4 Plugins Management

#### Plugin System with Three Types

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-plugin-system`

The system MUST provide a plugin system with three plugin types identified via GTS identifiers: Auth (credential injection), Guard (validation/policy enforcement that can reject a request), and Transform (request/response/error mutation). Execution order MUST be deterministic: Auth, then Guards, then Transform on request, then the upstream call, then Transform on response/error. Upstream plugins MUST execute before route plugins. Plugin definitions MUST be immutable after creation — updates are performed by creating a new plugin version and re-binding references. Circuit breaker is a core gateway resilience capability (configured as core policy), not a plugin.

**Rationale**: Extensibility for custom authentication schemes, validation rules, and request/response transformations without modifying the gateway core.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Built-in Plugins

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-builtin-plugins`

The system MUST include the following built-in plugins:

- **Auth**: `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` (no authentication); `...apikey.v1` (API key injection); `...oauth2_client_cred.v1` (OAuth2 client credentials, form encoding); `...oauth2_client_cred_basic.v1` (OAuth2 client credentials with basic client authentication); `...basic.v1` and `...bearer.v1` (HTTP Basic and Bearer token — catalog identifiers only, with no backing implementation; using either as the auth type MUST fail with an "unknown auth plugin" condition).
- **Guard**: `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` (required-header presence enforcement; the only guard identifier bindable via the plugin chain). The `...timeout.v1` and `...cors.v1` guard identifiers are catalog-only — timeout and CORS are core data-plane capabilities, not bindable guard plugins.
- **Transform**: `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1` (request correlation ID propagation). The `...logging.v1` and `...metrics.v1` transform identifiers are catalog-only — logging and metrics are core data-plane instrumentation, not bindable transform plugins.

**Rationale**: Covers the most common outbound API authentication, validation, and observability patterns out of the box.

**Actors**: `cpt-cf-oagw-actor-platform-operator`

#### Custom Plugin Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-plugin-lifecycle`

The system MUST support creation, listing, retrieval, and deletion of tenant-defined custom plugins. Custom plugins MUST be immutable after creation. Deletion MUST be refused while a plugin is referenced by any upstream or route binding (a "plugin in use" condition); only unlinked plugins MAY be deleted. The system MUST automatically garbage-collect unlinked custom plugins after a configurable retention period (default 30 days). Plugin bindings MUST be validated on write so that a referenced built-in or custom plugin resolves to a registry-available plugin of the matching plugin schema type.

**Rationale**: Ensures bound plugins always resolve, prevents deletion of plugins in active use, and bounds storage growth for abandoned plugins.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Plugin Source Retrieval

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-plugin-source`

The system MUST provide the capability to retrieve the source content of a custom plugin by its identifier, so operators and administrators can inspect or audit what code a plugin executes.

**Rationale**: Custom plugin code is executable policy; auditable source retrieval is required for trust and review.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Required Headers Guard Plugin

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-required-headers-guard`

The system MUST provide a built-in guard plugin that enforces the presence of configured header names on the inbound request (before proxying to the upstream) and/or on the upstream's response (before returning to the caller), configurable independently per phase. Header names MUST be matched case-insensitively; only presence is checked, not values. The plugin MUST reject the request (or the response) when the first configured header is missing, using a phase-appropriate error (request phase: a validation error; response phase: an upstream-relative error). The plugin MUST fail open — when not configured or configured blank, it MUST NOT alter behavior.

**Rationale**: Many upstreams require specific headers (for example a correlation ID or API version); a built-in presence check avoids a bespoke custom guard per upstream, while remaining strictly opt-in.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

### 5.5 Proxy Data Plane

#### Request Proxying

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-request-proxy`

The system MUST proxy client requests through a unified proxy capability addressed as `{METHOD} /api/oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`. It MUST resolve the upstream by alias, match the route by protocol, method, and path, merge configurations (upstream base, then route, then tenant), execute the plugin chain, and forward the request to the external service. The gateway MUST NOT perform automatic full-client-request retries (it will not re-issue the original client request as a whole); connector-level endpoint/connection failover or connection-retry attempts performed by the upstream connector are permitted.

**Rationale**: Core value proposition — a unified proxy endpoint that handles credential injection, transformation, and forwarding transparently.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Alias Resolution and Shadowing

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-alias-resolution`

The system MUST identify upstreams by alias in proxy traffic. Alias value MUST be enforced based on the upstream's endpoint type: hostname-based endpoints always auto-derive the alias (a user-provided alias that differs is rejected; an exact match is an idempotent no-op), while IP-based or non-derivable endpoints require an explicit alias. Derivation rules: a single hostname uses the hostname (port omitted for standard ports); multiple hostnames use the longest common domain suffix (at least two labels) validated against the public suffix list, so a bare public suffix (for example `co.uk`) is rejected as non-derivable; IP addresses or hostnames with no registrable common suffix require an explicit alias. Aliases MUST be normalized to ASCII lowercase with trailing dots stripped, and resolution MUST be case-insensitive. An alias MUST be unique within a tenant, and MUST be immutable once set (a change that would alter a derived alias is rejected; the operator deletes and re-creates the upstream). When resolving an alias, the system MUST search the tenant hierarchy from descendant to root; the closest match wins (a descendant shadows an ancestor). Enforced limits from ancestors MUST still apply across shadowing.

**Rationale**: Provides human-readable proxy addressing while supporting multi-tenant isolation and controlled override semantics.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-platform-operator`

#### Route Matching

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-route-matching`

At request time, the system MUST match the proxied request against the routes of the resolved upstream using the upstream's protocol: for HTTP, match by method allowlist and longest path prefix (with priority ordering), and validate query parameters against the route's query allowlist; a route with path-suffix mode disabled MUST reject requests that include a path suffix, while append mode appends the suffix to the matched path. Requests that match no route MUST be rejected with a "route not found" condition. Mutations of method, path, query, headers, and body beyond default behavior are the responsibility of the plugin chain.

**Rationale**: Deterministic request classification is what maps arbitrary client traffic onto configured upstream behaviors.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Multi-Endpoint Target Selection

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-target-host`

For upstreams with multiple endpoints, the system MUST support explicit selection of the target endpoint by a caller-provided target identifier carried on the proxied request. Selection MUST be validated: a missing required target identifier (for multi-endpoint pools addressed by a common-suffix derived alias), an invalidly formatted identifier, or an identifier that does not match any configured endpoint MUST each be rejected as a validation error with gateway-originated semantics. When no explicit target identifier is provided, the system MUST distribute requests across the pool endpoints (round-robin default). A target identifier provided for a single-endpoint upstream MUST be validated when present.

**Rationale**: Multi-region or multi-endpoint upstreams must be reachable precisely; ambiguous or unknown targets must never be silently misrouted.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Header Rules and Passthrough Control

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-header-transform`

The system MUST transform request and response headers per upstream/route configuration using set, add, and remove operations, and MUST support passthrough control (none / allowlist / all) governing which inbound headers are forwarded to the upstream. The system MUST automatically strip hop-by-hop headers as defined by the HTTP protocol specification, MUST consume routing-related headers during routing without forwarding them, MUST replace the request target host with the upstream host, and MUST validate well-known entity headers (length, type) — invalid values MUST be rejected as validation errors.

**Rationale**: Ensures clean outbound requests and prevents header leakage between internal and external networks.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Passthrough of Upstream Responses

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-passthrough`

The system MUST relay upstream responses — including error responses — to the caller without modifying the body, unless a configured transformation or guard explicitly applies. The gateway MUST NOT cache upstream responses.

**Rationale**: The gateway is transparent with respect to upstream payloads; clients rely on receiving the upstream's exact response.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Streaming Support

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-streaming`

The system MUST support streaming proxy flows: HTTP request/response proxying and Server-Sent Events (SSE) with proper connection lifecycle handling (open/close/error), and MUST support WebSocket and WebTransport session flows. When the upstream closes the connection, the system MUST close the client connection (and log the event); when the client disconnects, the system MUST close the upstream connection.

**Rationale**: Many external APIs (for example streaming chat completions) use SSE, and bidirectional real-time protocols require WebSocket/WebTransport.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Circuit Breaker

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-circuit-breaker`

The system MUST protect upstreams from cascade failure using a per-upstream circuit breaker as core gateway policy (not a plugin). The breaker MUST trip open after repeated consecutive failures in a short window and MUST reject subsequent requests with a gateway-originated, retriable "circuit breaker open" condition while open.

**Rationale**: Prevents an unhealthy upstream from degrading all of its consumers; mandated by the high-availability NFR.

**Actors**: `cpt-cf-oagw-actor-platform-operator`

#### Timeout Enforcement

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-timeout`

The system MUST enforce configurable timeouts for proxy operations (connection establishment, overall request, and idle time) and MUST enforce timeouts on plugin execution. Exceeded timeouts MUST be reported as gateway-originated, retriable timeout conditions.

**Rationale**: Bounded latency for every outbound call; runaway plugin or upstream execution must never hang the client indefinitely.

**Actors**: `cpt-cf-oagw-actor-platform-operator`

#### Body Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-body-size`

The system MUST validate the body of every inbound proxy request: a content-length that is present MUST be a valid integer and match the actual size; a hard maximum body size ceiling (100 MB) MUST be enforced and rejected before buffering; unsupported transfer encodings MUST be rejected. Additional validation (JSON schema, content type checks, custom rules) MUST be supported via guard plugins.

**Rationale**: Prevents resource exhaustion and malformed payloads from reaching external services.

**Actors**: `cpt-cf-oagw-actor-app-developer`

### 5.6 Authentication Injection

#### Credential Injection via Auth Plugins

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-auth-injection`

The system MUST inject credentials into outbound requests via auth plugins. Supported authentication methods MUST include API Key (header/query), HTTP Basic, OAuth2 Client Credentials, and Bearer Token. Credentials MUST be retrieved from the credential store at request time by reference (never embedded in configuration), and MUST be tenant-isolated such that a tenant can only resolve its own or ancestor-shared secrets. Authentication failure conditions MUST be reported distinctly.

**Rationale**: Centralizes credential management so application developers never handle API keys or tokens directly.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-cred-store`

#### OAuth2 Client Credentials Token Handling

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-oauth2-token-cache`

The OAuth2 Client Credentials auth plugin MUST NOT perform an identity-provider token fetch on every proxied request. It MUST fetch a token on a cache miss for a given (tenant, subject, configuration) tuple and serve subsequent requests from cache, with the cached lifetime bounded conservatively by the identity provider's reported token expiry (including a safety margin) and a configurable ceiling. Cached tokens MUST be isolated per tenant and per subject, and MUST never be served to a different tenant or subject even in the event of internal key collision. Failed token fetches MUST NOT be cached, so transient identity-provider failures self-heal on the next request.

**Rationale**: Per-request token fetches cost 100–500 ms and risk tripping identity-provider rate limits; caching with strict isolation keeps the hot path fast and secure.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-cred-store`

### 5.7 Rate Limiting

#### Rate Limiting Enforcement

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-rate-limiting`

The system MUST enforce rate limits at upstream and route levels. Configuration MUST include a sustained rate with a time window, burst capacity, cost (tokens consumed per request), counter scope (global/tenant/user/IP/route), and an exceedance strategy. The system MUST support at least the reject strategy (rejects the request with a gateway-originated, retriable rate-limit condition that includes guidance on when to retry) and queue and degrade strategies. Enforcement MUST be per-instance in the data plane for MVP. Rate limits MUST respond to hierarchical merge rules (see Hierarchical Configuration): a descendant can only be subject to a limit no looser than the effective ancestor limit.

**Rationale**: Prevents abuse, cost overruns, and violations of external service agreements.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

> **ADR status**: Token-bucket dual-rate configuration, hierarchical budget allocation, and per-instance enforcement are REQUIRED (accepted ADR-0003). Distributed/global rate-limit synchronization across instances (e.g., a shared store with periodic sync) is OPTIONAL / future and is NOT required for the MVP.

### 5.8 CORS

#### Built-In CORS Handling

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-cors`

The system MUST provide built-in CORS handling configured per upstream and per route (enabled only when explicitly configured — disabled by default). Browser preflight requests MUST be answered locally by the gateway (fast path, without upstream resolution or per-request authentication/plugin processing, though global infrastructure-level controls still apply), echoing the requested origin, method, and headers. Actual cross-origin requests MUST have their origin and method validated against the upstream/route CORS configuration after upstream resolution and before forwarding, rejecting disallowed origins and methods with gateway-originated errors. Origin matching MUST be exact (no wildcard-pattern matching; port- and protocol-sensitive), and credential-bearing CORS (allowing credentials) MUST be rejected at configuration time when combined with a wildcard origin. CORS configuration MUST follow the hierarchical sharing modes (child origins unioned with parent on `inherit`; child cannot add origins on `enforce`).

**Rationale**: Enables browser-based clients to use OAGW securely; preflight must not round-trip to the upstream and CORS must be secure by default.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-platform-operator`

> **ADR status**: REQUIRED (accepted ADR-0004); implemented as core data-plane functionality, not as a bindable guard plugin.

### 5.9 Error Semantics

#### Consistent Error Conditions

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-error-codes`

The system MUST return a consistent, well-defined set of error conditions for proxy and management operations. The set MUST cover at least: validation errors, authentication failures, route-not-found, payload-too-large, rate-limit-exceeded, secret-not-found, downstream/upstream errors, service-unavailable (disabled upstream / link unavailable), circuit-breaker-open, and timeouts. Each condition MUST carry explicit retriable vs non-retriable semantics so clients can implement correct retry and fallback behavior, and MUST identify the failing resource (for example the upstream) where relevant.

**Rationale**: Consistent, well-defined error conditions enable clients to implement correct retry and fallback behavior.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Error Source Distinction

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-error-source-distinction`

The system MUST make it possible for every proxy response to distinguish whether the error originated from the gateway or from the upstream service, using a dedicated response indicator that works across all supported protocols (including streaming). Gateway-originated errors MUST use a structured, machine-readable problem-details format conforming to the RFC 9457 standard so clients can parse them programmatically. Upstream-originated errors MUST be passed through unchanged (unmodified body), labeled as upstream-originated.

**Rationale**: Clients cannot choose the correct handling strategy (retry, fallback, alert) without reliably knowing who produced the error; the distinction must survive across JSON, binary, and streaming responses.

**Actors**: `cpt-cf-oagw-actor-app-developer`

> **ADR status**: REQUIRED (accepted ADR-0007). The exact indicator mechanism and the RFC 9457 field layout belong to DESIGN/ADR, not this PRD.

### 5.10 Configuration Hierarchy

#### Configuration Layering

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-config-layering`

The system MUST merge configuration with the priority order: Upstream (base) less than Route less than Tenant (highest priority).

**Rationale**: Allows fine-grained configuration at each level without duplicating base settings.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Hierarchical Configuration Override

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-hierarchical-config`

The system MUST support hierarchical configuration override across the tenant hierarchy with three sharing modes per field: `private` (not visible to descendants; default), `inherit` (visible; a descendant with permission can override), and `enforce` (visible; a descendant cannot override). Override rules MUST be: for auth, a descendant with permission may use its own credentials only when sharing is `inherit`; for rate limits, a descendant can only be stricter (effective limit is the minimum of the ancestor-enforced limit and the descendant's limit); for plugins, a descendant's plugins append to the inherited chain and enforced plugins cannot be removed; for CORS, origins union on `inherit` and cannot be added on `enforce`. Tags MUST always use add-only union semantics across the hierarchy (descendants can add tags but cannot remove inherited tags). A descendant's ability to override MUST depend on permissions granted by ancestors; without permission, the descendant uses the ancestor's configuration as-is even under `inherit`.

**Rationale**: Enables partner/customer hierarchies where partners share upstream access with controlled credential, rate-limit, plugin, and CORS policies.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

## 6. Non-Functional Requirements

### 6.1 NFR Inclusions

#### Testability

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-testability`

The gear's behavior MUST be verified by an automated test suite (unit, integration, and e2e) achieving at least 90% code coverage. No functional or non-functional requirement in this PRD may ship without automated verification.

**Threshold**: 90%+ code coverage across unit, integration, and e2e suites.

**Rationale**: The gear is a middleware contract point — regression-free behavior is essential because every outbound call depends on it.

#### Build & Dependency Constraint

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-build-constraints`

The gear MUST compile and run within the workspace e2e feature set and MUST NOT add any new dependency beyond the workspace lockfile.

**Threshold**: Clean build under the e2e feature set; dependency set identical to the workspace lockfile.

**Rationale**: The workspace is a locked single-responsibility monorepo; new dependencies would break supply-chain and build determinism.

#### Low Latency

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-low-latency`

The system MUST add less than 10 ms of overhead at p95 for proxy requests (excluding upstream response time), and MUST enforce plugin execution timeouts so misbehaving plugins cannot inflate latency.

**Threshold**: <10 ms added latency at p95 (excluding upstream response time).

**Rationale**: OAGW is on the hot path for every outbound API call; excessive latency directly impacts end-user experience.

#### High Availability

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-high-availability`

The system MUST maintain 99.9% availability under normal operating conditions, and circuit breakers MUST prevent cascade failures from unhealthy upstreams.

**Threshold**: 99.9% uptime; circuit breaker trips within 5 failed requests in a 30-second window.

**Rationale**: OAGW is a critical-path component — if it is down, no outbound API calls succeed.

#### SSRF Protection

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-ssrf-protection`

The system MUST not be exploitable as a server-side request forgery vector: DNS resolution results MUST be validated, IP pinning rules MUST be enforced, well-known internal headers MUST be stripped, and request paths and query parameters MUST be validated against route configuration.

**Threshold**: Zero SSRF vulnerabilities in security audit.

**Rationale**: OAGW makes HTTP requests to external URLs — it must not be exploitable to reach internal infrastructure.

#### Credential Isolation

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-credential-isolation`

Credentials MUST never appear in logs, error messages, or API responses. All credential references MUST use pointers to the credential store (UUID/URI references), and credentials MUST be tenant-isolated.

**Threshold**: Zero credential exposure in any log, error, or API output.

**Rationale**: Credential leakage is a critical security risk; defense-in-depth requires isolation at every layer.

#### Input Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-input-validation`

The system MUST validate path, query parameters, headers, and body size for all inbound (management and proxy) requests; all malformed requests MUST be rejected as validation errors before any side effect or forwarding.

**Threshold**: All proxy requests validated before forwarding; 100% of malformed requests rejected.

**Rationale**: Prevents injection attacks and ensures only well-formed requests reach external services.

#### Observability

- [ ] `p2` - **ID**: `cpt-cf-oagw-nfr-observability`

The system MUST log all proxy requests with a correlation ID and MUST expose operational metrics for request counts, latencies, error rates, and rate limit state. Logs MUST NOT contain request/response bodies, query parameters, headers (except allowlisted ones), or credentials.

**Threshold**: 100% of proxy requests logged with a correlation ID; metrics exposing request counts, latencies, error rates, and rate-limit state.

**Rationale**: Operators need full visibility into outbound API traffic patterns, errors, and performance without leaking sensitive payload data.

#### Multi-tenancy

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-multi-tenancy`

All resources (upstreams, routes, plugins) MUST be tenant-scoped, and tenant isolation MUST be enforced at the data layer — no cross-tenant reads, writes, or proxies.

**Threshold**: Zero cross-tenant data access.

**Rationale**: The platform is multi-tenant middleware; strict tenant isolation is a fundamental security requirement.

#### Starlark Sandbox

- [ ] `p3` - **ID**: `cpt-cf-oagw-nfr-starlark-sandbox`

Custom Starlark plugins MUST run in a sandbox with no network I/O, no file I/O, and no imports, with enforced timeout and memory limits.

**Threshold**: Zero sandbox escapes; plugin execution timeout <= 100 ms; memory <= 10 MB per invocation.

**Rationale**: User-defined plugins must not compromise gateway security or stability.

### 6.2 NFR Exclusions

- **Accessibility** (UX-PRD-002): Not applicable because OAGW is an API-side infrastructure component with no user-facing interface (no UI to make accessible).
- **Internationalization** (UX-PRD-003): Not applicable because OAGW is an English-only, operator/developer-facing API service deployed in a single locale.
- **Safety** (SAFE-PRD-001/002): Not applicable because OAGW is a pure information system with no physical interaction and no potential for physical harm.
- **Regulatory Compliance** (COMPL-PRD-001/002/003): Not applicable because OAGW processes no PII, payment, or health data — upstream payloads are passed through transparently and are not inspected for personal data; audit logging covers accountability.
- **Offline capability** (UX-PRD-004): Not applicable because OAGW is an always-connected server-side proxy.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### Host Component Registration

- [ ] `p1` - **ID**: `cpt-cf-oagw-interface-host-registration`

**Type**: Host component registration (Constructor Fabric gears-rust workspace).

**Stability**: stable

**Description**: The gear registers itself as a host component of the Constructor Fabric host runtime, participating in lifecycle and dependency injection, and exposing its management and proxy capabilities under the `/oagw/v1` base path.

**Breaking Change Policy**: Registration contract follows workspace gear ABI conventions; a change to the registration contract requires a coordinated workspace release.

#### Management API

- [ ] `p1` - **ID**: `cpt-cf-oagw-interface-management-api`

**Type**: REST API (OpenAPI 3.0)

**Stability**: unstable

**Description**: CRUD operations for upstreams, routes, and plugins, including retrieval of custom plugin source content. Versioned (v1).

**Breaking Change Policy**: Major version bump required (v1 to v2). Version compatibility is part of the contract.

#### Proxy API

- [ ] `p1` - **ID**: `cpt-cf-oagw-interface-proxy-api`

**Type**: REST API

**Stability**: unstable

**Description**: Proxy capability that forwards requests to external services with credential injection and transformation, addressed by upstream alias with an optional path suffix and query parameters. Versioned (v1).

**Breaking Change Policy**: Major version bump required (v1 to v2). Version compatibility is part of the contract.

### 7.2 External Integration Contracts

#### Credential Store Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-cred-store`

**Direction**: required from client

**Protocol/Format**: In-process Rust trait call via the `cred_store` SDK

**Compatibility**: Must match the `cred_store` SDK version in the workspace. Provides secret material retrieval by reference and enforces tenant access (own or ancestor-shared secrets).

#### Types Registry Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-types-registry`

**Direction**: required from client

**Protocol/Format**: In-process Rust trait call via the `types_registry` SDK

**Compatibility**: Must match the `types_registry` SDK version in the workspace. Provides GTS schema/instance registration for plugin types and upstream/route type definitions.

#### Tenant Resolution Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-tenant-resolver`

**Direction**: required from client

**Protocol/Format**: In-process Rust trait call via the `tenant-resolver` SDK

**Compatibility**: Must match the workspace `tenant-resolver` SDK version. Resolves the tenant hierarchy; OAGW receives the tenant identity from the security context on every request.

#### Authentication & Authorization Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-authn-authz`

**Direction**: required from client

**Protocol/Format**: In-process Rust trait call via `toolkit-auth` / `authz-resolver` SDKs

**Compatibility**: Must match the workspace `toolkit-auth` / `authz-resolver` SDK versions. Provides authentication (bearer token) and permission evaluation for all OAGW requests using the permissions below.

**Authorization model (exact per-actor/operation permissions)**: The generic requirement that every protected capability requires authentication and authorization is assumed and not restated per requirement. Specific permissions:

- **Platform Operator** MAY create, read, override (replace), and delete upstreams and routes within the operator's own (root) tenant scope, and MAY create, read, and delete auth/guard/transform plugins. Requires permissions `gts.cf.core.oagw.upstream.v1~:{create;override;read;delete}`, `gts.cf.core.oagw.route.v1~:{create;override;read;delete}`, and `gts.cf.core.oagw.{auth|guard|transform}_plugin.v1~:{create;read;delete}`. MAY set `enforce`-mode sharing to constrain descendants.
- **Tenant Administrator** MAY perform the same create/read/override/delete operations, but strictly within their own tenant scope, and only subject to ancestor sharing modes and the specific override permissions an ancestor grants: `oagw:upstream:bind` (create a binding to an ancestor's upstream), `oagw:upstream:override_auth` (override auth config when sharing is `inherit`), `oagw:upstream:override_rate` (specify own rate limits, subject to strictness minimum), `oagw:upstream:add_plugins` (append own plugins to an inherited chain). A tenant administrator MUST NOT override `enforce`-mode configuration, MUST NOT view or modify ancestor-owned resources via the management API (ancestor resources are invisible/not-found), and MUST NOT re-enable an ancestor-disabled resource.
- **Application Developer** MAY invoke the proxy for upstreams owned by their tenant or shared by an ancestor. Requires permission `gts.cf.core.oagw.proxy.v1~:invoke`. MUST NOT perform management operations.
- Browser CORS preflight requests bypass per-request authentication and plugin checks by design but remain subject to infrastructure-level controls (global/edge rate limiting and WAF/DDoS protection).

## 8. Use Cases

#### Proxy HTTP Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Upstream and route are configured and enabled.
- Auth plugin and credentials are set up for the upstream.
- Caller holds proxy invoke permission.

**Main Flow**:
1. Developer sends a request to the proxy capability (`/api/oagw/v1/proxy/{alias}/{path}`).
2. System resolves the upstream by alias, searching the tenant hierarchy from descendant to root.
3. System matches the route by protocol, method, and path.
4. System merges configurations (upstream base, then route, then tenant).
5. System retrieves credentials from the credential store and executes the plugin chain (Auth, then Guards, then Transform on request).
6. System checks rate limits, forwards the request to the upstream, and applies response transforms.
7. System returns the upstream response to the caller with the error-source indicator.

**Postconditions**:
- Response from the external service returned to the caller; request logged with a correlation ID.

**Alternative Flows**:
- **Upstream not found**: route-not-found condition returned.
- **Upstream disabled**: gateway-originated service-unavailable condition returned.
- **Auth plugin fails**: authentication-failed condition returned.
- **Rate limit exceeded**: rate-limit condition with retry guidance returned per configured strategy.
- **Target endpoint unknown/invalid**: validation error returned.
- **Upstream timeout**: retriable timeout condition returned.

#### Configure Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-configure-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Preconditions**:
- Actor is authenticated with upstream create permission.

**Main Flow**:
1. Operator creates an upstream with server endpoints, protocol, and auth configuration.
2. System validates the configuration (endpoint format, alias derivation/uniqueness, credential reference validity, pool homogeneity).
3. System persists the upstream configuration.

**Postconditions**:
- Upstream is created and available for proxy routing.

**Alternative Flows**:
- **Validation fails**: validation error returned with details; nothing persisted.
- **Alias conflict**: alias conflict condition returned.
- **Alias would change on update**: alias immutability condition returned; operator must delete and re-create.

#### Configure Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-configure-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Preconditions**:
- Target upstream exists and is owned by the calling tenant.
- Actor is authenticated with route create permission.

**Main Flow**:
1. Operator creates a route referencing the upstream with match rules.
2. System validates the upstream reference and match-rule format/determinism.
3. System persists the route configuration.

**Postconditions**:
- Route is created and active for request matching.

**Alternative Flows**:
- **Upstream not owned/not found**: validation error returned.
- **Match rule conflicts with an existing route**: conflict condition returned.

#### Manage Custom Plugin

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-manage-plugin`

**Actor**: `cpt-cf-oagw-actor-tenant-admin`

**Preconditions**:
- Actor is authenticated with plugin create/read/delete permission in their tenant scope.

**Main Flow**:
1. Tenant administrator creates a custom plugin with a name, type, config schema, and source code.
2. System validates the plugin (schema type, identifier) and persists it atomically.
3. Administrator binds the plugin to an upstream or route.
4. When no longer referenced, administrator deletes the plugin.

**Postconditions**:
- Plugin is created, is immutable, and can be bound; unlinked plugins are garbage-collected after the retention period.

**Alternative Flows**:
- **Plugin referenced on delete**: "plugin in use" condition returned; deletion refused.
- **Plugin source retrieval**: administrator retrieves the source content of a custom plugin by identifier for audit.

#### Rate Limit Exceeded

- [ ] `p2` - **ID**: `cpt-cf-oagw-usecase-rate-limit-exceeded`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Rate limit is configured on the upstream or route.
- Client has exceeded the configured rate.

**Main Flow**:
1. Developer sends a proxy request.
2. System evaluates the rate-limit counter (per configured scope).
3. The limit is exceeded — the system applies the configured strategy.

**Postconditions**:
- Request handled per strategy (rejected with retry guidance, queued within bounded capacity, or degraded).

#### CORS Preflight

- [ ] `p2` - **ID**: `cpt-cf-oagw-usecase-cors-preflight`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- A browser-based client sends a cross-origin request through the proxy.

**Main Flow**:
1. Browser sends a preflight request (OPTIONS with an Origin and requested method/headers).
2. System detects the preflight and answers locally without upstream resolution or per-request auth.
3. System echoes the requested origin, method, and headers with permissive CORS semantics.

**Postconditions**:
- Preflight answered locally; origin/method enforcement deferred to the actual request.

**Alternative Flows**:
- **Actual request with disallowed origin/method**: rejected before forwarding with a gateway-originated CORS error.

#### SSE Streaming

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-sse-streaming`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Upstream supports SSE responses.
- Route is configured for the target path.

**Main Flow**:
1. Developer sends a proxy request to an SSE endpoint.
2. System establishes the upstream connection.
3. System forwards SSE events as received to the client.
4. Connection lifecycle is managed (open/close/error).

**Postconditions**:
- All events forwarded; connection cleanly closed.

**Alternative Flows**:
- **Upstream closes the connection**: System closes the client connection and logs the event.
- **Client disconnects**: System closes the upstream connection.

## 9. Acceptance Criteria

- [ ] The gear builds and starts within the workspace e2e feature set, registers with the host runtime, and serves its management and proxy capabilities under `/oagw/v1` per the contract, with no new dependencies beyond the workspace lockfile.
- [ ] The management API supports full CRUD for upstreams, routes, and plugins: valid payloads are accepted; malformed payloads, alias conflicts/derivation violations, upstream-pool homogeneity violations, and match-rule conflicts are rejected with appropriate conditions; all operations are tenant-scoped with ancestor `enforce` constraints respected.
- [ ] Proxy requests are forwarded to configured upstreams with alias resolution (including shadowing), route matching, multi-endpoint target selection, and credential injection; responses are returned with error semantics that allow a client to distinguish gateway-originated from upstream-originated errors.
- [ ] Disabled upstreams and routes behave per contract (requests blocked / routes excluded from matching), and an ancestor-disabled resource cannot be re-enabled by a descendant.
- [ ] Rate limiting is enforced at upstream and route levels per configuration; rate-limit-exceeded responses include retry guidance.
- [ ] CORS behaves per contract: preflight handled locally; actual cross-origin requests validated against configured origins and methods; configuration-time rejection of credential-bearing wildcard origins.
- [ ] SSE streaming proxies events with correct connection lifecycle handling including upstream-close and client-disconnect paths.
- [ ] No credentials appear in any log, error message, or API response.
- [ ] The automated test suite (unit, integration, e2e) covers the above behavior with at least 90% code coverage.

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Host runtime (Constructor Fabric gears-rust workspace) | Gear lifecycle, dependency injection, REST API hosting, single-executable deployment | p1 |
| `api_ingress` (ToolKit) | REST API hosting for the management and proxy APIs | p1 |
| `toolkit-auth` / `authz-resolver` | Authentication (bearer token) and authorization permission evaluation for all OAGW requests | p1 |
| `tenant-resolver` | Tenant hierarchy resolution; tenant identity supplied via the security context | p1 |
| `cred_store` | Secret material retrieval by reference for auth-plugin credential injection | p1 |
| `types_registry` | GTS schema/instance registration for plugin types and upstream/route type definitions | p1 |
| `toolkit-db` | Database persistence for upstream, route, and plugin configurations | p1 |
| Gear configuration (`oagw` section in `config/e2e-local.yaml`) | Runtime settings: proxy timeout, SSRF policy, upstream transport policy | p2 |

## 11. Assumptions

- **MVP scope**: single-executable deployment; rate limiting is per-instance (data plane owns in-memory limiters); configuration lookups use in-memory caching. Distributed rate-limit sync (e.g., shared store with periodic sync) and an L2 (shared) cache layer are OPTIONAL / future per accepted ADRs and are NOT required for the MVP.
- **No automatic retries**: OAGW does not re-issue failed client requests; retry is the client's responsibility. Connector-level endpoint/connection failover and connection-retry attempts are permitted.
- **No response caching**: OAGW does not cache upstream responses; caching is the client's/upstream's responsibility.
- **Credential store availability**: the `cred_store` gear is available and supports retrieval by reference, with secrets tenant-isolated or shared to descendants via credential-store policies.
- **Tenant hierarchy**: resolved by the platform; OAGW receives the tenant identity from the security context.
- **Upstream reachability**: external upstream services are reachable over HTTP/HTTPS from the deployment environment; production upstream connections are HTTPS-only (plaintext only under explicit test configuration).
- **Catalog-only plugin identifiers** (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) are present in the type catalog but have no bindable implementation; references to them fail resolution. This is intended behavior, not a defect.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| SSRF exploitation (DNS rebinding, internal hostnames, header smuggling) | High — internal infrastructure reachable | Validate DNS resolution, enforce IP pinning, strip well-known internal headers, validate paths/query against routes, HTTPS-only upstreams in production (NFR: SSRF protection) |
| Alias shadowing / collisions across the tenant hierarchy | High — requests misrouted to the wrong upstream; accidental cross-tenant exposure | Per-tenant alias uniqueness, descendant-to-root resolution with closest-match-wins, enforced ancestor limits across shadowing, alias immutability (delete/re-create on change) |
| Catalog-only plugin identifiers referenced in configuration | Medium — operator confusion and failing bindings for `basic`/`bearer`/`timeout`/`cors`/`logging`/`metrics` | Document catalog-only status; fail resolution deterministically with an "unknown plugin" condition; validate bindings at write time where possible |
| Deleting referenced plugins blocked ("plugin in use") | Medium — abandoned plugins accumulate; operational friction | Refuse deletion with a clear in-use condition; automatic garbage collection of unlinked plugins after retention period |
| Streaming complexity (SSE/WebSocket/WebTransport lifecycle) | Medium — connection leaks, partial event delivery, resource exhaustion | Strict open/close/error lifecycle handling on both legs (client and upstream), timeout enforcement, dedicated e2e coverage |
| Rate-limit state loss on restart | Medium — brief window of unlimited requests | Persist rate-limit counters where feasible; accept a bounded burst on cold start |
| Credential store unavailability | High — proxy requests fail when secrets cannot be resolved | OAuth2 tokens already cached continue to be served until TTL; alert on credential-store health |
| Upstream service outage cascading to callers | High — all consumers of that upstream blocked | Per-upstream circuit breaker, configurable timeouts, retriable error conditions with retry guidance |
| Custom Starlark plugins escaping sandbox or abusing resources | High — gateway compromise or instability | Sandbox with no network/file I/O and no imports; enforced timeout and memory limits per invocation (NFR: Starlark sandbox) |
| Distributed rate-limit/cache infrastructure unavailable (if later enabled) | Medium — reduced global accuracy | Fall back to local-only enforcement / in-memory cache with degraded accuracy (per accepted ADRs) |

## Traceability

- **Gear specification source**: `docs/PRD.md`, `docs/DESIGN.md`, `docs/ADR/0001`..`0009`, `docs/schemas/*.json` under `gears/system/oagw/`.
- **Design**: `gears/system/oagw/docs/DESIGN.md`
- **ADRs**: `gears/system/oagw/docs/ADR/`
