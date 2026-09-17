---
status: accepted
date: 2026-08-27
decision-makers: Constructor Fabric Steering Committee
---

# Required Headers Guard Plugin — Request/Response Header Enforcement

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Plugin Configuration (`ctx.config` keys)](#plugin-configuration-ctxconfig-keys)
  - [Decision Flow](#decision-flow)
  - [Registry Integration](#registry-integration)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Dedicated builtin `GuardPlugin`](#dedicated-builtin-guardplugin)
  - [Per-upstream custom Starlark guard plugin](#per-upstream-custom-starlark-guard-plugin)
  - [Push the check into `Upstream.headers` passthrough config](#push-the-check-into-upstreamheaders-passthrough-config)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-oagw-adr-required-headers-guard-plugin`

**Priority**: p9

## Context and Problem Statement

Many upstreams need to enforce the presence of specific headers before a request reaches them — for example, requiring a correlation ID or an API version header on inbound requests. Symmetrically, operators sometimes need to reject upstream responses that omit an expected header (e.g., `Content-Type`) as a defense against a misbehaving or compromised upstream. Without a built-in plugin for this common pattern, each operator would have to write and maintain a bespoke Starlark guard (per [ADR: Plugin System](./0002-plugin-system.md)) for what is, in most cases, a simple presence check.

## Decision Drivers

* This is a common validation pattern that recurs across many upstreams and should not require a custom Starlark guard each time
* The check must be opt-in per upstream and fail-open when unconfigured — an upstream that does not configure the plugin should see no behavior change
* Both request and response phases are legitimate use cases and should be handled symmetrically by the same plugin, independently configurable

## Considered Options

* Dedicated builtin `GuardPlugin`
* Per-upstream custom Starlark guard plugin
* Push the check into `Upstream.headers` passthrough config

## Decision Outcome

Chosen option: "Dedicated builtin `GuardPlugin`". A **stateless** `RequiredHeadersGuardPlugin` checks for the presence of configured header names on the request (before proxying to the upstream) and/or on the upstream's response (before returning to the caller), rejecting with a phase-specific status code on the first missing header.

### Plugin Configuration (`ctx.config` keys)

| Key | Required | Description |
|---|---|---|
| `required_request_headers` | No | Comma-separated header names checked in the request phase; absent or blank (after trimming each entry) → the phase is a no-op |
| `required_response_headers` | No | Comma-separated header names checked in the response phase; absent or blank → the phase is a no-op |

Both keys are independent — configuring one does not affect the other phase. Header names are matched **case-insensitively**; only **presence** is checked, not header values.

### Decision Flow

The guard reads the corresponding list from config; if the list is absent or blank it allows (fail-open, unconfigured). Otherwise it splits on commas, trims, lowercases, drops empty entries, and scans the headers for each required name in order. If all are present it allows; on the **first missing** name it rejects:

- Request phase → status `400`, error code `REQUIRED_HEADER_MISSING`
- Response phase → status `502`, error code `REQUIRED_HEADER_MISSING`

Only the first missing header is reported per rejection, not the full set.

### Registry Integration

The plugin is stateless — no cache, no security-sensitive material. It is registered in `GuardPluginRegistry::with_builtins()` under its GTS identifier (`cf.core.oagw.required_headers.v1`). It is currently the only entry there: the timeout and CORS GTS identifiers are declared for the types-registry catalog, but their enforcement is core Data Plane logic (see [ADR: CORS](./0004-cors.md)) rather than `GuardPlugin` trait implementations.

### Consequences

#### Positive

- Good, because a common validation need is covered without writing a per-upstream custom Starlark guard
- Good, because request and response enforcement are both covered by the same plugin, independently configurable
- Good, because fail-open on absent/blank config means adding the plugin to the registry has no effect on upstreams that do not opt in
- Good, because the plugin is stateless — trivial to reason about and test

#### Negative

- Bad, because only header *presence* is validated, not header *values* — an upstream needing value validation (e.g., a specific API version) still needs a custom guard
- Bad, because only the first missing header is reported per rejection, which can require multiple round-trips to discover all missing headers

#### Risks

- **Misconfigured comma list**: an all-blank or empty config value (e.g., `", , ,"`) silently no-ops rather than erroring — an operator expecting enforcement from a malformed config string will not be warned.

### Confirmation

Code review confirms: `RequiredHeadersGuardPlugin` is implemented as a stateless guard in the plugin infrastructure module, registered in `GuardPluginRegistry::with_builtins()` under the `cf.core.oagw.required_headers.v1` GTS identifier.

## Pros and Cons of the Options

### Dedicated builtin `GuardPlugin`

A stateless plugin shipped in the `oagw` crate and registered by default.

* Good, because it requires zero setup per upstream beyond config — no code to write or deploy
* Good, because it is covered by the same test and review process as other builtin plugins
* Bad, because it only covers presence checks — anything more elaborate still needs a custom guard

### Per-upstream custom Starlark guard plugin

Each operator writes their own guard using the plugin mechanism.

* Good, because it is fully flexible — any check, including value validation, is possible
* Bad, because it duplicates the same presence-check logic across every upstream that needs it
* Bad, because it pushes maintenance and testing burden onto each operator for a pattern that is almost always identical

### Push the check into `Upstream.headers` passthrough config

Extend the existing header passthrough/rewrite configuration to also support "required" markers.

* Good, because it avoids introducing a new plugin type
* Bad, because it conflates header *forwarding* concerns with header *validation* concerns in a single config surface
* Bad, because it does not naturally extend to response-phase enforcement, which is a header validation concern independent of passthrough

## More Information

Out of scope: **header value validation** — matching a header against a regex, allow-list, or expected value. Only presence is checked today.

Future considerations:
- **Header value validation**: extending the config to support value constraints per header, not just presence
- **Reporting all missing headers**: returning the full set of missing header names in the rejection detail instead of only the first

Related ADRs:
- [ADR: Plugin System](./0002-plugin-system.md) — `GuardPlugin` trait and execution model
- [ADR: CORS](./0004-cors.md) — contrasting precedent: CORS is core Data Plane logic rather than a `GuardPlugin` trait implementation, for preflight fast-path reasons that do not apply here

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-oagw-fr-builtin-plugins` — Built-in plugin implementation for required header enforcement
* `cpt-cf-oagw-fr-plugin-system` — Guard plugin trait and execution model integration
* `cpt-cf-oagw-fr-error-codes` — Phase-specific rejection status codes (`400` request / `502` response)
