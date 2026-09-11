# Feature: Proxy Streaming — SSE and WebSocket

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [SSE Streaming Relay Flow](#sse-streaming-relay-flow)
  - [WebSocket Upgrade and Relay Flow](#websocket-upgrade-and-relay-flow)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [SSE Detection and Streaming Relay Algorithm](#sse-detection-and-streaming-relay-algorithm)
  - [Upgrade Header Reconstruction Algorithm](#upgrade-header-reconstruction-algorithm)
  - [WebSocket Bidirectional Relay Algorithm](#websocket-bidirectional-relay-algorithm)
- [4. States (CDSL)](#4-states-cdsl)
  - [SSE Stream Lifecycle State Machine](#sse-stream-lifecycle-state-machine)
  - [WebSocket Session Lifecycle State Machine](#websocket-session-lifecycle-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SSE Incremental Relay](#sse-incremental-relay)
  - [Streaming Exemption from Body Limit and Request Timeout](#streaming-exemption-from-body-limit-and-request-timeout)
  - [SSE Connection Lifecycle and Stream Abort Reporting](#sse-connection-lifecycle-and-stream-abort-reporting)
  - [WebSocket Upgrade Negotiation with Explicit Header Reconstruction](#websocket-upgrade-negotiation-with-explicit-header-reconstruction)
  - [WebSocket Bidirectional Relay and Close Propagation](#websocket-bidirectional-relay-and-close-propagation)
  - [Failed Upgrade Reported as a Gateway Error](#failed-upgrade-reported-as-a-gateway-error)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-proxy-streaming-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-proxy-streaming`

## 1. Feature Context

### 1.1 Overview

This feature extends the single proxy endpoint built by the HTTP proxy feature so it also
carries Server-Sent-Events (SSE, a one-way streaming response format) and WebSocket upgrade
traffic, without introducing a second endpoint or a separate resolution path.

### 1.2 Purpose

`cpt-cf-oagw-actor-app-developer` consumes external APIs that stream rather than return a
single buffered body — chat-completion SSE feeds and bidirectional WebSocket protocols are
the two concrete cases named in the PRD. Proxying, as a capability, is not complete until both
of these forward through the same alias, route, guard, and authorization path as an ordinary
HTTP request; this feature adds that missing half.

**Requirements**: `cpt-cf-oagw-fr-streaming`

**Use case**: `cpt-cf-oagw-usecase-sse-streaming`

**Principles**: None newly covered here; `cpt-cf-oagw-principle-no-retry` and
`cpt-cf-oagw-principle-error-source` apply to streaming traffic exactly as they apply to plain
HTTP, and remain attributed to `cpt-cf-oagw-feature-proxy-data-plane-http`.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Opens an SSE request or a WebSocket upgrade through the proxy endpoint and consumes the resulting stream or session. |
| `cpt-cf-oagw-actor-upstream-service` | The external SSE emitter or WebSocket peer OAGW (Outbound API Gateway) dials on the app developer's behalf. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) §5.4 `cpt-cf-oagw-fr-streaming`, §8 `cpt-cf-oagw-usecase-sse-streaming`
- **Design**: [DESIGN.md](../DESIGN.md) §3.2 Headers Transformation (`cpt-cf-oagw-interface-api`), Error Response Format table (`StreamAborted`, `ProtocolError`)
- **ADR**: [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md) (`cpt-cf-oagw-adr-error-source-distinction`)
- **Decomposition**: `cpt-cf-oagw-feature-proxy-streaming`
- **Dependencies**: `cpt-cf-oagw-feature-proxy-data-plane-http` (alias resolution, route matching, guard rules, and header transformation this feature reuses without restating; the authorization check that gates every proxy request also runs unchanged here)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-sse-streaming`

Both flows below assume the request has already passed the shared authorization, alias
resolution, route matching, and guard checks defined in
`cpt-cf-oagw-feature-proxy-data-plane-http`; those steps are referenced here, not repeated.

### SSE Streaming Relay Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-streaming-sse-relay`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The upstream answers with a `text/event-stream` body and events reach the client
  incrementally, as they are produced, rather than after upstream completion.
- The upstream finishes and closes its connection; the client connection is closed in turn and
  the closure is logged.
- The client disconnects mid-stream; the upstream connection is closed and no further reads
  occur.

**Error Scenarios**:
- The upstream connection fails before any response head is received — an ordinary gateway
  error, exactly as in the non-streaming HTTP path.
- The upstream connection fails after the stream has begun — the already-sent status code
  cannot change, so the stream is terminated and reported as `StreamAborted` (502) per
  `cpt-cf-oagw-adr-error-source-distinction`.

**Steps**:
1. [ ] - `p1` - App developer sends a proxy request to `{METHOD} /oagw/v1/proxy/{alias}[/path][?query]`; the request commonly carries `Accept: text/event-stream`, but that header is not the classification signal - `inst-sse-1`
2. [ ] - `p1` - API: reuse alias resolution, route match, guard checks, and authorization from `cpt-cf-oagw-feature-proxy-data-plane-http` - `inst-sse-2`
3. [ ] - `p1` - System opens the upstream connection within the configured `proxy_timeout_secs` budget, which bounds only reaching the response head - `inst-sse-3`
4. [ ] - `p1` - **IF** the upstream response's `Content-Type` is `text/event-stream` - `inst-sse-4`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-proxy-streaming-sse-detect` to switch from buffering to streaming relay - `inst-sse-4a`
   2. [ ] - `p1` - Forward each event to the client as it arrives, preserving SSE event framing unchanged - `inst-sse-4b`
5. [ ] - `p1` - **IF** the upstream closes its connection - `inst-sse-5`
   1. [ ] - `p1` - Close the client connection and log the stream-closed event - `inst-sse-5a`
6. [ ] - `p1` - **ELSE IF** the client disconnects - `inst-sse-6`
   1. [ ] - `p1` - Close the upstream connection and stop reading from it - `inst-sse-6a`
7. [ ] - `p1` - **ELSE IF** the upstream connection fails after the stream has begun - `inst-sse-7`
   1. [ ] - `p1` - Terminate the stream and report `StreamAborted` (502, `X-OAGW-Error-Source: gateway` or `upstream` depending on failure origin), leaving the already-sent status unchanged - `inst-sse-7a`
8. [ ] - `p1` - **RETURN** the streamed response, terminated by upstream completion, client disconnect, or abort - `inst-sse-8`

### WebSocket Upgrade and Relay Flow

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-proxy-streaming-websocket-relay`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The upstream answers `101 Switching Protocols`; the client's own upgrade completes with
  `101`, and frames flow in both directions until either side closes.
- A close frame sent by either the client or the upstream is relayed, with its status code, to
  the other side.

**Error Scenarios**:
- The upstream refuses the dial, or answers with any status other than `101` — reported as a
  gateway error carrying `X-OAGW-Error-Source: gateway`.
- The client disconnects abruptly without a close frame — the upstream connection is closed.
- The upstream disconnects abruptly without a close frame — the client connection is closed.

**Steps**:
1. [ ] - `p1` - App developer sends `{METHOD} /oagw/v1/proxy/{alias}[/path]` with `Upgrade: websocket` and `Connection: Upgrade` - `inst-ws-1`
2. [ ] - `p1` - API: reuse alias resolution, route match, guard checks, and authorization from `cpt-cf-oagw-feature-proxy-data-plane-http` - `inst-ws-2`
3. [ ] - `p1` - **IF** the request carries `Upgrade: websocket` and `Connection: Upgrade` - `inst-ws-3`
   1. [ ] - `p1` - Classify the request as an upgrade instead of a buffered HTTP request - `inst-ws-3a`
4. [ ] - `p1` - Run `cpt-cf-oagw-algo-proxy-streaming-header-reconstruction` to rebuild the `Upgrade` and `Connection` headers for the upstream dial, since the ordinary hop-by-hop stripping step removes both by default - `inst-ws-4`
5. [ ] - `p1` - System dials the upstream, within the configured `proxy_timeout_secs` budget, carrying the client's original `Sec-WebSocket-Key` forwarded unmodified, the negotiated subprotocol, and any required headers; this budget bounds only reaching the `101 Switching Protocols` response and does not apply to the established relay session - `inst-ws-5`
6. [ ] - `p1` - **IF** the upstream answers `101 Switching Protocols` - `inst-ws-6`
   1. [ ] - `p1` - Complete the client-facing handshake with `101 Switching Protocols`, relaying the upstream's `Sec-WebSocket-Accept` back to the client unmodified - `inst-ws-6a`
   2. [ ] - `p1` - Run `cpt-cf-oagw-algo-proxy-streaming-ws-relay` to relay frames bidirectionally until either side closes - `inst-ws-6b`
7. [ ] - `p1` - **ELSE** - `inst-ws-7`
   1. [ ] - `p1` - **RETURN** a gateway error (`ProtocolError`, 502, `X-OAGW-Error-Source: gateway`) - `inst-ws-7a`
8. [ ] - `p1` - **IF** the client sends a close frame - `inst-ws-8`
   1. [ ] - `p1` - Relay the close frame and status code to the upstream, then close the upstream connection - `inst-ws-8a`
9. [ ] - `p1` - **ELSE IF** the upstream sends a close frame - `inst-ws-9`
   1. [ ] - `p1` - Relay the close frame and status code to the client, then close the client connection - `inst-ws-9a`
10. [ ] - `p1` - **RETURN** the session ended, with both connections closed - `inst-ws-10`

## 3. Processes / Business Logic (CDSL)

### SSE Detection and Streaming Relay Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-proxy-streaming-sse-detect`

**Input**: Upstream response head (status, headers) received after `cpt-cf-oagw-feature-proxy-data-plane-http` has resolved the route

**Output**: Either an active streaming relay session, or a buffered response handed back to the ordinary HTTP response path

**Steps**:
1. [ ] - `p1` - Parse and normalize the `Content-Type` header from the upstream response head - `inst-sd-1`
2. [ ] - `p1` - **IF** `Content-Type` is `text/event-stream` - `inst-sd-2`
   1. [ ] - `p1` - Mark the request as an SSE stream: suspend the 100MB body cap for the remainder of the connection, since it applies only to buffered bodies - `inst-sd-2a`
   2. [ ] - `p1` - Confine the request timeout to having reached this response head; it no longer bounds the stream's remaining lifetime - `inst-sd-2b`
   3. [ ] - `p1` - **TRY** - `inst-sd-2c`
      1. [ ] - `p1` - **FOR EACH** chunk received from the upstream, forward it to the client unmodified, without altering SSE event framing, continuing until the upstream closes or the client disconnects - `inst-sd-2c1`
   4. [ ] - `p1` - **CATCH** an upstream connection failure occurring after the stream has begun - `inst-sd-2d`
      1. [ ] - `p1` - Terminate the stream and report `StreamAborted` (502) without altering the status already sent to the client - `inst-sd-2d1`
   5. [ ] - `p1` - **RETURN** the stream terminated by close, disconnect, or abort - `inst-sd-2e`
3. [ ] - `p1` - **ELSE** - `inst-sd-3`
   1. [ ] - `p1` - **RETURN** control to the buffered HTTP response handling of `cpt-cf-oagw-feature-proxy-data-plane-http` - `inst-sd-3a`

### Upgrade Header Reconstruction Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-proxy-streaming-header-reconstruction`

**Input**: Inbound request headers, the negotiated `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, and any negotiated subprotocol

**Output**: The outbound header set sent to the upstream on the upgrade dial

**Steps**:
1. [ ] - `p1` - Apply the standard hop-by-hop stripping documented in DESIGN.md's Headers Transformation table, which removes `Connection` and `Upgrade`, among others, by default - `inst-hr-1`
2. [ ] - `p1` - **IF** the request was classified as a WebSocket upgrade - `inst-hr-2`
   1. [ ] - `p1` - Re-add `Upgrade: websocket` and `Connection: Upgrade` explicitly for the upstream dial, since the default stripping step removes both and would otherwise silently downgrade the dial to a plain request - `inst-hr-2a`
   2. [ ] - `p1` - Forward the client's original `Sec-WebSocket-Key` unmodified, together with `Sec-WebSocket-Version` and any negotiated subprotocol, through to the outbound headers - `inst-hr-2b`
3. [ ] - `p1` - **ELSE** - `inst-hr-3`
   1. [ ] - `p1` - Leave `Connection` and `Upgrade` stripped, as for any ordinary proxied request - `inst-hr-3a`
4. [ ] - `p1` - **RETURN** the outbound header set - `inst-hr-4`

### WebSocket Bidirectional Relay Algorithm

- [ ] `p1` - **ID**: `cpt-cf-oagw-algo-proxy-streaming-ws-relay`

**Input**: An established client socket and upstream socket pair, following a successful `101` handshake on both sides

**Output**: Frames relayed in both directions until the session ends

**Steps**:
1. [ ] - `p1` - Confine `proxy_timeout_secs` to having reached the `101 Switching Protocols` response; it no longer bounds this established relay session's remaining lifetime - `inst-wr-1`
2. [ ] - `p1` - **FOR EACH** event, waiting on whichever side produces the next one - `inst-wr-2`
   1. [ ] - `p1` - **IF** a frame arrives from the client - `inst-wr-2a`
      1. [ ] - `p1` - Forward the frame to the upstream unmodified - `inst-wr-2a1`
   2. [ ] - `p1` - **ELSE IF** a frame arrives from the upstream - `inst-wr-2b`
      1. [ ] - `p1` - Forward the frame to the client unmodified - `inst-wr-2b1`
   3. [ ] - `p1` - **ELSE IF** the client sends a close frame - `inst-wr-2c`
      1. [ ] - `p1` - Relay the close frame and its status code to the upstream, then close the upstream socket - `inst-wr-2c1`
   4. [ ] - `p1` - **ELSE IF** the upstream sends a close frame - `inst-wr-2d`
      1. [ ] - `p1` - Relay the close frame and its status code to the client, then close the client socket - `inst-wr-2d1`
   5. [ ] - `p1` - **ELSE IF** the client disconnects without a close frame - `inst-wr-2e`
      1. [ ] - `p1` - Close the upstream socket - `inst-wr-2e1`
   6. [ ] - `p1` - **ELSE IF** the upstream disconnects without a close frame - `inst-wr-2f`
      1. [ ] - `p1` - Close the client socket - `inst-wr-2f1`
3. [ ] - `p1` - **RETURN** the session closed - `inst-wr-3`

## 4. States (CDSL)

### SSE Stream Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-proxy-streaming-sse-lifecycle`

**States**: idle, establishing, open, closing, closed

**Initial State**: idle

**Transitions**:
1. [ ] - `p1` - **FROM** idle **TO** establishing **WHEN** the client sends a proxy request; `Accept: text/event-stream` is commonly sent but does not gate this transition - `inst-sl-1`
2. [ ] - `p1` - **FROM** establishing **TO** open **WHEN** the upstream response head arrives with `Content-Type: text/event-stream` - `inst-sl-2`
3. [ ] - `p1` - **FROM** establishing **TO** closed **WHEN** the upstream connection fails before any response head arrives — an ordinary gateway error, since no stream was ever opened - `inst-sl-3`
4. [ ] - `p1` - **FROM** establishing **TO** closed **WHEN** the response head arrives without `Content-Type: text/event-stream`, handing control back to the buffered HTTP response path since no SSE stream was opened - `inst-sl-4`
5. [ ] - `p1` - **FROM** open **TO** closing **WHEN** the upstream closes its connection - `inst-sl-5`
6. [ ] - `p1` - **FROM** open **TO** closing **WHEN** the client disconnects - `inst-sl-6`
7. [ ] - `p1` - **FROM** open **TO** closing **WHEN** the upstream connection fails after the stream has begun, reported as `StreamAborted` - `inst-sl-7`
8. [ ] - `p1` - **FROM** closing **TO** closed **WHEN** the counterpart connection has been closed and the closure event logged - `inst-sl-8`

### WebSocket Session Lifecycle State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-proxy-streaming-ws-lifecycle`

**States**: idle, establishing, open, closing, closed

**Initial State**: idle

**Transitions**:
1. [ ] - `p1` - **FROM** idle **TO** establishing **WHEN** the client sends a request with `Upgrade: websocket` and `Connection: Upgrade` - `inst-wl-1`
2. [ ] - `p1` - **FROM** establishing **TO** open **WHEN** the upstream answers `101 Switching Protocols` and the client-facing handshake completes - `inst-wl-2`
3. [ ] - `p1` - **FROM** establishing **TO** closed **WHEN** the upstream refuses the dial or answers a status other than `101`, reported as a gateway error with `X-OAGW-Error-Source: gateway` - `inst-wl-3`
4. [ ] - `p1` - **FROM** open **TO** closing **WHEN** either side sends a close frame - `inst-wl-4`
5. [ ] - `p1` - **FROM** open **TO** closing **WHEN** either side disconnects without a close frame - `inst-wl-5`
6. [ ] - `p1` - **FROM** closing **TO** closed **WHEN** both sockets are closed and any close status code has been relayed to the other side - `inst-wl-6`

## 5. Definitions of Done

### SSE Incremental Relay

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-sse-relay`

The system **MUST** detect a `text/event-stream` upstream response by its `Content-Type` and
forward events to the client incrementally, as they arrive, without buffering the response to
completion first and without altering SSE event framing.

**Implements**:
- `cpt-cf-oagw-flow-proxy-streaming-sse-relay`
- `cpt-cf-oagw-algo-proxy-streaming-sse-detect`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`
- Entities: ProxyContext (from `cpt-cf-oagw-feature-proxy-data-plane-http`)

### Streaming Exemption from Body Limit and Request Timeout

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-body-limit-exempt`

The system **MUST** exempt an established SSE stream from the 100MB body limit
(`cpt-cf-oagw-constraint-body-limit`), and **MUST** confine `proxy_timeout_secs` to the time
needed to reach the upstream response head rather than the remaining lifetime of an
already-open stream.

**Implements**:
- `cpt-cf-oagw-algo-proxy-streaming-sse-detect`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`

### SSE Connection Lifecycle and Stream Abort Reporting

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-sse-lifecycle`

The system **MUST** close the client connection and log the event when the upstream closes,
**MUST** close the upstream connection and stop reading when the client disconnects, and
**MUST** report an upstream failure occurring after the stream has begun as `StreamAborted`
(502), leaving any status already sent to the client unchanged, per
`cpt-cf-oagw-adr-error-source-distinction`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-streaming-sse-relay`
- `cpt-cf-oagw-state-proxy-streaming-sse-lifecycle`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`

### WebSocket Upgrade Negotiation with Explicit Header Reconstruction

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-ws-upgrade`

The system **MUST** perform the client-facing upgrade handshake and dial the upstream with a
matching upgrade request that explicitly re-adds the `Upgrade` and `Connection` headers after
the ordinary hop-by-hop stripping step removes them, forwarding the client's original
`Sec-WebSocket-Key` unmodified along with the negotiated subprotocol and any required headers,
completing the client handshake with `101` only once the upstream itself answers `101`, and
**MUST** relay the upstream's `Sec-WebSocket-Accept` back to the client unmodified in that `101`
response.

**Implements**:
- `cpt-cf-oagw-flow-proxy-streaming-websocket-relay`
- `cpt-cf-oagw-algo-proxy-streaming-header-reconstruction`
- `cpt-cf-oagw-state-proxy-streaming-ws-lifecycle`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`

### WebSocket Bidirectional Relay and Close Propagation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-ws-relay`

The system **MUST** relay frames bidirectionally between client and upstream once the upgrade
completes, **MUST** relay a close frame and its status code in both directions, **MUST**
propagate a client disconnect to the upstream connection and an upstream close to the client
connection, and **MUST** confine `proxy_timeout_secs` to reaching the `101 Switching Protocols`
response, never tearing down an already-established relay session once that response has
arrived.

**Implements**:
- `cpt-cf-oagw-flow-proxy-streaming-websocket-relay`
- `cpt-cf-oagw-algo-proxy-streaming-ws-relay`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`

### Failed Upgrade Reported as a Gateway Error

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-proxy-streaming-ws-upgrade-failure`

The system **MUST** report a failed upgrade — the upstream refusing the dial, or answering with
any status other than `101 Switching Protocols` — as a gateway error carrying
`X-OAGW-Error-Source: gateway`, consistent with `cpt-cf-oagw-adr-error-source-distinction`.

**Implements**:
- `cpt-cf-oagw-flow-proxy-streaming-websocket-relay`
- `cpt-cf-oagw-state-proxy-streaming-ws-lifecycle`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}[/path]`

## 6. Acceptance Criteria

- [ ] Against a local stub upstream that emits SSE events with a delay between each one, the client receives each event as it is produced rather than receiving all events in one buffered block after the delay.
- [ ] When the stub upstream closes its SSE connection, the client observes the stream close, and a closure event is logged.
- [ ] When the client closes its connection mid-stream, the stub upstream observes its connection closed by the gateway.
- [ ] Against a local stub upstream that sends a `200` head with `Content-Type: text/event-stream`, emits some events, then drops its connection mid-stream, the client already received the `200` head unchanged, and the stream ends with `StreamAborted` (502) rather than any other status.
- [ ] Against a local stub upstream configured with `proxy_timeout_secs` set to `2` seconds, an SSE stream that stays open and keeps emitting events for longer than 2 seconds is not torn down at the 2-second mark; events keep arriving past that point.
- [ ] A WebSocket upgrade request through `{METHOD} /oagw/v1/proxy/{alias}[/path]` against a stub upstream that accepts the upgrade completes with `101 Switching Protocols`, and a text frame sent by the client is echoed back by the stub and received by the client.
- [ ] A close frame sent by the client is observed by the stub upstream, and a close frame sent by the stub upstream is observed by the client, in each case with the same status code.
- [ ] A WebSocket upgrade request against a stub upstream configured to refuse the upgrade (or to answer a non-`101` status) returns a gateway error response carrying `X-OAGW-Error-Source: gateway`.
- [ ] The SSE and WebSocket paths above complete successfully against a plaintext (`http`/`ws`) stub upstream, consistent with `allow_http_upstream: true`.
