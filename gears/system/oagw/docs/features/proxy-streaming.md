# Feature: Streaming and Protocol Upgrades


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [SSE Response Streaming Consumption](#sse-response-streaming-consumption)
  - [WebSocket Session Proxying](#websocket-session-proxying)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [SSE Detection and Incremental Relay](#sse-detection-and-incremental-relay)
  - [WebSocket Upgrade Negotiation](#websocket-upgrade-negotiation)
  - [WebSocket Bidirectional Frame Relay](#websocket-bidirectional-frame-relay)
  - [Stream and Upgrade Abort Handling](#stream-and-upgrade-abort-handling)
- [4. States (CDSL)](#4-states-cdsl)
  - [SSE Stream Session State Machine](#sse-stream-session-state-machine)
  - [WebSocket Session State Machine](#websocket-session-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SSE Detection and Incremental Forwarding](#sse-detection-and-incremental-forwarding)
  - [SSE Connection Lifecycle](#sse-connection-lifecycle)
  - [WebSocket Handshake via Shared Resolution Path](#websocket-handshake-via-shared-resolution-path)
  - [WebSocket Bidirectional Frame Relay and Close Propagation](#websocket-bidirectional-frame-relay-and-close-propagation)
  - [StreamAborted Error and Committed-Response Behavior](#streamaborted-error-and-committed-response-behavior)
  - [Request Timeout Does Not Sever Committed Streams or Sessions](#request-timeout-does-not-sever-committed-streams-or-sessions)
  - [Plaintext Upstream Streaming Under `allow_http_upstream`](#plaintext-upstream-streaming-under-allow_http_upstream)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-proxy-streaming-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-proxy-streaming`
## 1. Feature Context

### 1.1 Overview

This feature extends the resolved proxy request path delivered by `cpt-cf-oagw-feature-proxy-core` with two long-lived transport modes: Server-Sent-Event (SSE) response streaming forwarded to the client without buffering the whole body, and `websocket` protocol-upgrade proxying with bidirectional frame relay. Both modes reuse proxy-core's alias/route resolution and header rules for their initial request, and both carry correct connection-lifecycle handling, including the `StreamAborted` gateway error for an in-progress stream or session that is aborted.

**Out of scope, per `cpt-cf-oagw-feature-proxy-streaming`'s DECOMPOSITION entry**:
- WebTransport session flows (the `wt` endpoint scheme is reserved by the schema, but no Component Model, Domain Model, or Interactions & Sequences section in `DESIGN.md` binds a WebTransport proxy engine — there is no documented behavior to implement against in this round).
- gRPC bidirectional/server/client-streaming proxying (`DESIGN.md` states no gRPC proxy code path is implemented or reachable; it is Phase-3/4 future work per `PRD.md` §4.2 and `DESIGN.md` §4.7 item 7, "Requires prototype").
- Rate limiting and CORS applied to streaming connections (`cpt-cf-oagw-feature-cors-handling` and `cpt-cf-oagw-feature-rate-limiting` apply their checks at connection-open time using the same mechanism as a non-streaming request; this feature introduces no additional per-frame or per-event enforcement).
- HTTP/3 (QUIC) multiplexing (explicit `DESIGN.md` §4.5 future work).

### 1.2 Purpose

Many upstreams that OAGW fronts use SSE for incremental response delivery (e.g., LLM chat completions) and WebSocket for bidirectional real-time protocols; forwarding either transport correctly — without buffering an SSE body to completion, and without breaking a WebSocket handshake or losing frames/close codes — is required for OAGW to be usable as a general-purpose outbound gateway rather than a plain-request-only proxy.

**Requirements Covered**:

- [ ] `p1` - `cpt-cf-oagw-fr-streaming` — partial: server-sent events and WebSocket upgrade proxying are delivered by this feature; WebTransport is deferred (see 1.1 Overview: no design section binds a WebTransport proxy engine)
- [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`

**Design Principles Covered**:

- `cpt-cf-oagw-principle-no-cache` — an SSE response is relayed as it arrives and is never buffered/cached in full before delivery; this is the same "no response caching" posture proxy-core follows, applied to a streaming body

**Cross-cutting behavior**:

- **Security**: this feature introduces no new inbound authentication/authorization surface — the handshake/initial-request resolution reuses proxy-core's tenant/alias/route resolution and permission checks unchanged. It introduces no new SSRF surface: the same scheme allowlist and `allow_http_upstream` gate that proxy-core enforces for a plain request (`cpt-cf-oagw-constraint-https-only`, lifted when `allow_http_upstream` is `true`) governs whether a streaming or upgrade connection may be opened to a plaintext `http`/`ws` endpoint — streaming and upgrades are not restricted to TLS schemes only. Credential injection and guard/transform execution against the initial request are owned by `cpt-cf-oagw-feature-plugin-execution`, not this feature.
- **Reliability**: consistent with `cpt-cf-oagw-principle-no-retry`, an aborted stream or session is never silently retried; abort is a terminal outcome reported via the documented `StreamAborted` error or a connection-level close (see §3 `cpt-cf-oagw-algo-stream-abort-handling`).
- **Data integrity**: SSE bytes and WebSocket frames are relayed verbatim (byte-for-byte for SSE chunks; frame-type-preserving for WebSocket text/binary frames) — this feature performs no body transformation beyond the header handling already defined by proxy-core.
- **Observability**: every streamed/upgraded request carries proxy-core's correlation ID and audit-log entry; a `StreamAborted` condition (in either its rendered-error form or its connection-level-close form) is additionally recorded in the audit log and error metrics so operators can distinguish a clean stream close from an aborted one.
- **Rollback**: not applicable — this feature adds no database schema, no migrations, and no persisted configuration; it is pure request-time behavior layered on the existing proxy path, so there is no data-layer state to roll back.
- **UX and accessibility**: not applicable because this feature exposes no user interface — an event stream and a WebSocket session are machine-to-machine transports whose bytes and frames are relayed verbatim, with no rendered view, no interaction affordance and therefore no assistive-technology contract to satisfy.
- **Compliance and privacy**: not applicable because this feature processes no personal or otherwise regulated data — SSE chunks and WebSocket frames pass through unread, unclassified and unretained, and the only values this feature adds to the audit log are the correlation id it inherits from `cpt-cf-oagw-feature-proxy-core` and the `StreamAborted` outcome classification.
- **Data privacy**: not applicable for the same reason — nothing carried by a stream or a session is buffered to completion, cached, persisted or logged as content, so there is no subject data here to minimise, retain or erase.
- **Performance**: not applicable as a distinct budget for this feature — it introduces no additional per-event or per-frame processing beyond an immediate write to the opposite connection, and it deliberately removes rather than adds work by never buffering a stream body; the gateway's latency budget and its measurement remain `cpt-cf-oagw-feature-proxy-core`'s, whose correlation, metrics and audit plumbing this feature reuses unchanged.
- **Extension points**: not applicable because this feature declares no hook surface of its own for a further feature to attach to — it is the consumer of proxy-core's named hooks (the upgrade-header exception, the protocol-upgrade branch, the incremental-relay hook and the long-lived-transfer state), not a provider of new ones. Inbound request-body streaming is not part of this feature's scope in this decomposition round: the scope is upstream-response event streaming and WebSocket frame relay only.
- **Resilience and recovery**: dispositioned rather than waived, and covered by the Reliability statement above together with `cpt-cf-oagw-algo-stream-abort-handling` — an abort is terminal and never retried, recovery is the client's to initiate by opening a new stream or session, and no partial state survives a failed one.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends the SSE-producing request or the WebSocket upgrade request through the proxy path, consumes the streamed events or relayed frames, and may disconnect mid-stream/mid-session. |
| `cpt-cf-oagw-actor-upstream-service` | External service that produces the SSE event stream or acts as the other endpoint of the WebSocket session; may close cleanly or abort mid-stream/mid-session. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — `cpt-cf-oagw-fr-streaming`, `cpt-cf-oagw-usecase-sse-streaming`, the Error Codes table (`cpt-cf-oagw-fr-error-codes`)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-component-model` (Domain Model Entities: Stream session, SSE event-forwarding state, WebSocket upgrade context), the Headers Transformation and HTTP Version Negotiation subsections, and the Error Response Format table's `StreamAborted` row
- **ADRs**: `cpt-cf-oagw-adr-error-source-distinction` (error-source contract applies identically to SSE and WebSocket responses/errors), `cpt-cf-oagw-adr-request-routing` (the resolution path this feature's initial request/handshake shares with a plain request)
- **Dependencies**:
  - [ ] `p1` - `cpt-cf-oagw-feature-proxy-core`

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**: `cpt-cf-oagw-usecase-sse-streaming`

### SSE Response Streaming Consumption

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-stream-sse-consumption`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The client's proxy request resolves normally (per `cpt-cf-oagw-feature-proxy-core`), the upstream response is an event stream, and each event reaches the client as it arrives rather than after the upstream finishes; the stream closes cleanly when the upstream closes it.
- The client disconnects while the stream is open; the corresponding upstream connection is closed as a direct consequence.

**Error Scenarios**:
- The upstream connection aborts before the response status line and headers have been committed to the client: the gateway renders the RFC 9457 `StreamAborted` (`502`) error with `X-OAGW-Error-Source: gateway`.
- The upstream connection aborts after the response status line and headers have already been committed to the client: since the status line can no longer change, the gateway terminates the client connection without emitting a second response, and records `StreamAborted` in the audit log and error metrics.

**Steps**:
1. [x] - `p1` - Application Developer sends `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}` toward an upstream endpoint expected to respond with an event stream - `inst-stream-sse-consumption-01`
2. [x] - `p1` - System resolves the alias, matches the route, merges configuration, and processes headers exactly as `cpt-cf-oagw-feature-proxy-core` does for any plain request, then opens the outbound connection to the resolved upstream endpoint - `inst-stream-sse-consumption-02`
3. [x] - `p1` - System reads only the upstream response status line and headers (not the body) and evaluates `cpt-cf-oagw-algo-stream-sse-detect-relay` to classify the response - `inst-stream-sse-consumption-03`
4. [x] - `p1` - **IF** the upstream response is classified as an event stream - `inst-stream-sse-consumption-04`
   1. [x] - `p1` - System commits the response status line and headers to the client immediately, without buffering the body, and the session enters the `Streaming` state of `cpt-cf-oagw-state-stream-sse-session` - `inst-stream-sse-consumption-05`
   2. [x] - `p1` - System forwards each chunk received from the upstream to the client as it arrives, per `cpt-cf-oagw-algo-stream-sse-detect-relay` - `inst-stream-sse-consumption-06`
   3. [x] - `p1` - **IF** the client disconnects while streaming - `inst-stream-sse-consumption-07`
      1. [x] - `p1` - System closes the upstream connection and transitions `cpt-cf-oagw-state-stream-sse-session` to `Closed` - `inst-stream-sse-consumption-08`
   4. [x] - `p1` - **ELSE IF** the upstream closes the connection normally after delivering all events - `inst-stream-sse-consumption-09`
      1. [x] - `p1` - System closes the client connection cleanly and transitions `cpt-cf-oagw-state-stream-sse-session` to `Closed` - `inst-stream-sse-consumption-10`
   5. [x] - `p1` - **ELSE IF** the upstream connection aborts unexpectedly mid-stream - `inst-stream-sse-consumption-11`
      1. [x] - `p1` - System applies `cpt-cf-oagw-algo-stream-abort-handling` and transitions `cpt-cf-oagw-state-stream-sse-session` to `Aborted` - `inst-stream-sse-consumption-12`
5. [x] - `p1` - **ELSE** the response is not an event stream - `inst-stream-sse-consumption-13`
   1. [x] - `p1` - System forwards the response using `cpt-cf-oagw-feature-proxy-core`'s non-streaming forwarding path (out of this feature's scope) - `inst-stream-sse-consumption-14`
6. [x] - `p1` - **RETURN** the fully delivered event stream terminated by a clean `Closed` transition, or the `StreamAborted` outcome produced by step 4.5 - `inst-stream-sse-consumption-15`

### WebSocket Session Proxying

- [x] `p1` - **ID**: `cpt-cf-oagw-flow-stream-websocket-session`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The client sends an `Upgrade: websocket` request on the proxy path; it is resolved exactly as a plain request, the upstream completes the handshake, and text/binary frames are relayed in both directions until either side sends a close frame, whose close code is propagated to the other side.

**Error Scenarios**:
- Resolution fails before any upgrade attempt is made (e.g., the upstream/route resolution itself fails): the gateway renders an ordinary RFC 9457 gateway error, identical to a plain request's resolution failure.
- The upstream refuses or fails to complete the handshake (non-`101` response, connection failure, or timeout) before it completes: the gateway renders an ordinary RFC 9457 gateway error, because no upgrade has occurred yet and a normal HTTP error response is still possible.
- The connection fails after the handshake has completed: no HTTP response is possible any longer; the gateway performs a connection-level close of both sides and records `StreamAborted`.

**Steps**:
1. [x] - `p1` - Application Developer sends `GET /oagw/v1/proxy/{alias}/{path_suffix}` with `Upgrade: websocket`, `Connection: Upgrade`, and the `Sec-WebSocket-*` handshake headers - `inst-stream-websocket-session-01`
2. [x] - `p1` - System resolves the alias, matches the route, merges configuration, and applies header rules exactly as `cpt-cf-oagw-feature-proxy-core` does for any plain request, recognizing the request as an upgrade request via `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation` - `inst-stream-websocket-session-02`
3. [x] - `p1` - **IF** resolution fails (e.g., `RouteNotFound`, disabled upstream) - `inst-stream-websocket-session-03`
   1. [x] - `p1` - System renders the identical ordinary RFC 9457 gateway error a plain request would receive; no upgrade is attempted - `inst-stream-websocket-session-04`
4. [x] - `p1` - **ELSE** - `inst-stream-websocket-session-05`
   1. [x] - `p1` - System forwards the handshake request to the resolved upstream endpoint per `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`, and the session enters the `HandshakePending` state of `cpt-cf-oagw-state-stream-websocket-session` - `inst-stream-websocket-session-06`
   2. [x] - `p1` - **IF** the upstream fails to complete the handshake (non-`101` response, refusal, or timeout) - `inst-stream-websocket-session-07`
      1. [x] - `p1` - System renders an ordinary RFC 9457 gateway error and transitions `cpt-cf-oagw-state-stream-websocket-session` to `Aborted` - `inst-stream-websocket-session-08`
   3. [x] - `p1` - **ELSE** the upstream returns `101 Switching Protocols` - `inst-stream-websocket-session-09`
      1. [x] - `p1` - System completes the upgrade with the client and transitions `cpt-cf-oagw-state-stream-websocket-session` to `Open` - `inst-stream-websocket-session-10`
      2. [x] - `p1` - System relays frames bidirectionally per `cpt-cf-oagw-algo-stream-websocket-frame-relay` - `inst-stream-websocket-session-11`
      3. [x] - `p1` - **IF** either side sends a close frame - `inst-stream-websocket-session-12`
         1. [x] - `p1` - System propagates the close frame and its close code to the other side, transitions `cpt-cf-oagw-state-stream-websocket-session` to `Closing`, then to `Closed` once both sides close - `inst-stream-websocket-session-13`
      4. [x] - `p1` - **ELSE IF** the connection terminates abnormally without a close frame - `inst-stream-websocket-session-14`
         1. [x] - `p1` - System applies `cpt-cf-oagw-algo-stream-abort-handling`'s connection-level closure and transitions `cpt-cf-oagw-state-stream-websocket-session` to `Aborted` - `inst-stream-websocket-session-15`
5. [x] - `p1` - **RETURN** the closed session (clean `Closed`, pre-handshake gateway error, or post-handshake `Aborted` connection-level close) - `inst-stream-websocket-session-16`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly.

### SSE Detection and Incremental Relay

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-stream-sse-detect-relay`

**Input**: the upstream response status line, headers, and body byte stream, already produced by proxy-core's resolved outbound connection

**Output**: either an incremental relay of the response to the client (this algorithm's success path), or a signal that the response is not an event stream so the caller should fall back to proxy-core's non-streaming forwarding

**Steps**:
1. [x] - `p1` - Parse the upstream response's `Content-Type` header, comparing only its base media type (ignoring `charset`/other parameters) - `inst-stream-sse-detect-relay-01`
2. [x] - `p1` - **IF** the base media type is `text/event-stream` - `inst-stream-sse-detect-relay-02`
   1. [x] - `p1` - Commit the upstream response's status line and headers to the client immediately, applying the same passthrough/hop-by-hop header rules `cpt-cf-oagw-feature-proxy-core` applies to a plain response, and set `X-OAGW-Error-Source: upstream` on the committed response per `cpt-cf-oagw-adr-error-source-distinction` (the response originates from the upstream, not the gateway) - `inst-stream-sse-detect-relay-03`
   2. [x] - `p1` - **FOR EACH** chunk of bytes received from the upstream body stream - `inst-stream-sse-detect-relay-04`
      1. [x] - `p1` - Write the chunk to the client connection immediately; do not accumulate the response body in memory before writing - `inst-stream-sse-detect-relay-05`
   3. [x] - `p1` - **RETURN** a clean-completion signal when the upstream signals end-of-stream, or an abort signal if the upstream connection fails mid-loop - `inst-stream-sse-detect-relay-06`
3. [x] - `p1` - **ELSE** - `inst-stream-sse-detect-relay-07`
   1. [x] - `p1` - **RETURN** a not-a-stream signal so the caller forwards the response via the non-streaming path - `inst-stream-sse-detect-relay-08`

### WebSocket Upgrade Negotiation

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`

**Input**: an inbound request already resolved to an upstream/route via `cpt-cf-oagw-feature-proxy-core`'s resolution path

**Output**: a completed upgrade with the upstream (bidirectional channel ready for `cpt-cf-oagw-algo-stream-websocket-frame-relay`), or a rendered RFC 9457 gateway error

**Steps**:
1. [x] - `p1` - Recognize the request as an upgrade request: method `GET`, a `Connection` header whose value contains the `Upgrade` token (case-insensitive), and an `Upgrade` header value of `websocket` - `inst-stream-websocket-negotiation-01`
2. [x] - `p1` - Apply `cpt-cf-oagw-feature-proxy-core`'s alias resolution, route matching, and header-processing rules to this request identically to a plain request, with exactly two headers exempted from the general hop-by-hop stripping rule: `Connection` and `Upgrade`, both of which are members of proxy-core's strip set (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) and both of which are required to complete the handshake. The `Sec-WebSocket-*` headers need no exemption because they were never in that strip set; they are unaffected by the hop-by-hop rule and pass through normally under the configured passthrough mode like any other request header - `inst-stream-websocket-negotiation-02`
3. [x] - `p1` - Forward the handshake request, including the preserved upgrade-related headers, to the resolved upstream endpoint - `inst-stream-websocket-negotiation-03`
4. [x] - `p1` - **IF** the upstream responds with `101 Switching Protocols` and a matching `Sec-WebSocket-Accept` - `inst-stream-websocket-negotiation-04`
   1. [x] - `p1` - Relay the `101` response and its headers to the client, completing the upgrade on both sides - `inst-stream-websocket-negotiation-05`
   2. [x] - `p1` - **RETURN** the upgraded bidirectional channel - `inst-stream-websocket-negotiation-06`
5. [x] - `p1` - **ELSE** the upstream returns a non-`101` status, refuses the connection, or the attempt fails or times out - `inst-stream-websocket-negotiation-07`
   1. [x] - `p1` - Render an ordinary RFC 9457 gateway error to the client with `X-OAGW-Error-Source: gateway`, since no upgrade has occurred and a normal HTTP error response is still possible - `inst-stream-websocket-negotiation-08`
   2. [x] - `p1` - **RETURN** the rendered gateway error - `inst-stream-websocket-negotiation-09`

### WebSocket Bidirectional Frame Relay

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-stream-websocket-frame-relay`

**Input**: the upgraded client channel and the upgraded upstream channel produced by `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`

**Output**: frames relayed on both channels until the session closes

**Steps**:
1. [x] - `p1` - **FOR EACH** frame received from the client channel - `inst-stream-websocket-frame-relay-01`
   1. [x] - `p1` - Relay the frame to the upstream channel unmodified, preserving its frame type (text or binary) - `inst-stream-websocket-frame-relay-02`
2. [x] - `p1` - **FOR EACH** frame received from the upstream channel - `inst-stream-websocket-frame-relay-03`
   1. [x] - `p1` - Relay the frame to the client channel unmodified, preserving its frame type (text or binary) - `inst-stream-websocket-frame-relay-04`
3. [x] - `p1` - **IF** a close frame is received from either channel - `inst-stream-websocket-frame-relay-05`
   1. [x] - `p1` - Propagate the close frame, including its close code and reason, to the other channel - `inst-stream-websocket-frame-relay-06`
   2. [x] - `p1` - Close both underlying connections - `inst-stream-websocket-frame-relay-07`
4. [x] - `p1` - **ELSE IF** either channel's underlying connection terminates abnormally without a close frame - `inst-stream-websocket-frame-relay-08`
   1. [x] - `p1` - Close the other channel's connection and hand off to `cpt-cf-oagw-algo-stream-abort-handling`'s connection-level-closure branch - `inst-stream-websocket-frame-relay-09`
5. [x] - `p1` - **RETURN** once both channels are closed - `inst-stream-websocket-frame-relay-10`

### Stream and Upgrade Abort Handling

- [x] `p2` - **ID**: `cpt-cf-oagw-algo-stream-abort-handling`

**Input**: an in-progress SSE stream or WebSocket session, and the point at which the connection failed (pre-commit vs. post-commit for SSE; pre-handshake vs. post-handshake for WebSocket)

**Output**: either the rendered RFC 9457 `StreamAborted` gateway error, or a connection-level close plus audit-log/error-metric recording

**Steps**:
1. [x] - `p1` - **IF** the upstream connection for an SSE stream fails before the response status line and headers have been committed to the client - `inst-stream-abort-handling-01`
   1. [x] - `p1` - Render the RFC 9457 `StreamAborted` (`502`) gateway error with `X-OAGW-Error-Source: gateway`, per the documented Error Response Format table - `inst-stream-abort-handling-02`
2. [x] - `p1` - **ELSE IF** the upstream connection for an SSE stream fails after the response status line and headers have already been committed to the client - `inst-stream-abort-handling-03`
   1. [x] - `p1` - Terminate the client connection without emitting a second response or a new status line - `inst-stream-abort-handling-04`
   2. [x] - `p1` - Record the `StreamAborted` condition in the audit log and error metrics - `inst-stream-abort-handling-05`
3. [x] - `p1` - **ELSE IF** a WebSocket handshake attempt fails before completion (before `101 Switching Protocols` reaches the client) - `inst-stream-abort-handling-06`
   1. [x] - `p1` - Render the applicable documented RFC 9457 gateway error as an ordinary HTTP response, per `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation` step 5 - `inst-stream-abort-handling-07`
4. [x] - `p1` - **ELSE** the failure occurs on a WebSocket session after the handshake has completed - `inst-stream-abort-handling-08`
   1. [x] - `p1` - Close the underlying connection on both sides (no HTTP response is possible post-handshake) - `inst-stream-abort-handling-09`
   2. [x] - `p1` - Record the `StreamAborted` condition in the audit log and error metrics - `inst-stream-abort-handling-10`
5. [x] - `p1` - **RETURN** the applicable outcome from the branch taken above - `inst-stream-abort-handling-11`

## 4. States (CDSL)

### SSE Stream Session State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-stream-sse-session`

**States**: Opening, Streaming, Closed, Aborted

**Initial State**: Opening

**Transitions**:
1. [x] - `p1` - **FROM** Opening **TO** Streaming **WHEN** the upstream response is classified as an event stream and its status line/headers have been committed to the client - `inst-stream-sse-session-state-01`
2. [x] - `p1` - **FROM** Opening **TO** Aborted **WHEN** the upstream connection fails before any response has been committed to the client (pre-commit abort) - `inst-stream-sse-session-state-02`
3. [x] - `p1` - **FROM** Streaming **TO** Closed **WHEN** the upstream closes the connection normally after delivering all events, or the client disconnects - `inst-stream-sse-session-state-03`
4. [x] - `p1` - **FROM** Streaming **TO** Aborted **WHEN** the upstream connection fails unexpectedly after the response has been committed (post-commit abort) - `inst-stream-sse-session-state-04`

`Closed` and `Aborted` are terminal states.

### WebSocket Session State Machine

- [x] `p2` - **ID**: `cpt-cf-oagw-state-stream-websocket-session`

**States**: HandshakePending, Open, Closing, Closed, Aborted

**Initial State**: HandshakePending

**Transitions**:
1. [x] - `p1` - **FROM** HandshakePending **TO** Open **WHEN** the upstream returns `101 Switching Protocols` and the gateway completes the upgrade with the client - `inst-stream-websocket-session-state-01`
2. [x] - `p1` - **FROM** HandshakePending **TO** Aborted **WHEN** the upstream refuses or fails the handshake, or the handshake attempt times out - `inst-stream-websocket-session-state-02`
3. [x] - `p1` - **FROM** Open **TO** Closing **WHEN** either side sends a close frame - `inst-stream-websocket-session-state-03`
4. [x] - `p1` - **FROM** Closing **TO** Closed **WHEN** the close frame and its close code have been propagated to the other side and both sides have closed their connections - `inst-stream-websocket-session-state-04`
5. [x] - `p1` - **FROM** Open **TO** Aborted **WHEN** the connection terminates abnormally without a close frame - `inst-stream-websocket-session-state-05`

`Closed` and `Aborted` are terminal states.

## 5. Definitions of Done

### SSE Detection and Incremental Forwarding

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-sse-detect-and-forward`

The system **MUST** detect that an upstream response is an event stream by its `Content-Type` and forward each portion of the response to the client as it is received, without buffering the complete response body in memory.

**Implements**:
- `cpt-cf-oagw-flow-stream-sse-consumption`
- `cpt-cf-oagw-algo-stream-sse-detect-relay`

**Constraints**: `cpt-cf-oagw-principle-no-cache`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`
- Entities: SSE event-forwarding state

### SSE Connection Lifecycle

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-sse-lifecycle`

The system **MUST** implement the SSE connection lifecycle exactly as `cpt-cf-oagw-state-stream-sse-session` models it: a clean transition to `Streaming` on open, a clean transition to `Closed` when the upstream finishes, propagation of a client disconnect to a corresponding upstream-connection close, and the `Aborted` transition (via `cpt-cf-oagw-algo-stream-abort-handling`) when the upstream fails mid-stream.

**Implements**:
- `cpt-cf-oagw-flow-stream-sse-consumption`
- `cpt-cf-oagw-state-stream-sse-session`

**Touches**:
- Entities: Stream session, SSE event-forwarding state

### WebSocket Handshake via Shared Resolution Path

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-websocket-handshake`

The system **MUST** recognize an `Upgrade: websocket` request on the proxy path, resolve it through the identical alias/route/header path `cpt-cf-oagw-feature-proxy-core` uses for a plain request, preserve the upgrade-related headers (`Upgrade`, `Connection`, `Sec-WebSocket-*`) required to complete the handshake, and render an ordinary RFC 9457 gateway error for any failure that occurs before the handshake completes.

**Implements**:
- `cpt-cf-oagw-flow-stream-websocket-session`
- `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path_suffix}`
- Entities: WebSocket upgrade context

### WebSocket Bidirectional Frame Relay and Close Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-websocket-frame-relay`

The system **MUST** relay text and binary frames verbatim in both directions once the upgrade completes, and **MUST** propagate a close frame's close code from whichever side initiates it to the other side before closing both connections.

**Implements**:
- `cpt-cf-oagw-flow-stream-websocket-session`
- `cpt-cf-oagw-algo-stream-websocket-frame-relay`
- `cpt-cf-oagw-state-stream-websocket-session`

**Touches**:
- Entities: Stream session

### StreamAborted Error and Committed-Response Behavior

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-abort-error`

The system **MUST** render the `StreamAborted` (`502`) RFC 9457 gateway error with `X-OAGW-Error-Source: gateway` when an in-progress SSE stream or a pre-handshake WebSocket upgrade is aborted before any response has been committed to the client, and **MUST** instead terminate the client connection — without a second HTTP response — while recording `StreamAborted` in the audit log and error metrics when the abort occurs after the response status line (SSE) or the handshake (WebSocket) has already been committed.

**Implements**:
- `cpt-cf-oagw-algo-stream-abort-handling`

**Touches**:
- Entities: Stream session

### Request Timeout Does Not Sever Committed Streams or Sessions

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-timeout-exemption`

The system **MUST NOT** apply the gear's configured `proxy_timeout_secs` request timeout to the full wall-clock duration of an open SSE stream or an upgraded WebSocket session once the response has been committed or the handshake has completed. `proxy_timeout_secs` continues to bound only the pre-commit/pre-handshake phase — connecting to the upstream and receiving its initial response or handshake reply — identically to how `cpt-cf-oagw-feature-proxy-core` already applies it to a plain request's forwarding step. This is a feature-level application of the existing `proxy_timeout_secs` configuration surface (owned by `cpt-cf-oagw-feature-gear-foundation`), not a new configuration key or architectural decision: without this exemption, a naive request timeout would sever every long-lived stream or socket, which neither `PRD.md` nor `DESIGN.md` intend and which would make SSE/WebSocket support unusable in practice.

The override is anchored rather than undeclared. It relaxes a MUST-level guarantee that `cpt-cf-oagw-feature-proxy-core` states for a plain request — one deadline covering connection establishment, request transmission and response receipt — and the sanction for relaxing it is the long-lived-transfer transition that `cpt-cf-oagw-state-proxy-request-lifecycle` declares for exactly this purpose: proxy-core extends its `Relaying` state into a streaming or upgraded-connection state that survives many events or frames, and it is that extended state, not a new one, whose wall-clock duration `proxy_timeout_secs` no longer bounds. The identifier is therefore listed under **Implements** below, so the relaxation is traceable to the state machine that authorises it.

**Implements**:
- `cpt-cf-oagw-flow-stream-sse-consumption`
- `cpt-cf-oagw-flow-stream-websocket-session`
- `cpt-cf-oagw-state-proxy-request-lifecycle`

**Touches**:
- Entities: Stream session

### Plaintext Upstream Streaming Under `allow_http_upstream`

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-plaintext-upstream`

The system **MUST** permit SSE forwarding and WebSocket upgrade proxying to a plaintext `http`/`ws` upstream endpoint when the gear's `allow_http_upstream` configuration flag is enabled, applying the identical `allow_http_upstream` gate `cpt-cf-oagw-feature-proxy-core` enforces for a plain request's outbound connection. Streaming and upgrades are not restricted to TLS (`https`/`wss`) schemes only.

**Implements**:
- `cpt-cf-oagw-flow-stream-sse-consumption`
- `cpt-cf-oagw-flow-stream-websocket-session`

**Constraints**: `cpt-cf-oagw-constraint-https-only` (lifted when `allow_http_upstream` is `true`, per the graded configuration)

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`

## 6. Acceptance Criteria

- [x] An SSE proxy request delivers at least one event to the client before the upstream has sent its final event or closed the connection — events arrive incrementally, not only after the upstream completes.
- [x] When the client disconnects while an SSE stream is open, the corresponding upstream connection is observably closed as part of the same request lifecycle.
- [x] A completed WebSocket echo round-trip through the proxy path succeeds: a client-sent text frame and a client-sent binary frame are each relayed to the upstream and the corresponding echoed frame is relayed back to the client unmodified.
- [x] A close frame carrying a specific close code, initiated by either the client or the upstream, is observed on the other side carrying the same close code.
- [x] An upstream that aborts an SSE stream before any response bytes are committed to the client causes the gateway to return `502` with GTS type `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` and header `X-OAGW-Error-Source: gateway`.
- [x] An upstream that aborts an SSE stream after the response status line/headers have already been committed to the client results in the client connection being closed without a second HTTP response, with the abort recorded in the audit log/error metrics.
- [x] A WebSocket upgrade request whose handshake fails before completion (e.g., the upstream returns a non-`101` status) yields an ordinary RFC 9457 `application/problem+json` gateway error response, not a raw connection drop.
- [x] An open SSE stream or an upgraded WebSocket session that remains active longer than the configured `proxy_timeout_secs` after being committed/upgraded is not unilaterally terminated by the request timeout.
- [x] An SSE proxy request and a WebSocket upgrade request each succeed against a plaintext (`http`/`ws`) upstream endpoint when `allow_http_upstream` is enabled.
- [x] A request whose resolved upstream endpoint declares the `wt` scheme, or a route matched as a gRPC streaming call, is not handled by this feature's SSE-detection or WebSocket-upgrade logic (no WebTransport or gRPC-streaming relay is exercised).
