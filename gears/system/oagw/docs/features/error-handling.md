# Feature: Error Handling

- [x] `p2` - **ID**: `cpt-cf-oagw-featstatus-error-handling-implemented`

<!-- reference to DECOMPOSITION entry -->
- [x] `p2` - `cpt-cf-oagw-feature-error-handling`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gateway Error Rendering](#gateway-error-rendering)
  - [Upstream Error Passthrough](#upstream-error-passthrough)
  - [Target-Host Routing Error Rendering](#target-host-routing-error-rendering)
  - [Disabled Upstream Rejection](#disabled-upstream-rejection)
  - [Payload Too Large Rejection](#payload-too-large-rejection)
  - [Validation Failure Rendering](#validation-failure-rendering)
  - [Timeout and Downstream Error Rendering](#timeout-and-downstream-error-rendering)
  - [Streaming Error Source Marking](#streaming-error-source-marking)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [DomainError to Problem-Details Mapping](#domainerror-to-problem-details-mapping)
  - [Retriability and Retry Guidance Classification](#retriability-and-retry-guidance-classification)
  - [Error Source Assignment](#error-source-assignment)
  - [Validation Failure Rendering](#validation-failure-rendering-1)
  - [Problem-Details Serialization](#problem-details-serialization)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Problem-Details Contract](#problem-details-contract)
  - [Design Error-Table Mapping](#design-error-table-mapping)
  - [PRD Error-Code Table Mapping](#prd-error-code-table-mapping)
  - [Error Source Header](#error-source-header)
  - [Target-Host Routing Error Bodies](#target-host-routing-error-bodies)
  - [Disabled Upstream 503 Mapping](#disabled-upstream-503-mapping)
  - [Payload Too Large 413 Mapping](#payload-too-large-413-mapping)
  - [Validation Failure Rendering](#validation-failure-rendering-2)
  - [Retriability and Retry Guidance Metadata](#retriability-and-retry-guidance-metadata)
  - [DomainError Taxonomy and Mapping Layer](#domainerror-taxonomy-and-mapping-layer)
  - [Trace Identifier Propagation](#trace-identifier-propagation)
  - [Upstream Error Passthrough](#upstream-error-passthrough-1)
  - [Automated Unit Test Coverage](#automated-unit-test-coverage)
  - [Automated Integration Test Coverage](#automated-integration-test-coverage)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Defines the single error contract applied to every OAGW endpoint: RFC 9457 `application/problem+json` gateway errors carrying GTS error types under `gts.cf.core.errors.err.v1~cf.oagw.*`, the complete DESIGN §3.3 error-table mapping, the `X-OAGW-Error-Source: gateway|upstream` distinction between gateway-generated and upstream-passthrough failures, the retriable and non-retriable classification, and the rendering of validation failures as 4xx problem+json. The feature adds no endpoint and no route; it supplies the error mapping that every `/oagw/v1/...` endpoint registered by the foundation consumes.

### 1.2 Purpose

Without one error contract, each entry would invent its own failure shape and clients could neither distinguish a gateway fault from an upstream fault nor decide whether to retry. This feature owns error rendering and nothing else: enforcement of request validation and of the body-size limit is owned by `cpt-cf-oagw-feature-request-proxy`, and the observability metrics and logging surface is owned by `cpt-cf-oagw-feature-observability-and-operability`. The `429` outcome is split the same way: `cpt-cf-oagw-feature-rate-limiting` (entry 2.7) owns the rate-limit decision, the `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` headers, and the retry-guidance value, while this entry renders the resulting `429` `application/problem+json` body, its `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` type, and the `Retry-After` / `retry_after_seconds` pair taken from that decision, and emits no `X-RateLimit-*` header of its own. It realizes the component slice of `cpt-cf-oagw-component-model` that carries the error mapping layer (`api/rest/error.rs`), the `DomainError` taxonomy (`domain/error.rs`), the `X-OAGW-Error-Source` response-header emission, and problem-details serialization with GTS type identifiers and OAGW extension fields. The error contract is expressed against the proxy path and is exercised by the proxy outcomes documented in `cpt-cf-oagw-seq-proxy-flow`, whose error branch this feature renders.

Delivered by this feature:

- `p1` - `cpt-cf-oagw-fr-error-codes`
- [x] `p1` - `cpt-cf-oagw-nfr-input-validation` - rendering slice only; enforcement is owned by entry 2.4 and the CORS-configuration validation slice by entry 2.8
- [x] `p2` - `cpt-cf-oagw-nfr-observability` - the error-surface slice only: `trace_id` propagation into problem+json bodies and `error_type` attribution; metrics and logging are owned by entry 2.9

**Principles**: `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

**Constraints**: `cpt-cf-oagw-constraint-body-limit` produces the `413` mapping delivered here but is enforced in entry 2.4; this feature introduces no DESIGN constraint of its own.

**Sequences**: none. `cpt-cf-oagw-seq-proxy-flow` is the only sequence DESIGN.md defines and is owned by entry 2.4; error rendering is a branch of that flow.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Consumes every gateway error and every passthrough upstream error; reads the GTS `type`, the retriable classification, and `X-OAGW-Error-Source` to decide retry and fallback behavior |
| `cpt-cf-oagw-actor-upstream-service` | Origin of passthrough errors; its status, headers, content type, and body are forwarded unmodified with `X-OAGW-Error-Source: upstream` |
| `cpt-cf-oagw-actor-platform-operator` | Owns the error surface as an operability contract; correlates failures through `trace_id` and the GTS `type` carried by the problem+json body |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **ADR**: [ADR/0007-error-source-distinction.md](../ADR/0007-error-source-distinction.md)
- **Dependencies**: `cpt-cf-oagw-feature-request-proxy`. The error contract is exercised by the proxy path (timeouts, upstream failures, target-host validation, streaming aborts, the disabled-upstream outcome, and the payload-limit outcome) and must be verified against real proxy outcomes. Entries 2.6, 2.7, and 2.8 raise errors that this contract renders, and entry 2.9 consumes the `trace_id` and `error_type` fields it produces.

**Applicability**: the requirement domains this entry does not carry are excluded explicitly rather than left unaddressed:

- **Performance (PERF)**: not applicable because the error path adds one in-process mapping pass and owns no latency budget of its own; the proxy latency budget of `cpt-cf-oagw-nfr-low-latency` is owned by `cpt-cf-oagw-feature-request-proxy` (entry 2.4).
- **Security (SEC)**: not applicable as an enforcement concern, because authentication and authorization enforcement is owned by entries 2.4 and 2.6 and this feature renders their outcomes; the only security obligation carried here is the negative one — no rendered body, `detail` string, or extension member may leak credential material.
- **Operations (OPS)**: not applicable because no configuration, no health check, and no rollout step is introduced here; the configuration model is delivered by `cpt-cf-oagw-feature-gear-foundation` (entry 2.1) and the health and metrics surface is owned by `cpt-cf-oagw-feature-observability-and-operability` (entry 2.9).
- **Compliance (COMPL-001, COMPL-002)**: not applicable because no regulatory regime, no data-retention obligation, and no audited control applies to an error-rendering contract.
- **User experience (UX-002)**: not applicable because the surface is an HTTP error body consumed by a client developer; there is no human-facing UI journey for this entry to cover.
- **Maintainability (MAINT-003)**: not applicable because no technical-debt item is introduced; the contract is one mapping layer with no alternative implementation path.
- **Architecture — extension points (ARCH-007)**: not claimed as an owned extension point; sibling entries extend this contract only through the member and type sets named in section 3, and never by adding a second mapping path.

## 2. Actor Flows (CDSL)

**Use cases**: this feature exposes no end-user use case of its own. `cpt-cf-oagw-usecase-proxy-request`, `cpt-cf-oagw-usecase-sse-streaming`, and `cpt-cf-oagw-usecase-rate-limit-exceeded` all terminate in the error contract rendered here, and the 429 body of the rate-limit use case is produced by this feature's mapping layer from the decision made in entry 2.7.

### Gateway Error Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-gateway-render`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A `DomainError` raised by any component on the proxy path or on a management path is rendered as one problem+json body whose status, GTS type, title, detail, instance, and extension fields come from the single mapping layer, with `X-OAGW-Error-Source: gateway`.

**Error Scenarios**:
- No upstream was resolved before the failure, so `upstream_id` and `host` are unknown: those extension fields are omitted and the remaining contract fields are still present.
- The failure originates on a management endpoint rather than the proxy path: the same contract applies with `instance` naming the management path.
- A passthrough upstream body reaches the rendering layer: it is refused as gateway-renderable material and handed to the passthrough flow instead.
- The failure is the `429` refusal decided by `cpt-cf-oagw-feature-rate-limiting` (entry 2.7): the body is rendered here as the rate-limit branch of this flow, taking the guidance value from that decision and emitting no `X-RateLimit-*` header, because the quota headers stay owned by entry 2.7.

**Steps**:
1. [x] - `p1` - A component on the proxy path or on a management path returns a `DomainError` describing what failed, where it failed, and the request context available at the failure point - `inst-eh-render-1`
2. [x] - `p1` - Map the `DomainError` variant to its HTTP status, GTS error type, and retriable flag through the single mapping layer rather than at the call site - `inst-eh-render-2`
3. [x] - `p1` - Collect the occurrence context: the resolved `upstream_id`, the target `host`, the request `path`, and the `trace_id` - `inst-eh-render-3`
4. [x] - `p1` - **IF** the failure is a response received from the upstream service rather than a gateway decision - `inst-eh-render-4`
   1. [x] - `p1` - Hand the response to the upstream passthrough flow and render no problem+json body for it - `inst-eh-render-5`
5. [x] - `p1` - Serialize the body with the five RFC 9457 members plus every known OAGW extension member and set `Content-Type: application/problem+json` - `inst-eh-render-6`
6. [x] - `p1` - Emit `X-OAGW-Error-Source: gateway` on the response - `inst-eh-render-7`
7. [x] - `p1` - **IF** the mapped type is retriable and carries retry guidance - `inst-eh-render-8`
   1. [x] - `p1` - Emit `Retry-After` and the `retry_after_seconds` extension member with the same value - `inst-eh-render-9`
8. [x] - `p1` - **RETURN** the rendered error response carrying the status code from the mapping - `inst-eh-render-10`
   1. [x] - `p1` - **IF** the returned response is the rate-limit branch, that is the `429` refusal decided by `cpt-cf-oagw-feature-rate-limiting` - `inst-eh-render-11`
      1. [x] - `p1` - Render it as `429` with `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`, take the `Retry-After` and `retry_after_seconds` pair from that decision, and emit no `X-RateLimit-Limit`, `X-RateLimit-Remaining`, or `X-RateLimit-Reset` header from this contract - `inst-eh-render-12`

### Upstream Error Passthrough

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-upstream-passthrough`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- An upstream response carrying an error status is returned to the client with its status, headers, content type, and body unmodified, and with `X-OAGW-Error-Source: upstream`.

**Error Scenarios**:
- The upstream connection fails before a complete response is received: the failure is gateway-originated and is rendered through the gateway contract instead.
- A stream aborts after response headers were already sent: the source header already emitted is preserved and the outcome is classified as a stream abort rather than silently truncating the stream.
- The upstream returns a body in a format OAGW does not understand: it is still passed through unchanged, because passthrough never inspects or transforms the body.

**Steps**:
1. [x] - `p1` - Receive the upstream response carrying an error status from the upstream connector - `inst-eh-passthru-1`
2. [x] - `p1` - Classify the response as upstream-originated and select `X-OAGW-Error-Source: upstream` - `inst-eh-passthru-2`
3. [x] - `p1` - Forward the upstream status code, headers, content type, and body unmodified, adding no field, rewriting no content type, and applying no problem+json rendering - `inst-eh-passthru-3`
4. [x] - `p1` - **IF** the upstream connection fails before a complete response is received - `inst-eh-passthru-4`
   1. [x] - `p1` - Classify the failure as gateway-originated and render it through the gateway error contract instead - `inst-eh-passthru-5`
5. [x] - `p1` - **IF** a streaming response aborts after its headers were sent - `inst-eh-passthru-6`
   1. [x] - `p1` - Preserve the source header value already emitted, terminate the stream, and classify the outcome as `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` - `inst-eh-passthru-7`
6. [x] - `p1` - **RETURN** the passthrough response with the upstream body left unmodified - `inst-eh-passthru-8`

### Target-Host Routing Error Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-target-host-render`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request whose `X-OAGW-Target-Host` header is missing where it is required, malformed, or does not match a configured endpoint returns a 400 problem+json body carrying the corresponding routing type and exactly the extension members that body owns, per `cpt-cf-oagw-adr-error-source-distinction` Appendix A: the missing-header body carries `upstream_id`, `alias`, and `valid_hosts`; the invalid-format body carries `upstream_id` and `invalid_value`; the unknown-host body carries `upstream_id`, `invalid_value`, and `valid_hosts`; and all three carry `trace_id`. No routing body carries all of the members at once.

**Error Scenarios**:
- No upstream resolves for the requested alias: the target-host family does not apply and the failure is rendered as the route-matching or link error instead.
- The offending header value carries a port, a path, or special characters: the invalid-format body is returned and only the malformed header value is echoed in `invalid_value`, never any credential-bearing header value.

**Steps**:
1. [x] - `p1` - Receive the target-host routing failure from the proxy path with the resolved upstream identifier, the requested alias, the offending header value when one was sent, and the configured endpoint host list - `inst-eh-th-1`
2. [x] - `p1` - **IF** the header is absent and the resolved upstream requires it - `inst-eh-th-2`
   1. [x] - `p1` - Map to 400 with `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` and populate `upstream_id`, `alias`, `valid_hosts`, and `trace_id` - `inst-eh-th-3`
3. [x] - `p1` - **IF** the header value is present but is not a bare hostname or IP address - `inst-eh-th-4`
   1. [x] - `p1` - Map to 400 with `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` and populate `upstream_id`, `invalid_value`, and `trace_id` - `inst-eh-th-5`
4. [x] - `p1` - **IF** the header value is well formed but matches no configured endpoint - `inst-eh-th-6`
   1. [x] - `p1` - Map to 400 with `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` and populate `upstream_id`, `invalid_value`, `valid_hosts`, and `trace_id` - `inst-eh-th-7`
5. [x] - `p1` - Emit `X-OAGW-Error-Source: gateway` with `Content-Type: application/problem+json` on every body in this family - `inst-eh-th-8`
6. [x] - `p1` - **RETURN** the 400 problem+json body whose `detail` names the valid hosts so the client can correct the request - `inst-eh-th-9`
   1. [x] - `p1` - Bound the echoed `invalid_value` to at most 128 characters with no control character before it is placed in any response member, emit it only through the JSON serializer, never concatenate it into `detail` or into a header value, and limit the echo to the value of the `X-OAGW-Target-Host` request header, never to any other request header or request body content - `inst-eh-th-10`

### Disabled Upstream Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-disabled-upstream`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A proxy request whose resolved upstream is disabled returns `503` with `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`, `X-OAGW-Error-Source: gateway`, and retriable metadata, because DESIGN §3.3 defines no dedicated GTS type for the PRD's disabled-upstream outcome.

**Error Scenarios**:
- The `503` originates from an open circuit breaker or from an unresolvable plugin reference rather than a disabled upstream: the status is the same but the GTS type differs, so the three 503 outcomes stay distinguishable.
- The upstream does not exist at all rather than being disabled: the failure is a resolution failure and is not rendered as link-unavailable.

**Steps**:
1. [x] - `p1` - Receive the disabled-upstream outcome from the proxy path with the resolved upstream identifier and the requested alias - `inst-eh-dis-1`
2. [x] - `p1` - Map the outcome to 503 with `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` and `X-OAGW-Error-Source: gateway` - `inst-eh-dis-2`
3. [x] - `p1` - Classify the type as retriable and emit no upstream-derived body content - `inst-eh-dis-3`
4. [x] - `p1` - Populate `upstream_id`, `host` when known, `path`, and `trace_id` - `inst-eh-dis-4`
5. [x] - `p1` - **IF** the 503 originates from an open circuit breaker or from an unresolvable plugin reference - `inst-eh-dis-5`
   1. [x] - `p1` - Map to `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` or to `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` so the three 503 outcomes remain distinguishable by type - `inst-eh-dis-6`
6. [x] - `p1` - **RETURN** the 503 problem+json response carrying retriable metadata - `inst-eh-dis-7`

### Payload Too Large Rejection

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-payload-too-large`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request body that exceeds the configured limit is rejected with 413 `payload.too_large.v1`, `X-OAGW-Error-Source: gateway`, and no retry guidance, before the request is forwarded or fully buffered.

**Error Scenarios**:
- The rejection is a body-shape or membership failure rather than a payload-size failure, such as an unparsable Content-Length or an unsupported Transfer-Encoding: it is rendered as 400 `validation.error.v1` instead of 413.
- An implementation buffers the body before applying the limit: the outcome is the same 413, but the buffering itself violates the enforcement owned by entry 2.4 and is reported by that entry's tests.

**Steps**:
1. [x] - `p1` - Receive the payload-limit rejection raised by the proxy path before buffering completes - `inst-eh-pl-1`
2. [x] - `p1` - Map the rejection to 413 with `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` and `X-OAGW-Error-Source: gateway` - `inst-eh-pl-2`
3. [x] - `p1` - Classify the type as non-retriable and emit no `Retry-After` header and no `retry_after_seconds` member - `inst-eh-pl-3`
4. [x] - `p1` - Populate `path`, `trace_id`, and `upstream_id` when an upstream was resolved, and state the limit in `detail` without echoing any request body content - `inst-eh-pl-4`
5. [x] - `p1` - **IF** the rejection is a body-shape or membership failure rather than a payload-size failure - `inst-eh-pl-5`
   1. [x] - `p1` - Map it to 400 with `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` instead of 413 - `inst-eh-pl-6`
6. [x] - `p1` - **RETURN** the 413 problem+json response with no upstream call having been made - `inst-eh-pl-7`

### Validation Failure Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-validation-render`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- Every validation failure enforced on the proxy path - a method outside the route allowlist, an unknown query parameter, a rejected path suffix under `path_suffix_mode: disabled`, an invalid well-known header, a Content-Length integrity failure, or an unsupported Transfer-Encoding - is rendered as a 4xx problem+json body with `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and `X-OAGW-Error-Source: gateway`. A CORS origin or CORS method rejection is rendered through the same contract but with its own `403` type from entry 2.8, which sits outside the DESIGN §3.3 table.

**Error Scenarios**:
- Enforcement passes and the upstream rejects the request: the response is upstream passthrough and is not rendered as a validation failure.
- The failing check belongs to the CORS origin or CORS method rules: the body is rendered through this contract using the CORS rejection types owned by entry 2.8, `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` or `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, at status `403` rather than `400`.

**Steps**:
1. [x] - `p1` - Accept the validation failure set produced by the enforcement owned by entry 2.4 and perform no validation here - `inst-eh-val-1`
2. [x] - `p1` - Render each failure as 400 with `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and `X-OAGW-Error-Source: gateway` - `inst-eh-val-2`
3. [x] - `p1` - Write `detail` naming the first failing check and the offending field, and keep credential-bearing header values and request body content out of the body - `inst-eh-val-3`
4. [x] - `p1` - Classify the type as non-retriable, because resending an unmodified request fails again - `inst-eh-val-4`
5. [x] - `p1` - **IF** the failing check is a payload-size failure rather than a body-shape or membership failure - `inst-eh-val-5`
   1. [x] - `p1` - Render 413 with `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` instead of 400 - `inst-eh-val-6`
6. [x] - `p1` - **IF** the failing check belongs to the CORS origin or CORS method rules - `inst-eh-val-7`
   1. [x] - `p1` - Render through this contract with status `403` and the type owned by entry 2.8 for that check, `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` or `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, so the response shape does not fork per rejection source - `inst-eh-val-8`
7. [x] - `p1` - **RETURN** the rendered 4xx problem+json response with the request left unforwarded - `inst-eh-val-9`

### Timeout and Downstream Error Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-timeout-downstream`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- Connection, request, and idle timeouts render as 504 with their own retriable types; protocol-level failures render as 502, a stream abort occurring before response headers are produced renders as 502, and upstream service errors render as 502 `downstream.error.v1` with retriability decided per occurrence.

**Error Scenarios**:
- The upstream returned a complete error response rather than failing the transport: the response is passthrough and is not classified as a downstream error.
- The transport failure occurs on a streaming session after headers were sent: the outcome is `stream.aborted.v1` with the source header value already emitted.

**Steps**:
1. [x] - `p1` - Receive the transport-failure classification from the proxy path: connection-establishment timeout, overall request timeout, idle stream timeout, protocol-level failure, or stream abort - `inst-eh-td-1`
2. [x] - `p1` - Map the three timeouts to 504 with `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`, `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`, and `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`, each retriable - `inst-eh-td-2`
3. [x] - `p1` - Map a protocol-level failure to 502 with `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` and a stream abort that occurs BEFORE response headers are produced to 502 with `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, both non-retriable; an abort after headers were sent is classified as `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` for attribution only, with no status or body change - `inst-eh-td-3`
4. [x] - `p1` - Map an upstream service error to 502 with `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` and take its retriable flag from the error's own metadata - `inst-eh-td-4`
5. [x] - `p1` - Emit `X-OAGW-Error-Source: gateway` with `Content-Type: application/problem+json` on every rendered body in this family - `inst-eh-td-5`
6. [x] - `p1` - Populate `upstream_id`, `host`, `path`, and `trace_id` so the failing endpoint is identifiable - `inst-eh-td-6`
7. [x] - `p1` - **IF** the upstream returned a complete error response instead of failing the transport - `inst-eh-td-7`
   1. [x] - `p1` - Route the response to the passthrough flow and do not classify it as a downstream error - `inst-eh-td-8`
8. [x] - `p1` - **RETURN** the 502 or 504 problem+json response - `inst-eh-td-9`

### Streaming Error Source Marking

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-error-handling-streaming-source`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- An error on a streaming session carries `X-OAGW-Error-Source` exactly as a non-streaming response does: a gateway error raised before headers are produced is problem+json, and an upstream error is passthrough, for the `sse`, `ws`, and `wt` endpoint schemes alike.

**Error Scenarios**:
- The stream aborts after headers were sent: no status change or body rewrite is possible, so the source header value already emitted is preserved and the abort is classified as `stream.aborted.v1`.
- An intermediary strips the header from a streaming response: the gateway error remains recognizable by its problem+json content type, and the upstream error by its untouched body.

**Steps**:
1. [x] - `p1` - Classify the failure point of the streaming session: before response headers are produced, or after the stream has begun - `inst-eh-stream-1`
2. [x] - `p1` - **IF** the failure occurs before headers are produced - `inst-eh-stream-2`
   1. [x] - `p1` - Render the gateway error as problem+json with `X-OAGW-Error-Source: gateway`, or forward the upstream error unchanged with `X-OAGW-Error-Source: upstream`, exactly as for a non-streaming response - `inst-eh-stream-3`
3. [x] - `p1` - **IF** the stream aborts after headers were sent - `inst-eh-stream-4`
   1. [x] - `p1` - Preserve the source header value already emitted, terminate the stream, and classify the outcome as `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` - `inst-eh-stream-5`
4. [x] - `p1` - Apply the same distinction to the `sse`, `ws`, and `wt` endpoint schemes with no protocol-specific body rewriting - `inst-eh-stream-6`
5. [x] - `p1` - **RETURN** the streaming error response carrying `X-OAGW-Error-Source` - `inst-eh-stream-7`

## 3. Processes / Business Logic (CDSL)

### DomainError to Problem-Details Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-handling-status-mapping`

**Input**: one `DomainError` from any OAGW component, plus the request context available at the failure point: the requested alias, the resolved upstream identifier, the target host, the request path, and the trace identifier.
**Output**: the HTTP status, the GTS error type, the retriable flag, and the extension-field set for the response.

**Steps**:
1. [x] - `p1` - Resolve the variant against one mapping layer rather than mapping per call site, so every endpoint produces the same body for the same failure - `inst-eh-map-1`
2. [x] - `p1` - Assign the HTTP status from the DESIGN §3.3 row: 400 for validation and the three routing failures, 401 for authentication failure, 404 for route not found, 409 for plugin in use, 413 for payload too large, 429 for rate limit exceeded, 500 for secret not found, 502 for protocol error, downstream error, and stream aborted, 503 for link unavailable, circuit breaker open, and plugin not found, and 504 for the three timeouts - `inst-eh-map-2`
3. [x] - `p1` - Assign the GTS type `gts.cf.core.errors.err.v1~cf.oagw.<family>.<name>.v1` from the same row so the type and the status never disagree - `inst-eh-map-3`
4. [x] - `p1` - Take `title` from the mapped type and write `detail` from the occurrence, so the body always explains this failure and not only its class - `inst-eh-map-4`
5. [x] - `p1` - Set `instance` to the request path of the failing occurrence under the gear-relative `/oagw/v1` tree with no `/api` segment, per graded deviation 1 - `inst-eh-map-5`
6. [x] - `p1` - Populate extension members only when known: `upstream_id` whenever an upstream was resolved, `host` when a target endpoint was selected, `path` for the failing request, `retry_after_seconds` when retry guidance exists, `trace_id` whenever a trace identifier exists, and `referenced_by` for `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409) only, taken from the reference scan performed by `cpt-cf-oagw-feature-plugin-system` - `inst-eh-map-6`
7. [x] - `p1` - Omit an unknown extension member rather than emitting it as null - `inst-eh-map-7`
8. [x] - `p1` - **IF** the error context carries a `cred://` reference, secret material, or a credential-bearing header value - `inst-eh-map-8`
   1. [x] - `p1` - Exclude it from `detail` and from every extension member, so the error surface never leaks credential material - `inst-eh-map-9`
9. [x] - `p1` - **RETURN** the mapping result to the serializer - `inst-eh-map-10`

### Retriability and Retry Guidance Classification

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-handling-retriability`

**Input**: a mapped error with its GTS type and HTTP status.
**Output**: the retriable flag and, when a positive value exists, the retry guidance carried by both `Retry-After` and `retry_after_seconds`.

**Steps**:
1. [x] - `p1` - Treat retriability as metadata of the error type, not as a gateway decision to resend anything - `inst-eh-retry-1`
2. [x] - `p1` - Mark retriable: `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` (429), `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` (503), and `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`, `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`, `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` (504) - `inst-eh-retry-2`
3. [x] - `p1` - Mark non-retriable: the 400 validation and routing family, `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`, `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`, `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`, `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`, `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`, `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1`, `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`, and the two 403 CORS rejection types `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, which sit outside the DESIGN §3.3 table and are classified with the same non-retriable rule as the 400 family - `inst-eh-retry-3`
4. [x] - `p1` - Leave `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` as the single "depends" case and resolve its flag per occurrence from the error's own metadata - `inst-eh-retry-4`
5. [x] - `p1` - Emit `retry_after_seconds` and the matching `Retry-After` header only for a retriable type that carries guidance, with the guidance value sourced per retriable family: for `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` the value is supplied by the rate-limit decision of `cpt-cf-oagw-feature-rate-limiting` (entry 2.7); for `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`, `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`, and `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` the value is the configured `proxy_timeout_secs` of `OagwConfig` delivered by entry 2.1; for `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` and `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` no guidance is emitted, so neither `retry_after_seconds` nor `Retry-After` appears on those bodies - `inst-eh-retry-5`
6. [x] - `p1` - Never re-issue the failed client request from the gateway, per `cpt-cf-oagw-principle-no-retry` - `inst-eh-retry-6`
7. [x] - `p1` - **RETURN** the classification - `inst-eh-retry-7`

### Error Source Assignment

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-handling-source-header`

**Input**: the origin of the response being produced (an OAGW component or the upstream service) and the response kind (success, error, or streaming error).
**Output**: the `X-OAGW-Error-Source` header value.

**Steps**:
1. [x] - `p1` - Set `gateway` when the response is produced by an OAGW component, including every mapped problem+json body - `inst-eh-src-1`
2. [x] - `p1` - Set `upstream` when the response was received from the upstream service and is forwarded, regardless of its status code - `inst-eh-src-2`
3. [x] - `p1` - Keep the body unmodified for every response marked `upstream`, including its content type - `inst-eh-src-3`
4. [x] - `p1` - Emit the header on every response the gateway produces, success and error alike: `gateway` for a gateway-generated body, `upstream` for a passthrough response of any status including a success status, and on error responses of every protocol, including streaming error responses on the `sse`, `ws`, and `wt` endpoint schemes, per graded deviation for `cpt-cf-oagw-principle-error-source` - `inst-eh-src-4`
5. [x] - `p1` - Keep the body form an independent signal, because intermediaries may strip the header: a gateway error is always recognizable by `application/problem+json` and an upstream error by its untouched body - `inst-eh-src-5`
6. [x] - `p1` - **RETURN** the header value - `inst-eh-src-6`

### Validation Failure Rendering

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-handling-validation-render`

**Input**: the validation failure set produced by the proxy-path enforcement owned by entry 2.4, with the failing check and the request context.
**Output**: a 4xx problem+json response carrying `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` or, for a size failure, `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`.

**Steps**:
1. [x] - `p1` - Consume the failure set as input and perform no validation in this layer - `inst-eh-vrender-1`
2. [x] - `p1` - Render every shape, membership, and integrity failure as 400 with `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and `X-OAGW-Error-Source: gateway` - `inst-eh-vrender-2`
3. [x] - `p1` - Name the first failing check in `detail` together with the offending field, and exclude credential material from the body - `inst-eh-vrender-3`
4. [x] - `p1` - Classify the type as non-retriable - `inst-eh-vrender-4`
5. [x] - `p1` - **IF** the failure is a payload-size failure rather than a body-shape or membership failure - `inst-eh-vrender-5`
   1. [x] - `p1` - Render 413 with `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` instead of 400 - `inst-eh-vrender-6`
6. [x] - `p1` - **RETURN** the rendered 4xx response - `inst-eh-vrender-7`

### Problem-Details Serialization

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-error-handling-serialization`

**Input**: a mapping result carrying the status, GTS type, title, detail, instance, retriable flag, retry guidance, and known extension fields.
**Output**: the response headers and body bytes of one gateway error.

**Steps**:
1. [x] - `p1` - Serialize exactly the five RFC 9457 members `type`, `title`, `status`, `detail`, and `instance` - `inst-eh-ser-1`
2. [x] - `p1` - Serialize the OAGW extension members `upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`, and `referenced_by` whenever their values are known, with `referenced_by` carried only by `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409) and sourced from the reference scan of `cpt-cf-oagw-feature-plugin-system` - `inst-eh-ser-2`
3. [x] - `p1` - Serialize the target-host routing family members `alias`, `valid_hosts`, and `invalid_value` on the routing types that carry them, per `cpt-cf-oagw-adr-error-source-distinction` - `inst-eh-ser-3`
4. [x] - `p1` - Set `Content-Type: application/problem+json` and `X-OAGW-Error-Source: gateway` on the response - `inst-eh-ser-4`
5. [x] - `p1` - Keep the `status` member equal to the HTTP status code of the response - `inst-eh-ser-5`
6. [x] - `p1` - Omit any extension member whose value is unknown and emit no null member - `inst-eh-ser-6`
7. [x] - `p1` - **RETURN** the serialized response - `inst-eh-ser-7`
   1. [x] - `p1` - Serialize the two 403 CORS rejection types with the five RFC 9457 members plus the shared request-context members `path` and `trace_id`, and with none of the routing-family members, because the types delivered by `cpt-cf-oagw-feature-cors` carry no routing context - `inst-eh-ser-8`

## 4. States (CDSL)

No explicit lifecycle states in this feature. Error mapping is stateless: every `DomainError` is mapped and rendered in one pass, no error record is stored, and the circuit-breaker state that would be stateful is DESIGN §4.7 future work outside this feature.

## 5. Definitions of Done

### Problem-Details Contract

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-problem-details-contract`

The system **MUST** render every gateway-generated error as an RFC 9457 problem-details body with `Content-Type: application/problem+json` carrying the five standard members `type`, `title`, `status`, `detail`, and `instance`, plus the OAGW extension members `upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`, and `referenced_by` whenever their values are known, and **MUST** set `instance` to the failing request path under the gear-relative `/oagw/v1` tree with no `/api` segment. The extension-member vocabulary is closed at those six members plus the routing-family members `alias`, `valid_hosts`, and `invalid_value`; `referenced_by` is carried only by `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409) and is sourced from the reference scan performed by `cpt-cf-oagw-feature-plugin-system` (entry 2.6), whose delete flow requires that body. No gateway error **MUST** be rendered in any other body format, and no extension member outside this vocabulary **MUST** be emitted.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-gateway-render`
- `cpt-cf-oagw-algo-error-handling-serialization`

**Touches**:
- API: error rendering applied to every `/oagw/v1/...` endpoint registered by the foundation
- Entities: `DomainError`, problem-details response DTO
- Tests: unit tests in `src/api/rest/error_tests.rs` for the member set, content type, and omission of unknown extension members

### Design Error-Table Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-error-table-mapping`

The system **MUST** map every row of the DESIGN §3.3 error table onto its GTS type and HTTP status through one mapping layer, with exactly one HTTP status per row: `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` (400, carrying both the RouteError and the ValidationError rows), `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1` (400), `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` (401), `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1` (404), `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1` (409), `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1` (413), `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` (429), `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1` (500), `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` (502), `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1` (503), `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1` (504), `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1` (504), and `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` (504).

The RouteError row is decided here as follows: `RouteError` is a **DISTINCT** `DomainError` variant that carries route context (the failing method, path prefix, or match rule) and maps to the same GTS type and the same HTTP status as `ValidationError`, namely `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` at `400`; it is distinguishable from a validation failure by its `detail` text and not by its type or status.

Two further GTS types are rendered through this mapping layer although they sit outside the DESIGN §3.3 table: `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` (403) and `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1` (403). Both types are owned as types by `cpt-cf-oagw-feature-cors` (entry 2.8), which supplies the type, the status, and the detail text, but they are rendered through this contract so the response shape does not fork per rejection source; they appear in the mapping table, in the retriability classification as non-retriable, and in the extension-member rules of this document.

Inbound request failures are mapped as follows. An inbound authentication failure - a missing or invalid bearer token, or an absent security context - maps to `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` with status `401`, the same type DESIGN §3.3 lists for outbound upstream authentication. An inbound authorization failure - the caller lacks the DESIGN permission for the operation - is rendered through the shared canonical permission-denied surface of the platform with status `403` and introduces **NO** new OAGW error type. With those two decisions stated, every gateway outcome named by `cpt-cf-oagw-feature-request-proxy` (entry 2.4), `cpt-cf-oagw-feature-plugin-system` (entry 2.6), `cpt-cf-oagw-feature-rate-limiting` (entry 2.7), and `cpt-cf-oagw-feature-cors` (entry 2.8) resolves to a type and a status in this document.

A stream abort is mapped conditionally: a stream abort that occurs BEFORE response headers are produced maps to `502` with `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, while an abort after headers were sent is classified as `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` for attribution only, with no status or body change.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-gateway-render`
- `cpt-cf-oagw-flow-error-handling-timeout-downstream`
- `cpt-cf-oagw-algo-error-handling-status-mapping`

**Touches**:
- API: error rendering applied to every `/oagw/v1/...` endpoint
- Entities: `DomainError`
- Tests: unit tests in `src/api/rest/error_tests.rs` asserting one status, one GTS type, and one retriable flag per mapped variant

### PRD Error-Code Table Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-prd-error-codes`

The system **MUST** satisfy `cpt-cf-oagw-fr-error-codes` by returning the PRD error-code table for proxy and management operations, with each code realized by the GTS type above: 400 ValidationError, 401 AuthenticationFailed, 404 RouteNotFound, 413 PayloadTooLarge, 429 RateLimitExceeded, 500 SecretNotFound, 502 DownstreamError, 503 CircuitBreakerOpen, and 504 Timeout. The three 503 outcomes of the DESIGN table (link unavailable, circuit breaker open, plugin not found) **MUST** remain distinguishable by GTS type while all satisfying the PRD's 503 row, and the PRD's single 504 Timeout row **MUST** be realized by the three timeout types.

**Implements**:
- `cpt-cf-oagw-algo-error-handling-status-mapping`
- `cpt-cf-oagw-algo-error-handling-retriability`

**Touches**:
- API: error rendering applied to every `/oagw/v1/...` endpoint registered by the foundation
- Entities: `DomainError`
- Tests: unit tests in `src/api/rest/error_tests.rs` asserting the PRD code set and the retriable flags 429 yes, 503 yes, 504 yes, 502 depends

### Error Source Header

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-error-source-header`

The system **MUST** emit `X-OAGW-Error-Source: gateway` on every gateway-generated error and `X-OAGW-Error-Source: upstream` on every passthrough upstream error, on error responses of every protocol including streaming error responses on the `sse`, `ws`, and `wt` endpoint schemes, and **MUST** leave the upstream body, headers, and content type unmodified whenever the value is `upstream`. The header **MUST** additionally be emitted on every non-error response the gateway produces, so that ADR 0007's Confirmation clause ("success responses include the header") is verifiable: `gateway` on a response body the gateway generated, and `upstream` on a passthrough response of any status, success included. The emission point for the success path is the response pipeline owned by `cpt-cf-oagw-feature-request-proxy` (entry 2.4); the value-assignment rule and the success-path assertion are owned here. This realizes `cpt-cf-oagw-principle-error-source` per `cpt-cf-oagw-adr-error-source-distinction`.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-upstream-passthrough`
- `cpt-cf-oagw-flow-error-handling-streaming-source`
- `cpt-cf-oagw-algo-error-handling-source-header`

**Touches**:
- API: response-header emission on every `/oagw/v1/...` endpoint
- Entities: none
- Tests: integration tests in `tests/streaming_error_source.rs` asserting both values, presence on streaming error responses, presence on a success response of each source kind, and an untouched upstream body

### Target-Host Routing Error Bodies

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-target-host-bodies`

The system **MUST** render the three `X-OAGW-Target-Host` routing failures as 400 problem+json bodies per the examples in `cpt-cf-oagw-adr-error-source-distinction`: the missing-header body carries `upstream_id`, `alias`, and `valid_hosts`; the invalid-format body carries `upstream_id` and `invalid_value`; the unknown-host body carries `upstream_id`, `invalid_value`, and `valid_hosts`; and all three carry `trace_id` and name the valid hosts in `detail`. The echoed `invalid_value` **MUST** be truncated to a maximum of 128 characters and **MUST** contain no control character; it **MUST** be emitted only through the JSON serializer and **MUST NOT** be concatenated into `detail` or into any header value; and the echoed value is limited to the value of the `X-OAGW-Target-Host` request header, never to any other request header or to any request body content.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-target-host-render`
- `cpt-cf-oagw-algo-error-handling-serialization`

**Touches**:
- API: error rendering applied to `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `DomainError`
- Tests: integration tests in `tests/error_contract.rs` asserting the three bodies, their extension members, and the 128-character, control-character, and serializer-only bounds on the echoed `invalid_value`

### Disabled Upstream 503 Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-disabled-upstream-503`

The system **MUST** render the PRD's disabled-upstream outcome as `503` with `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`, `X-OAGW-Error-Source: gateway`, and a retriable flag, because DESIGN §3.3 defines no dedicated GTS type for that outcome; the mapping **MUST** be stated here rather than left implicit, and no other type **MUST** be emitted for a disabled upstream.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-disabled-upstream`
- `cpt-cf-oagw-algo-error-handling-status-mapping`

**Touches**:
- API: error rendering applied to `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `DomainError`
- Tests: integration tests in `tests/error_contract.rs` disabling an upstream and asserting status 503, the GTS type, and the gateway source header

### Payload Too Large 413 Mapping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-payload-too-large-413`

The system **MUST** render the body-limit outcome enforced by entry 2.4 as `413` with `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`, `X-OAGW-Error-Source: gateway`, a non-retriable flag, and no `Retry-After` header, while the limit check itself remains owned by entry 2.4 under `cpt-cf-oagw-constraint-body-limit`.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-payload-too-large`
- `cpt-cf-oagw-algo-error-handling-validation-render`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: error rendering applied to `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `DomainError`
- Tests: integration tests in `tests/error_contract.rs` asserting 413 with the payload type for an oversized body and 400 with the validation type for a malformed Content-Length

### Validation Failure Rendering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-validation-rendering`

The system **MUST** render every validation failure enforced by entry 2.4 - a method outside the route allowlist, an unknown query parameter, a rejected path suffix, an invalid well-known header, a Content-Length integrity failure, and an unsupported Transfer-Encoding - as a 4xx problem+json body with `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`, `X-OAGW-Error-Source: gateway`, and a non-retriable flag, and **MUST** perform no validation of its own. A body-shape or membership failure rather than a payload-size failure is rendered as `400` with the validation type and never as `413`. A CORS origin or CORS method rejection enforced by entry 2.8 is rendered through this same contract at `403` with the type that entry owns, which sits outside the DESIGN §3.3 table. This is the rendering slice of `cpt-cf-oagw-nfr-input-validation`; enforcement is owned by entry 2.4 and the CORS-configuration validation slice by entry 2.8.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-validation-render`
- `cpt-cf-oagw-algo-error-handling-validation-render`

**Touches**:
- API: error rendering applied to `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `DomainError`
- Tests: integration tests in `tests/error_contract.rs` asserting one 400 body per enforced check class

### Retriability and Retry Guidance Metadata

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-retriability`

The system **MUST** carry retriability as metadata on the mapped error type - retriable for 429, for both 503 link-unavailable and circuit-breaker-open, and for all three 504 timeouts; non-retriable for 400, 401, 404, 409, 413, 500, 502 protocol-error and stream-aborted, 503 plugin-not-found, and the two 403 CORS rejection types; and resolved per occurrence for 502 downstream-error - and **MUST** emit `retry_after_seconds` with a matching `Retry-After` header only for a retriable type that carries guidance. Every retriable type **MUST** have either a named guidance source or an explicit no-guidance statement, and the emitted value **MUST** be asserted against that source. The `429` outcome is split between two entries: `cpt-cf-oagw-feature-rate-limiting` (entry 2.7) owns the rate-limit decision, the `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` headers, and the guidance value, while this entry renders the `429` `application/problem+json` body, the `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1` type, and the `Retry-After` / `retry_after_seconds` pair derived from that decision, and emits no `X-RateLimit-*` header. The gateway **MUST NOT** re-issue a failed client request, per `cpt-cf-oagw-principle-no-retry`.

**Implements**:
- `cpt-cf-oagw-algo-error-handling-retriability`
- `cpt-cf-oagw-flow-error-handling-gateway-render`

**Touches**:
- API: error rendering applied to every `/oagw/v1/...` endpoint; the 429 body of `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: `DomainError`
- Tests: unit tests in `src/api/rest/error_tests.rs` for every retriable flag, plus integration tests in `tests/error_contract.rs` asserting `Retry-After` and `retry_after_seconds` agree and that the emitted value matches its named guidance source

### DomainError Taxonomy and Mapping Layer

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-error-handling-domain-error-taxonomy`

The system **MUST** expose one mapping layer in `api/rest/error.rs` that resolves a `DomainError` variant to its HTTP status, GTS type, retriable flag, and extension-field set, so no endpoint maps an error locally and the mapping layer is the single source of this contract. This is a **mapping-layer** definition of done: the `DomainError` variant set itself is defined by `cpt-cf-oagw-dod-gear-foundation-domain-error` (entry 2.1) in `domain/error.rs`, and this entry **MUST NOT** re-define, extend, or re-test that variant set. This entry owns only the variant-to-status, variant-to-GTS-type, retriable-flag, and extension-field mapping, and it extends the existing taxonomy only where a DESIGN §3.3 row has no variant to map onto: `RouteError` is such a case and is carried as a distinct variant that maps to the same type and status as `ValidationError`, while the two 403 CORS rejection types and the platform permission-denied surface are types owned by entry 2.8 and by the platform respectively and are rendered here without adding a variant. Inbound authentication failures - a missing or invalid bearer token, or an absent security context - map onto the existing `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` variant at `401`, the same type DESIGN §3.3 lists for outbound upstream authentication, and inbound authorization failures resolve to the platform's canonical permission-denied surface at `403` with no new OAGW type, so no additional variant is owed by this entry for either case.

**Implements**:
- `cpt-cf-oagw-algo-error-handling-status-mapping`
- `cpt-cf-oagw-flow-error-handling-gateway-render`

**Touches**:
- API: `api/rest/error.rs`
- Entities: `DomainError`
- Tests: unit tests in `src/api/rest/error_tests.rs` for variant-to-status, variant-to-GTS-type, retriable-flag, and extension-field coverage against the DESIGN table; the variant-set assertions of the taxonomy stay in `src/domain/error_tests.rs` under entry 2.1 and are not duplicated here

### Trace Identifier Propagation

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-error-handling-trace-id`

The system **MUST** propagate a trace identifier into every problem+json body as `trace_id` and make the GTS `type` available as the `error_type` attribution field consumed by the audit log of entry 2.9, and **MUST** omit `trace_id` only when no trace identifier exists for the request. This is the error-surface slice of `cpt-cf-oagw-nfr-observability`; the metrics and logging surface is owned by entry 2.9.

**Implements**:
- `cpt-cf-oagw-algo-error-handling-serialization`
- `cpt-cf-oagw-flow-error-handling-gateway-render`

**Touches**:
- API: error rendering applied to every `/oagw/v1/...` endpoint
- Entities: problem-details response DTO
- Tests: integration tests in `tests/error_contract.rs` asserting `trace_id` presence and stability across a rendered error and its audit record

### Upstream Error Passthrough

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-upstream-passthrough`

The system **MUST** forward an upstream error response with its status code, headers, content type, and body unmodified and with `X-OAGW-Error-Source: upstream`, and **MUST** neither wrap it in problem+json, add extension members to it, nor reclassify it as a gateway error.

**Implements**:
- `cpt-cf-oagw-flow-error-handling-upstream-passthrough`
- `cpt-cf-oagw-algo-error-handling-source-header`

**Touches**:
- API: response pipeline of `/oagw/v1/proxy/{alias}[/{path_suffix}]`
- Entities: none
- Tests: integration tests in `tests/streaming_error_source.rs` asserting byte-identical upstream bodies and the upstream source header value

### Automated Unit Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-unit-tests`

The system **MUST** ship unit tests as sibling `*_tests.rs` modules inside the `oagw` crate covering the variant-to-status, variant-to-GTS-type, retriable-flag, and extension-field mapping for the complete DESIGN §3.3 table, the extension-field population rules including omission of unknown values and the closed member vocabulary, the problem-details serialization shape and content type, the source-header assignment, and the exclusion of credential material from error bodies, and **MUST NOT** place any test under `testing/e2e/gears/oagw/`, which is out of scope per graded deviation 4. The mapping assertions live in `src/api/rest/error_tests.rs` beside the mapping layer they verify; the `DomainError` variant-set assertions of entry 2.1 stay in `src/domain/error_tests.rs` and are not re-delivered here.

**Implements**:
- `cpt-cf-oagw-dod-error-handling-problem-details-contract`
- `cpt-cf-oagw-dod-error-handling-domain-error-taxonomy`
- `cpt-cf-oagw-dod-error-handling-retriability`

**Touches**:
- API: none
- Entities: `DomainError`, problem-details response DTO
- Tests: `src/api/rest/error_tests.rs`, with `src/domain/error_tests.rs` left to entry 2.1

### Automated Integration Test Coverage

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-error-handling-integration-tests`

The system **MUST** ship integration-style tests inside the crate's `tests/` directory covering every mapped status, header value, and problem+json body shape against real proxy outcomes: the PRD error-code set, the three target-host bodies and the bound placed on the echoed `invalid_value`, the disabled-upstream 503, the 413 payload outcome, the 400 validation bodies, the 429 body rendered from the entry-2.7 decision, the 403 CORS rejection bodies rendered with the entry-2.8 types, the 502 and 504 families, `X-OAGW-Error-Source` on non-streaming and streaming error responses and on a success response of each source kind, and byte-identical upstream passthrough, and **MUST NOT** create `testing/e2e/gears/oagw/`.

**Implements**:
- `cpt-cf-oagw-dod-error-handling-prd-error-codes`
- `cpt-cf-oagw-dod-error-handling-target-host-bodies`
- `cpt-cf-oagw-dod-error-handling-disabled-upstream-503`
- `cpt-cf-oagw-dod-error-handling-payload-too-large-413`
- `cpt-cf-oagw-dod-error-handling-retriability`
- `cpt-cf-oagw-dod-error-handling-error-source-header`
- `cpt-cf-oagw-dod-error-handling-upstream-passthrough`

**Touches**:
- API: verification against `/oagw/v1/proxy/{alias}[/{path_suffix}]` and the management prefixes
- Entities: `DomainError`
- Tests: `tests/error_contract.rs`, `tests/streaming_error_source.rs`, `tests/streaming_error_source.rs`

## 6. Acceptance Criteria

- [x] Every gateway error response carries `Content-Type: application/problem+json` with the five RFC 9457 members `type`, `title`, `status`, `detail`, and `instance`, where `status` equals the response status code and `instance` is the failing gear-relative request path with no `/api` segment (DoD `cpt-cf-oagw-dod-error-handling-problem-details-contract`).
- [x] Each of the twenty DESIGN §3.3 rows maps onto one of the nineteen distinct GTS types - RouteError and ValidationError both map to `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` - with exactly one HTTP status per row, and no endpoint maps an error locally outside the shared mapping layer (DoD `cpt-cf-oagw-dod-error-handling-error-table-mapping`, `cpt-cf-oagw-dod-error-handling-domain-error-taxonomy`).
- [x] The PRD error-code table is satisfied end to end: 400, 401, 404, 413, 429, 500, 502, 503, and 504 all appear with the documented GTS types, the three 503 outcomes are distinguishable by type, and the three timeout types share 504 (DoD `cpt-cf-oagw-dod-error-handling-prd-error-codes`).
- [x] A gateway error carries `X-OAGW-Error-Source: gateway` and an upstream error carries `X-OAGW-Error-Source: upstream`, on both non-streaming and streaming error responses across the `sse`, `ws`, and `wt` schemes, and a success response - gateway-generated or passthrough - carries the header with the same value rule, so ADR 0007's Confirmation clause holds (DoD `cpt-cf-oagw-dod-error-handling-error-source-header`).
- [x] An upstream error response is returned byte-identically in body, status, headers, and content type, with no problem+json wrapper and no added extension member (DoD `cpt-cf-oagw-dod-error-handling-upstream-passthrough`).
- [x] The missing, invalid, and unknown `X-OAGW-Target-Host` outcomes return 400 with their three distinct GTS types and exactly the documented extension members - `upstream_id`, `alias`, and `valid_hosts` for the missing header; `upstream_id` and `invalid_value` for the invalid format; `upstream_id`, `invalid_value`, and `valid_hosts` for the unknown host - with `trace_id` on all three (DoD `cpt-cf-oagw-dod-error-handling-target-host-bodies`).
- [x] An echoed `invalid_value` is truncated to at most 128 characters, contains no control character, is emitted only through the JSON serializer, never appears in `detail` or in a header value, and is limited to the `X-OAGW-Target-Host` header value (DoD `cpt-cf-oagw-dod-error-handling-target-host-bodies`, asserted in `tests/error_contract.rs`).
- [x] A proxy request against a disabled upstream returns 503 with `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` and `X-OAGW-Error-Source: gateway`, and the open-breaker and plugin-not-found 503 outcomes carry different types (DoD `cpt-cf-oagw-dod-error-handling-disabled-upstream-503`).
- [x] A request body exceeding the limit returns 413 with `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`, while a malformed Content-Length or an unsupported Transfer-Encoding returns 400 with the validation type (DoD `cpt-cf-oagw-dod-error-handling-payload-too-large-413`).
- [x] Every validation failure enforced on the proxy path is rendered as a 4xx problem+json body naming the failing check, and the rendering layer performs no validation of its own (DoD `cpt-cf-oagw-dod-error-handling-validation-rendering`).
- [x] A CORS origin or CORS method rejection is rendered through this contract with the type owned by entry 2.8, `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1` or `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`, and a `403` status, so the response shape does not fork per rejection source (DoD `cpt-cf-oagw-dod-error-handling-error-table-mapping`, asserted in `tests/error_contract.rs`).
- [x] A request refused by the rate limiter returns the `429` problem+json body with `retry_after_seconds` and `Retry-After` taken from the entry-2.7 decision and carries no `X-RateLimit-*` header from this contract (DoD `cpt-cf-oagw-dod-error-handling-retriability`, asserted in `tests/error_contract.rs`).
- [x] Every retriable type has either a named guidance source or an explicit no-guidance statement, the emitted guidance value matches that source - the rate-limit decision of entry 2.7, or the configured `proxy_timeout_secs` of `OagwConfig` for the three timeouts - and `link.unavailable.v1` and `circuit_breaker.open.v1` carry neither `retry_after_seconds` nor `Retry-After` (DoD `cpt-cf-oagw-dod-error-handling-retriability`).
- [x] Every gateway outcome named by entries 2.4, 2.6, 2.7, and 2.8 resolves to a GTS type and an HTTP status in this document, including inbound authentication failures mapped to `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1` at 401 and inbound authorization failures resolved through the platform's canonical permission-denied surface at 403 with no new OAGW type (DoD `cpt-cf-oagw-dod-error-handling-error-table-mapping`, `cpt-cf-oagw-dod-error-handling-domain-error-taxonomy`).
- [x] Retriability flags match the DESIGN table for all nineteen distinct types, `Retry-After` and `retry_after_seconds` agree whenever retry guidance is emitted, and no gateway error triggers a re-issued client request (DoD `cpt-cf-oagw-dod-error-handling-retriability`).
- [x] Every problem+json body carries `trace_id` when a trace identifier exists, and the GTS `type` is available as the `error_type` attribution field for the audit record (DoD `cpt-cf-oagw-dod-error-handling-trace-id`).
- [x] No rendered error body, detail string, or extension member contains a `cred://` secret value, credential material, or a credential-bearing header value (DoD `cpt-cf-oagw-dod-error-handling-problem-details-contract`).
- [x] All tests for this feature live inside the `oagw` crate as sibling `*_tests.rs` modules and files under `tests/`, and no `testing/e2e/gears/oagw/` directory is created (DoD `cpt-cf-oagw-dod-error-handling-unit-tests`, `cpt-cf-oagw-dod-error-handling-integration-tests`).
