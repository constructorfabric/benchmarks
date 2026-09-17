# PRD — Outbound API Gateway (OAGW)

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
  - [5.1 Core Management](#51-core-management)
  - [5.2 Proxy Execution](#52-proxy-execution)
  - [5.3 Plugin System](#53-plugin-system)
  - [5.4 Streaming](#54-streaming)
  - [5.5 Configuration Hierarchy](#55-configuration-hierarchy)
  - [5.6 Error Handling](#56-error-handling)
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

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

The Outbound API Gateway (OAGW) manages all outbound API requests from application gears to external services. It acts as a centralized proxy layer that handles credential injection, rate limiting, header transformation, and security enforcement for every external call made by the platform.

OAGW provides a unified interface for application gears to reach external APIs without managing credentials, connection details, or security policies directly. Gears send requests to OAGW's proxy interface addressed by an upstream alias, and OAGW resolves the target upstream, injects authentication, applies policies, executes the plugin chain, and forwards the request.

### 1.2 Background / Problem Statement

Gears need to communicate with external third-party services (e.g., OpenAI, Stripe, payment gateways). Without a centralized gateway, each gear must independently manage credentials, rate limits, error handling, and security policies for outbound calls. This leads to credential sprawl, inconsistent error handling, and no unified observability.

OAGW solves these problems by providing a single outbound proxy layer with pluggable authentication, configurable rate limiting, header transformation, and security policies. All external calls flow through OAGW, ensuring consistent credential isolation, audit trails, and policy enforcement across the platform.

**Key Problems Solved**:

- Credential sprawl and exposure of secrets in application code, logs, and error messages
- Inconsistent error handling across gears when external services fail
- No unified observability or cost control over outbound API traffic
- No centralized enforcement of security policies (SSRF protection, header validation)

### 1.3 Goals (Business Outcomes)

- Centralize all outbound API credential management — zero credential exposure in application code, logs, or error messages
- Provide a unified proxy interface for external services with consistent error handling and observability
- Enforce rate limiting and security policies (SSRF protection, header validation) to prevent abuse and cost overruns
- Support multi-tenant hierarchical configuration with sharing, inheritance, and enforcement semantics

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Upstream | External service target defined by server endpoints (scheme/host/port), protocol, authentication configuration, default headers, and rate limits |
| Route | API path on an upstream that matches requests by method, path, and query allowlist. Routes map inbound proxy requests to specific upstream behaviors |
| Plugin | Modular processor attached to upstreams or routes. Three types: Auth (credential injection), Guard (validation/policy enforcement), Transform (request/response mutation) |
| Data Plane | Internal component that orchestrates proxy requests: resolves configuration, executes plugin chains, and forwards HTTP calls to external services |
| Control Plane | Internal component that manages configuration data (upstreams, routes, plugins) |
| Alias | Short identifier used in proxy requests to reference an upstream. Auto-derived from hostname for hostname-based endpoints (a user-provided alias that differs from the auto-derived value is rejected (400); providing the exact derived value is tolerated silently as an idempotent no-op); explicit alias required for IP-based or non-derivable endpoints. Normalized to ASCII lowercase; resolution is case-insensitive |
| Sharing Mode | Configuration visibility setting for hierarchical tenancy: `private` (owner only), `inherit` (descendants can override), `enforce` (descendants cannot override) |
| GTS | Global Type System — the platform's schema and instance registration system used for plugin type identification |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by steering committee. Document **actors** (users, systems) that interact with this module.

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-oagw-actor-platform-operator`

**Role**: Manages global configuration: upstreams, routes, system-wide plugins, and security policies. Owns platform-level visibility and enforcement across the tenant hierarchy.

**Needs**: CRUD operations for upstreams, routes, and plugins; ability to enforce configuration on descendant tenants; visibility into all proxy traffic and errors.

**Permissions**:
- `gts.cf.core.oagw.upstream.v1~:create|override|read|delete` on any tenant in the hierarchy
- `gts.cf.core.oagw.route.v1~:create|override|read|delete` on any tenant in the hierarchy
- `gts.cf.core.oagw.{auth_plugin|guard_plugin|transform_plugin}.v1~:create|read|delete` (bind built-in and external plugins, including plugin source retrieval)
- May set sharing mode to `enforce` on any configuration so descendants cannot override it

#### Tenant Administrator

**ID**: `cpt-cf-oagw-actor-tenant-admin`

**Role**: Manages tenant-specific settings: credentials, rate limits, custom plugins, and configuration overrides within allowed sharing policies.

**Needs**: Override inherited configurations where permitted; manage tenant-scoped credentials; set stricter rate limits for their tenant hierarchy.

**Permissions**:
- `gts.cf.core.oagw.upstream.v1~:create|override|read|delete` and `gts.cf.core.oagw.route.v1~:create|override|read|delete` within their own tenant only
- `gts.cf.core.oagw.{auth_plugin|guard_plugin|transform_plugin}.v1~:create|read|delete` for tenant-scoped plugins; enforced plugins inherited from ancestors cannot be removed
- May override ancestor configurations whose sharing mode is `inherit` (only where permission allows); cannot override `enforce` configurations and cannot re-enable an ancestor-disabled resource

#### Application Developer

**ID**: `cpt-cf-oagw-actor-app-developer`

**Role**: Consumes external APIs via the OAGW proxy interface without managing credentials or external service details.

**Needs**: A simple proxy request addressed by upstream alias with transparent credential injection; clear error responses that distinguish gateway errors from upstream errors and indicate retriability.

**Permissions**: Proxy invocation only — no management permissions on upstreams, routes, or plugins; receives only the response to their own proxied requests and the defined error semantics.

### 2.2 System Actors

#### Credential Store

**ID**: `cpt-cf-oagw-actor-cred-store`

**Role**: Secure storage and retrieval of secrets (API keys, OAuth tokens, passwords) by UUID reference. OAGW never stores credentials directly — it references them via `cred_store`.

#### Types Registry

**ID**: `cpt-cf-oagw-actor-types-registry`

**Role**: GTS schema and instance registration and validation. OAGW registers its plugin type schemas and upstream/route type definitions in the types registry.

#### Upstream Service

**ID**: `cpt-cf-oagw-actor-upstream-service`

**Role**: External third-party service (e.g., OpenAI, Stripe) that OAGW proxies requests to. OAGW treats upstream services as opaque HTTP endpoints.

## 3. Operational Concept & Environment

> **Note**: Project-wide runtime, OS, architecture, lifecycle policy, and integration patterns defined in root PRD. Document only module-specific deviations here.

### 3.1 Module-Specific Environment Constraints

None. OAGW follows standard Gears ToolKit gear conventions; all project-wide runtime, deployment, lifecycle, and integration constraints defined in the root PRD apply unchanged.

## 4. Scope

### 4.1 In Scope

- Management of upstreams, routes, and plugins (create, read, update, delete)
- HTTP/HTTPS proxy with alias-based upstream resolution and route matching
- Credential injection via auth plugins (API key, HTTP Basic, OAuth2 client credentials, bearer token)
- Rate limiting at upstream and route levels with configurable strategies
- Header transformation (set/add/remove, hop-by-hop stripping, passthrough control)
- Plugin system with three types: Auth, Guard, Transform (built-in and external)
- Streaming support: HTTP request/response, SSE, WebSocket, WebTransport
- Multi-tenant hierarchical configuration with sharing modes (private/inherit/enforce)
- Alias resolution with shadowing across the tenant hierarchy
- Error semantics that distinguish gateway errors from upstream errors
- Metrics collection and audit logging

### 4.2 Out of Scope

- DNS resolution and IP pinning rule implementation details
- Plugin versioning and lifecycle management details
- Response caching (client/upstream responsibility)
- Automatic request retries (client responsibility)
- gRPC proxying (planned for a later phase)

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches (analysis, inspection, demonstration).

### 5.1 Core Management

#### Upstream Management

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-upstream-mgmt`

The system **MUST** provide create, read, update, and delete operations for upstream configurations. Each upstream defines server endpoints (scheme/host/port), protocol, authentication configuration, default headers, and rate limits. All operations are tenant-scoped.

**Rationale**: Upstreams are the fundamental configuration unit — every proxy request targets an upstream.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Route Management

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-route-mgmt`

The system **MUST** provide create, read, update, and delete operations for routes. Routes define matching rules (HTTP method, path pattern, query parameter allowlist) that map inbound proxy requests to specific upstreams.

**Rationale**: Routes control which requests reach which upstream endpoints and with what transformations.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Enable/Disable Semantics

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-enable-disable`

The system **MUST** support an enabled/disabled state (default: enabled) on upstreams and routes. A disabled upstream **MUST** cause all proxy requests targeting it to be rejected with a service-unavailable error. A disabled route **MUST** be excluded from route matching. If an ancestor tenant disables an upstream, it **MUST** remain disabled for all descendants, and descendants **MUST NOT** be able to re-enable an ancestor-disabled resource.

**Rationale**: Enables temporary maintenance, emergency circuit breaks, and gradual rollouts without deleting configuration.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

### 5.2 Proxy Execution

#### Request Proxying

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-request-proxy`

The system **MUST** proxy requests from application gears to external services through the outbound proxy interface (`cpt-cf-oagw-interface-proxy-api`), addressed by upstream alias with an optional path and query. For each request the system **MUST** resolve the upstream by alias, match the route by method and path, merge configurations (upstream < route < tenant), execute the plugin chain, and forward the request to the external service. The system **MUST NOT** automatically re-issue an entire client request; full-client retries are the caller's responsibility. Connector-level endpoint failover or connection-retry attempts (endpoint-level retries) performed by the upstream connector are permitted.

**Rationale**: Core value proposition — a unified proxy interface that handles credential injection, transformation, and forwarding transparently.

**Actors**: `cpt-cf-oagw-actor-app-developer`

#### Authentication Injection

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-auth-injection`

The system **MUST** inject credentials into outbound requests via auth plugins. Supported authentication methods: API Key (header or query), HTTP Basic Auth, OAuth2 Client Credentials, and Bearer Token. Credentials **MUST** be retrieved from the credential store at request time by UUID reference.

**Rationale**: Centralizes credential management so application developers never handle API keys or tokens directly.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-cred-store`

#### Rate Limiting

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-rate-limiting`

The system **MUST** enforce rate limits at upstream and route levels. Configuration **MUST** include: rate, window, capacity, cost, scope (global, tenant, user, IP, or route, defaulting to `tenant`), and strategy (reject with communicated retry timing, queue, or degrade). When the reject strategy is applied, the response **MUST** communicate retry timing to the caller.

**Rationale**: Prevents abuse, cost overruns, and protects external service agreements.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Header Transformation

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-header-transform`

The system **MUST** transform request and response headers: set, add, and remove operations; passthrough control; and automatic stripping of hop-by-hop headers (Connection, Keep-Alive, Proxy-Authenticate, Proxy-Authorization, TE, Trailer, Transfer-Encoding, Upgrade).

**Rationale**: Ensures clean outbound requests and prevents header leakage between internal and external networks.

**Actors**: `cpt-cf-oagw-actor-app-developer`

### 5.3 Plugin System

#### Plugin System

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-plugin-system`

The system **MUST** provide a plugin system with three plugin types: Auth (`gts.cf.core.oagw.auth_plugin.v1~*`) for credential injection, Guard (`gts.cf.core.oagw.guard_plugin.v1~*`) for validation/policy enforcement (can reject requests), and Transform (`gts.cf.core.oagw.transform_plugin.v1~*`) for request/response mutation. Execution order **MUST** be: Auth, then Guards, then request Transform, then the upstream call, then response/error Transform. Upstream plugins **MUST** execute before route plugins. Plugin definitions **MUST** be immutable after creation; updates are performed by creating a new plugin version and re-binding references. The circuit breaker is a core gateway resilience capability (configured as core policy), not a plugin.

**Rationale**: Extensibility for custom authentication schemes, validation rules, and request/response transformations without modifying the gateway core.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Built-in Plugins

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-builtin-plugins`

The system **MUST** include the following built-in plugins with their binding semantics:

**Auth Plugins** (type `gts.cf.core.oagw.auth_plugin.v1~*`):
- No-op — no authentication
- API key — key injection via header or query
- OAuth2 client credentials — client credentials flow
- OAuth2 client credentials with Basic auth — client credentials flow using Basic auth
- HTTP Basic — catalog identifier only; no backing Auth implementation, not bindable for execution
- Bearer token — catalog identifier only; no backing Auth implementation, not bindable for execution

**Guard Plugins** (type `gts.cf.core.oagw.guard_plugin.v1~*`):
- Required headers — required header enforcement (request/response); the only guard plugin bindable via plugin references
- Request timeout — core Data Plane configuration; catalog identifier only, not bindable via plugin references
- CORS preflight — core Data Plane configuration via upstream CORS settings; catalog identifier only, not bindable via plugin references

**Transform Plugins** (type `gts.cf.core.oagw.transform_plugin.v1~*`):
- Request ID — X-Request-ID propagation for correlation
- Logging — request/response logging; core Data Plane instrumentation, catalog identifier only (not resolvable via the Transform plugin registry)
- Metrics — metrics collection; core Data Plane instrumentation, catalog identifier only (not resolvable via the Transform plugin registry)

**Rationale**: Covers the most common outbound API authentication and observability patterns out of the box.

**Actors**: `cpt-cf-oagw-actor-platform-operator`

### 5.4 Streaming

#### Streaming Support

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-streaming`

The system **MUST** support HTTP request/response proxying and SSE (Server-Sent Events) streaming with proper connection lifecycle handling (open, close, error). The system **MUST** support WebSocket and WebTransport session flows.

**Rationale**: Many external APIs (e.g., OpenAI chat completions) use SSE for streaming responses; WebSocket/WebTransport needed for bidirectional real-time protocols.

**Actors**: `cpt-cf-oagw-actor-app-developer`

### 5.5 Configuration Hierarchy

#### Configuration Layering

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-config-layering`

The system **MUST** merge configurations with the following priority order: Upstream (base) < Route < Tenant (highest priority).

**Rationale**: Allows fine-grained configuration at each level without duplicating base settings.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Hierarchical Configuration Override

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-hierarchical-config`

The system **MUST** support hierarchical configuration override across tenant hierarchies with three sharing modes:

| Mode | Behavior |
|------|----------|
| `private` | Not visible to descendants (default) |
| `inherit` | Visible; descendant can override if specified |
| `enforce` | Visible; descendant cannot override |

Override rules:

- **Auth**: With `sharing: inherit`, a descendant with permission can use its own credentials
- **Rate limits**: Descendants can only be stricter — effective rate is the minimum of the ancestor's enforced rate and the descendant's rate
- **Plugins**: Descendant plugins append to ancestor plugins; enforced plugins cannot be removed

Tags **MUST NOT** have a sharing mode — they always use add-only union semantics: effective tags are the union of ancestor and descendant tags. Descendants can add tags but cannot remove inherited tags. If upstream creation resolves to an existing upstream definition (binding-style flow), request tags are treated as tenant-local additions for effective discovery; they **MUST NOT** mutate ancestor tags.

**Rationale**: Enables partner/customer hierarchies where partners share upstream access with controlled credential and rate limit policies.

**Actors**: `cpt-cf-oagw-actor-platform-operator`, `cpt-cf-oagw-actor-tenant-admin`

#### Alias Resolution and Shadowing

- [ ] `p2` - **ID**: `cpt-cf-oagw-fr-alias-resolution`

The system **MUST** identify upstreams by alias in the outbound proxy interface. Alias is **enforced** based on endpoint type: hostname-based endpoints always auto-derive the alias (a user-provided alias that differs from the auto-derived value is rejected (400); providing the exact derived value is tolerated silently as an idempotent no-op); IP-based or non-derivable endpoints require an explicit alias. Derivation rules: a single hostname uses the hostname (without port for standard ports); multiple hostnames use the longest common domain suffix (at least two labels), validated against the public suffix list to reject bare public suffixes (e.g., `co.uk`); IP addresses or an absence of a common suffix require an explicit alias. Aliases **MUST** be normalized to ASCII lowercase with trailing dots stripped; resolution **MUST** be case-insensitive. When resolving an alias, the system **MUST** search the tenant hierarchy from descendant to root; the closest match wins (descendant shadows ancestor). Enforced limits from ancestors **MUST** still apply across shadowing. Multiple endpoints within the same upstream form a load-balancing pool; endpoints in a pool **MUST** share identical protocol, scheme, and port.

**Rationale**: Provides human-readable proxy requests while supporting multi-tenant isolation and override semantics.

**Actors**: `cpt-cf-oagw-actor-app-developer`, `cpt-cf-oagw-actor-platform-operator`

### 5.6 Error Handling

#### Error Semantics

- [ ] `p1` - **ID**: `cpt-cf-oagw-fr-error-codes`

The system **MUST** return consistent, well-defined error outcomes for proxy and management operations, and **MUST** distinguish gateway errors from upstream errors. Error categories **MUST** include: validation (malformed or invalid input), authentication failed, route not found, payload too large, rate limit exceeded, secret not found, downstream error (upstream failure), circuit breaker open, and timeout. The system **MUST** communicate for each error outcome whether the caller may retry: rate limit exceeded, circuit breaker open, and timeout are retriable; validation, authentication failed, route not found, payload too large, and secret not found are not retriable. Rejection responses under the rate-limit reject strategy **MUST** include retry timing for the caller.

**Rationale**: Consistent, well-defined error semantics enable clients to implement correct retry and fallback behavior.

**Actors**: `cpt-cf-oagw-actor-app-developer`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs (performance, security, reliability, scalability) defined in root PRD. Document only gear-specific NFRs here: **exclusions** from defaults or **standalone** requirements.
>
> **Testing strategy**: NFRs verified via automated benchmarks, security scans, and monitoring unless otherwise specified.

### 6.1 NFR Inclusions

#### Low Latency

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-low-latency`

The system **MUST** add less than 10ms overhead at p95 for proxy requests. Plugin execution timeouts **MUST** be enforced.

**Threshold**: <10ms added latency at p95 (excluding upstream response time)

**Rationale**: OAGW is on the hot path for every outbound API call; excessive latency directly impacts end-user experience.

#### High Availability

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-high-availability`

The system **MUST** maintain 99.9% availability. Circuit breakers **MUST** prevent cascade failures from unhealthy upstreams.

**Threshold**: 99.9% uptime; circuit breaker trips within 5 failed requests in a 30s window

**Rationale**: OAGW is a critical path component — if it is down, no outbound API calls succeed.

#### SSRF Protection

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-ssrf-protection`

The system **MUST** validate DNS resolution results, enforce IP pinning rules, strip well-known internal headers, and validate request paths and query parameters against route configuration.

**Threshold**: Zero SSRF vulnerabilities in security audit

**Rationale**: OAGW makes HTTP requests to external URLs — it must not be exploitable as an SSRF vector.

#### Credential Isolation

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-credential-isolation`

Credentials **MUST** never appear in logs, error messages, or API responses. All credential references **MUST** use UUID pointers to the credential store. Credentials **MUST** be tenant-isolated.

**Threshold**: Zero credential exposure in any log, error, or API output

**Rationale**: Credential leakage is a critical security risk; defense-in-depth requires isolation at every layer.

#### Input Validation

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-input-validation`

The system **MUST** validate path, query parameters, headers, and body size for all inbound requests. Invalid requests **MUST** be rejected.

**Threshold**: All proxy requests validated before forwarding; 100% of malformed requests rejected

**Rationale**: Prevents injection attacks and ensures only well-formed requests reach external services.

#### Observability

- [ ] `p2` - **ID**: `cpt-cf-oagw-nfr-observability`

The system **MUST** log all proxy requests with correlation IDs, expose Prometheus metrics for request counts, latencies, error rates, and rate limit state, and maintain an audit trail of management operations and configuration changes.

**Threshold**: 100% of proxy requests logged with correlation ID; metrics scraped from the metrics endpoint; all management operations recorded in the audit trail

**Rationale**: Operators need full visibility into outbound API traffic patterns, errors, and performance.

#### Starlark Sandbox

- [ ] `p3` - **ID**: `cpt-cf-oagw-nfr-starlark-sandbox`

Custom Starlark plugins **MUST** run in a sandbox with no network I/O, no file I/O, no imports, and enforced timeout and memory limits.

**Threshold**: Zero sandbox escapes; plugin execution timeout ≤ 100ms; memory ≤ 10MB per invocation

**Rationale**: User-defined plugins must not compromise gateway security or stability.

#### Multi-tenancy

- [ ] `p1` - **ID**: `cpt-cf-oagw-nfr-multi-tenancy`

All resources (upstreams, routes, plugins) **MUST** be tenant-scoped. Tenant isolation **MUST** be enforced at the data layer.

**Threshold**: Zero cross-tenant data access

**Rationale**: Gears is a multi-tenant middleware; strict tenant isolation is a fundamental security requirement.

### 6.2 NFR Exclusions

All project-default NFRs apply to this gear and are not restated here, including throughput/capacity targets, recovery targets (RPO/RTO), deployment and rollback standards, and documentation requirements — as defined in the root PRD.

None. All project-default NFRs apply to this gear.

## 7. Public Library Interfaces

Define the public API surface, versioning/compatibility guarantees, and integration contracts provided by this library.

### 7.1 Public API Surface

#### Management API

- [ ] `p1` - **ID**: `cpt-cf-oagw-interface-management-api`

**Type**: REST API (OpenAPI 3.0)

**Stability**: unstable

**Description**: Management interface providing create, read, update, and delete operations for upstreams, routes, and plugins, including retrieval of plugin source content. Operations are tenant-scoped and enforced per the actor permission model in Section 2.

**Breaking Change Policy**: Major version bump required (v1 to v2)

#### Proxy API

- [ ] `p1` - **ID**: `cpt-cf-oagw-interface-proxy-api`

**Type**: REST API

**Stability**: unstable

**Description**: Outbound proxy interface addressed by upstream alias with an optional path and query. Accepts application requests and forwards them to external services with credential injection, rate limiting, header transformation, and plugin policy execution; supports streaming and SSE responses.

**Breaking Change Policy**: Major version bump required (v1 to v2)

### 7.2 External Integration Contracts

Contracts this gear expects from external systems or provides to downstream consumers.

#### Credential Store Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-cred-store`

**Direction**: required from client (OAGW consumes)

**Protocol/Format**: In-process Rust trait call via the `cred_store` SDK; UUID-based secret retrieval

**Compatibility**: Must match the `cred_store` SDK version in the workspace

#### Types Registry Contract

- [ ] `p1` - **ID**: `cpt-cf-oagw-contract-types-registry`

**Direction**: required from client (OAGW consumes; OAGW registers plugin type schemas and upstream/route type definitions)

**Protocol/Format**: In-process Rust trait call via the `types_registry` SDK

**Compatibility**: Must match the `types_registry` SDK version in the workspace

## 8. Use Cases

#### Proxy HTTP Request

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Upstream and route are configured and enabled
- Auth plugin and credentials are set up for the upstream

**Main Flow**:
1. App sends a request to the outbound proxy interface addressing an upstream alias, with optional path and query
2. System resolves upstream by alias (tenant hierarchy search)
3. System matches route by method and path
4. System merges configurations (upstream < route < tenant)
5. System retrieves credentials from the credential store and transforms the request
6. System executes the plugin chain (Auth, then Guard, then Transform)
7. System forwards the request to the upstream and returns the response

**Postconditions**:
- Response from the external service returned to the caller
- Request logged with correlation ID

**Alternative Flows**:
- **Upstream not found / alias unresolvable**: Return route-not-found error
- **Upstream disabled**: Return service-unavailable gateway error
- **Auth plugin fails**: Return authentication-failed error
- **Rate limit exceeded (reject strategy)**: Return rate-limit-exceeded error with retry timing
- **Upstream timeout**: Return timeout error

#### Configure Upstream

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-configure-upstream`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Preconditions**:
- Actor holds `gts.cf.core.oagw.upstream.v1~:create` permission (or `override`/`delete` as applicable)
- Tenant context is resolved

**Main Flow**:
1. Operator submits an upstream definition (server endpoints, protocol, auth configuration, default headers, rate limits)
2. System validates configuration (endpoint format, alias derivation/uniqueness, credential reference validity)
3. System persists the upstream configuration

**Postconditions**:
- Upstream is created and available for proxy routing; visible to descendants per its sharing mode

**Alternative Flows**:
- **Validation fails**: Return validation error with details
- **Alias conflict**: Return conflict error

#### Configure Route

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-configure-route`

**Actor**: `cpt-cf-oagw-actor-platform-operator`

**Preconditions**:
- Target upstream exists
- Actor holds `gts.cf.core.oagw.route.v1~:create` permission (or `override`/`delete` as applicable)

**Main Flow**:
1. Operator submits a route definition referencing an upstream, with match rules (method, path, query allowlist)
2. System validates the upstream reference and match rule format
3. System persists the route configuration

**Postconditions**:
- Route is created and active for request matching

**Alternative Flows**:
- **Upstream not found**: Return validation error
- **Validation fails**: Return validation error with details

#### Rate Limit Exceeded

- [ ] `p2` - **ID**: `cpt-cf-oagw-usecase-rate-limit-exceeded`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Rate limit is configured on the upstream or route
- Client has exceeded the configured rate

**Main Flow**:
1. App sends a proxy request
2. System evaluates the rate limit counter
3. Rate limit is exceeded — system applies the configured strategy

**Postconditions**:
- Request handled per strategy (rejected, queued, or degraded)

**Alternative Flows**:
- **Strategy: reject**: Return rate-limit-exceeded error with retry timing
- **Strategy: queue**: Request queued for later execution within bounded capacity
- **Strategy: degrade**: Request processed with reduced functionality

#### SSE Streaming

- [ ] `p1` - **ID**: `cpt-cf-oagw-usecase-sse-streaming`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Preconditions**:
- Upstream supports SSE responses
- Route is configured for the target path

**Main Flow**:
1. App sends a proxy request to an SSE endpoint
2. System establishes a connection to the upstream
3. System forwards SSE events to the client as received
4. Connection lifecycle is managed (open, close, error)

**Postconditions**:
- All events forwarded; connection cleanly closed

**Alternative Flows**:
- **Upstream closes connection**: System closes the client connection and logs the event
- **Client disconnects**: System closes the upstream connection

## 9. Acceptance Criteria

Business-level acceptance criteria for the PRD as a whole.

- [ ] Proxy requests complete with <10ms added latency at p95
- [ ] Zero credential exposure in any log output, error message, or API response
- [ ] 99.9% availability under normal operating conditions
- [ ] Complete audit trail for all proxy requests (correlation ID, timestamps, status) and management operations
- [ ] All upstream, route, and plugin management operations validated and tenant-scoped
- [ ] Rate limiting enforced per configuration; rejection responses include retry timing
- [ ] SSE streaming proxies events with correct lifecycle handling
- [ ] Zero cross-tenant access to upstreams, routes, plugins, or credentials
- [ ] All error outcomes distinguish gateway errors from upstream errors and communicate retriability per `cpt-cf-oagw-fr-error-codes`

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `types_registry` | GTS schema/instance registration for plugin types and upstream/route type definitions | p1 |
| `cred_store` | Secret material retrieval by UUID reference for auth plugin credential injection | p1 |
| `api_ingress` | REST API hosting via the ToolKit framework | p1 |
| `toolkit-db` | Database persistence for upstream, route, and plugin configurations | p1 |
| `toolkit-auth` | Platform authentication and SecurityContext extraction for authorization on all management operations | p1 |

## 11. Assumptions

- Gears `cred_store` gear is available and supports UUID-based secret retrieval
- ToolKit framework provides gear lifecycle, dependency injection, and REST API hosting
- Tenant hierarchy is resolved by the platform (tenant-resolver gear); OAGW receives the tenant context from the SecurityContext
- External upstream services are reachable via HTTP/HTTPS from the Gears deployment environment
- Platform-level authentication (identity, session, MFA where applicable) follows platform-wide defaults via `toolkit-auth`; per-operation authorization follows the permission model in Section 2
- Throughput, capacity, and recovery targets follow root PRD project defaults
- Permissions follow the GTS naming convention `gts.cf.core.oagw.<resource>.v1~:<action>` — `{create;override;read;delete}` for upstream/route resources and `{create;read;delete}` for plugin resources (`auth_plugin`, `guard_plugin`, `transform_plugin`) — consistent with the management surface
- Open questions deferred to DESIGN/steering for resolution: maximum number of endpoints per upstream pool; plugin garbage collection policy (time-based vs reference-count-based); audit log retention period

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Upstream service outage cascading to OAGW callers | High — all consumers of that upstream blocked | Circuit breaker pattern; configurable timeout and fallback behavior |
| Credential store unavailability | High — proxy requests fail if credentials cannot be retrieved | Cache last-known-good credentials with short TTL; alert on credential store health |
| Rate limit state loss on restart | Medium — brief window of unlimited requests | Persist rate limit counters; accept brief burst on cold start |
| Plugin execution exceeding timeout | Medium — increased proxy latency | Enforced plugin timeout; circuit-break misbehaving plugins |
| Misconfigured sharing modes or alias shadowing causing cross-tenant exposure | High — tenant A reaches tenant B's upstream or credentials | Strict validation of sharing semantics and alias resolution order; enforcement rules verified by automated tests; full audit trail of configuration changes |
