---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# Plugin System — Three Plugin Types with Trait-Based Extensibility

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Plugin Types](#plugin-types)
  - [Plugin Trait Surfaces](#plugin-trait-surfaces)
  - [Execution Order](#execution-order)
  - [GTS Identifier Resolution and Built-in Plugins](#gts-identifier-resolution-and-built-in-plugins)
  - [External Plugins](#external-plugins)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Three plugin types with separate traits](#three-plugin-types-with-separate-traits)
  - [Single generic Extension trait](#single-generic-extension-trait)
  - [Starlark-only interpreted plugins](#starlark-only-interpreted-plugins)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-plugin-system`

**Priority**: p2

## Context and Problem Statement

OAGW needs extensibility for request/response processing. Different use cases require different behaviors: authentication (API key, OAuth2, JWT), validation (timeouts, CORS, rate limiting), and transformation (logging, metrics, request ID). The question is how to structure the plugin system to support both built-in and external plugins with clear boundaries.

## Decision Drivers

* Clear trait boundaries for each plugin purpose
* Same traits for built-in and external plugins (no special-casing)
* ToolKit integration for external plugins
* Native Rust performance (no WASM overhead for MVP)
* Compile-time type safety for built-in plugins
* Deterministic execution order

## Considered Options

* Three plugin types with separate traits (Auth, Guard, Transform)
* Single generic Extension trait for all purposes
* Starlark-only interpreted plugins

## Decision Outcome

Chosen option: "Three plugin types with separate traits", because it provides clear semantic boundaries, type safety, and deterministic execution order.

### Plugin Types

**AuthPlugin** (`gts.cf.core.oagw.auth_plugin.v1~*`): Injects authentication credentials. Executed once per request, before guards. Examples: API key, OAuth2, Bearer token, Basic auth.

**GuardPlugin** (`gts.cf.core.oagw.guard_plugin.v1~*`): Validates requests and enforces policies (can reject). Executed after auth, before transform. Examples: required header enforcement, timeout enforcement, CORS validation, rate limiting.

**TransformPlugin** (`gts.cf.core.oagw.transform_plugin.v1~*`): Modifies request/response/error data. Executed before and after proxy call. Examples: logging, metrics collection, request ID propagation.

### Plugin Trait Surfaces

Each plugin family exposes a dedicated async trait surface with an `id()` accessor, a `plugin_type()` accessor returning the GTS identifier, and phase-specific methods:

| Trait | Phase Methods |
|---|---|
| `AuthPlugin` | `authenticate(ctx)` — credential injection, executes once per request before guards |
| `GuardPlugin` | `guard_request(ctx)` and `guard_response(ctx)` — validation/policy enforcement, can reject |
| `TransformPlugin` | `transform_request(ctx)`, `transform_response(ctx)`, and `transform_error(ctx)` — mutation of request, response, and error data |

### Execution Order

The plugin chain executes in a fixed order: Auth plugins (credential injection), then Guard plugins (validation, can reject), then Transform plugins (request modification), followed by the HTTP call to the external service, then Transform plugins (response modification), and finally the response is returned to the client. Upstream plugins execute before route plugins. Plugin definitions are immutable after creation; updates are performed by creating a new plugin version and re-binding references.

### GTS Identifier Resolution and Built-in Plugins

Plugins are resolved through registries keyed by GTS identifier (`AuthPluginRegistry`, `GuardPluginRegistry`, `TransformPluginRegistry`). Built-ins shipped in the `oagw` crate include:

**Auth Plugins**: `ApiKeyAuthPlugin` (API key injection via header/query), `NoopAuthPlugin` (no authentication), and `OAuth2ClientCredAuthPlugin` registered twice (Form and Basic client-auth-method variants — see [ADR: OAuth2 Client Credentials Auth Plugin](./0008-oauth2-client-credentials-auth-plugin.md)). `cf.core.oagw.basic.v1` and `cf.core.oagw.bearer.v1` are reserved GTS identifiers cataloged in the types-registry with no backing `AuthPlugin` implementation.

**Guard Plugins**: `RequiredHeadersGuardPlugin` (required header enforcement for request/response). Request timeout and CORS are core Data Plane logic, not `GuardPlugin` trait implementations (see [ADR: CORS](./0004-cors.md)); their GTS identifiers (`cf.core.oagw.timeout.v1`, `cf.core.oagw.cors.v1`) exist for types-registry cataloging only and are not resolvable via `GuardPluginRegistry`.

**Transform Plugins**: `RequestIdTransformPlugin` (X-Request-ID propagation). Request/response logging and Prometheus metrics collection are core Data Plane instrumentation, not `TransformPlugin` trait implementations; their GTS identifiers (`cf.core.oagw.logging.v1`, `cf.core.oagw.metrics.v1`) are catalog-only and not resolvable via `TransformPluginRegistry`.

### External Plugins

External plugins are separate ToolKit gears that implement the same `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` traits. Because external plugins implement the identical trait surfaces as built-ins, no special-casing is required at any layer; the Data Plane registers external plugins alongside built-ins during initialization.

### Consequences

* Good, because extensibility without modifying OAGW core
* Good, because built-in plugins have zero overhead (native code)
* Good, because external plugins integrate via ToolKit (standard pattern)
* Good, because clear execution order and lifecycle
* Bad, because external plugins require Rust implementation (no scripting languages yet)
* Bad, because plugin changes require recompilation (acceptable for MVP)

### Confirmation

Code review confirms: the `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` traits are defined; built-in implementations live in the plugin infrastructure module; external plugins register via ToolKit dependency injection.

## Pros and Cons of the Options

### Three plugin types with separate traits

Separate `AuthPlugin`, `GuardPlugin`, and `TransformPlugin` traits with specific phase methods.

* Good, because each type has a clear purpose and lifecycle
* Good, because compile-time type safety for plugin implementations
* Good, because deterministic execution order (Auth → Guard → Transform)
* Bad, because three traits to maintain instead of one

### Single generic Extension trait

One `Extension` trait with generic hooks for all purposes.

* Good, because a simpler trait hierarchy
* Bad, because loses type safety and clear semantics
* Bad, because execution ordering becomes configuration-dependent
* Bad, because harder to reason about plugin interactions

### Starlark-only interpreted plugins

All plugins as interpreted Starlark scripts.

* Good, because no recompilation for plugin changes
* Good, because sandboxed execution
* Bad, because too slow for hot path operations (auth, guards)
* Bad, because limited expressiveness for complex auth flows

## More Information

Future enhancements:
- Starlark plugins (p3): for simple transforms that do not need native performance
- WASM plugins (p3): for sandboxed untrusted code

The circuit breaker is a core gateway resilience capability (configured as core policy), not a plugin.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-plugin-system` — Plugin system architecture with three plugin types
* `cpt-cf-oagw-fr-builtin-plugins` — Built-in plugin implementations and binding semantics
* `cpt-cf-oagw-fr-auth-injection` — Auth plugins handle credential injection
* `cpt-cf-oagw-fr-rate-limiting` — Guard plugins enforce rate limits
