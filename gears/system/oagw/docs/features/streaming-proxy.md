# Feature: Streaming and WebSocket Proxy


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [SSE Stream Session](#sse-stream-session)
  - [SSE Client Disconnect](#sse-client-disconnect)
  - [SSE Stream Aborted](#sse-stream-aborted)
  - [WebSocket Session](#websocket-session)
  - [WebSocket Upgrade Rejected](#websocket-upgrade-rejected)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Event-Stream Detection](#event-stream-detection)
  - [Incremental Event Forwarding](#incremental-event-forwarding)
  - [Upgrade Negotiation](#upgrade-negotiation)
  - [Upgrade Header Preservation](#upgrade-header-preservation)
  - [Frame Relay](#frame-relay)
  - [Session Teardown](#session-teardown)
  - [Long-Lived Pipeline Reuse](#long-lived-pipeline-reuse)
- [4. States (CDSL)](#4-states-cdsl)
  - [SSE Stream Session State Machine](#sse-stream-session-state-machine)
  - [WebSocket Proxy Session State Machine](#websocket-proxy-session-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Event-Stream Response Detection](#event-stream-response-detection)
  - [Incremental Event Forwarding](#incremental-event-forwarding-1)
  - [Event-Stream Framing Preservation](#event-stream-framing-preservation)
  - [Upstream-Initiated Stream Close](#upstream-initiated-stream-close)
  - [Client-Initiated Stream Close](#client-initiated-stream-close)
  - [Mid-Flight Stream Abort Handling](#mid-flight-stream-abort-handling)
  - [Upgrade Request Recognition](#upgrade-request-recognition)
  - [Upstream-First Upgrade Ordering](#upstream-first-upgrade-ordering)
  - [Upgrade Header Survival Through Hop-by-Hop Stripping](#upgrade-header-survival-through-hop-by-hop-stripping)
  - [Bidirectional Frame Relay](#bidirectional-frame-relay)
  - [Close Propagation and Session Teardown](#close-propagation-and-session-teardown)
  - [Upgrade Refusal as Gateway Error](#upgrade-refusal-as-gateway-error)
  - [Long-Lived Pipeline and Timeout Policy](#long-lived-pipeline-and-timeout-policy)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-streaming-proxy-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-streaming-proxy`

## 1. Feature Context

### 1.1 Overview

This feature carries server-sent-event streams and WebSocket sessions through the same proxy path `/oagw/v1/proxy/{alias}/{path}` that plain HTTP requests already use. It replaces response buffering with incremental pass-through forwarding, negotiates WebSocket upgrades toward the upstream first, and relays frames in both directions until either side closes.

Long-lived connections are managed symmetrically: an upstream close ends the client connection, and a client disconnect ends the upstream connection. WebTransport is declared in the endpoint scheme set as `wt`, but this feature implements no WebTransport session flow, and that gap is a stated limitation rather than partial support.

### 1.2 Purpose

The streaming requirement demands server-sent-event proxying with open, close and error lifecycle handling, plus WebSocket session flows over the existing proxy surface. Many external services stream chat completions or telemetry as events, so buffering a whole response before answering the caller would break those integrations outright.

This feature extends the HTTP proxy data plane rather than duplicating it. Guard rules, header transformation, endpoint selection, error mapping and the plugin-invocation point are all established once in the HTTP proxy feature and reused here without modification.

**Requirements**: `cpt-cf-oagw-fr-streaming`

**Principles**: `cpt-cf-oagw-principle-no-retry`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Opens an event-stream request or a WebSocket upgrade through the proxy path and consumes relayed events and frames |
| `cpt-cf-oagw-actor-upstream-service` | Produces the event stream or accepts the WebSocket upgrade, and may close or abort the connection at any time |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Design elements**: `cpt-cf-oagw-component-model`, `cpt-cf-oagw-interface-api`, `cpt-cf-oagw-interface-proxy-api`
- **Sequences**: `cpt-cf-oagw-seq-proxy-flow`
- **ADRs**: `cpt-cf-oagw-adr-error-source-distinction`, `cpt-cf-oagw-adr-request-routing`
- **Dependencies**: `cpt-cf-oagw-feature-http-proxy` provides the proxy path, guard rules, header transformation, endpoint selection, the plaintext-connection policy, error mapping and the plugin-invocation point that both streaming transports reuse unchanged. Plugin chain execution itself belongs to `cpt-cf-oagw-feature-plugin-runtime`, which hooks the same invocation point, so this feature adds no direct edge to it.

The plaintext-connection policy is reused from the HTTP proxy feature and is not re-specified here. That policy refuses a plaintext endpoint with status `503` and the `LinkUnavailable` error type, grouping the `ws` scheme with the `http` scheme.

**Limitations**: WebTransport is not implemented. The endpoint scheme set accepts `wt`, and the streaming requirement names WebTransport session flows, but no WebTransport code path exists in this feature. A resolved endpoint whose scheme is `wt` is refused with a gateway-sourced protocol error instead of being upgraded. gRPC streaming, Starlark custom-plugin execution, the Redis second-level cache and distributed rate-limit synchronization are excluded here as well.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end behaviour of a streaming or upgraded proxy request.

**Use cases**: `cpt-cf-oagw-usecase-sse-streaming`

### SSE Stream Session

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-sse-stream-session`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The caller opens an event-stream request, receives each event as the upstream emits it, and the stream ends when the upstream closes it.
- Response headers reach the caller before the first event, carrying the event-stream content type unchanged from the upstream.

**Error Scenarios**:
- The upstream refuses the request or never returns headers, so a gateway-sourced problem response is returned before streaming starts.
- The upstream stream aborts after headers were forwarded, so the caller sees a truncated stream and a closed connection.

**Steps**:
1. [ ] - `p1` - Caller sends an event-stream request with header `Accept: text/event-stream` - `inst-sse-open-01`
2. [ ] - `p1` - API: GET /oagw/v1/proxy/{alias}/{path} (streaming request accepted, no body buffered) - `inst-sse-open-02`
3. [ ] - `p1` - Reuse the ordinary pipeline stages through `cpt-cf-oagw-algo-longlived-pipeline` - `inst-sse-open-03`
4. [ ] - `p1` - Open the upstream request against the selected endpoint and await response headers - `inst-sse-open-04`
5. [ ] - `p1` - Classify the response through `cpt-cf-oagw-algo-stream-detection` - `inst-sse-open-05`
6. [ ] - `p1` - **IF** the response media type is `text/event-stream` - `inst-sse-open-06`
   1. [ ] - `p1` - Forward the status and response headers immediately, preserving the event-stream content type - `inst-sse-open-07`
   2. [ ] - `p1` - Mark the session Open and enter `cpt-cf-oagw-algo-incremental-forwarding` - `inst-sse-open-08`
7. [ ] - `p1` - **ELSE** - `inst-sse-open-09`
   1. [ ] - `p1` - Hand the response back to the buffered plain-HTTP response path unchanged - `inst-sse-open-10`
8. [ ] - `p1` - **FOR EACH** chunk read from the upstream body - `inst-sse-open-11`
   1. [ ] - `p1` - Write the chunk to the client and flush it without any aggregation delay - `inst-sse-open-12`
9. [ ] - `p1` - **IF** the upstream signals end of stream - `inst-sse-open-13`
   1. [ ] - `p1` - Close the client response body and record the close with the correlation identifier - `inst-sse-open-14`
10. [ ] - `p1` - **RETURN** a completed stream with both halves closed - `inst-sse-open-15`

### SSE Client Disconnect

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-sse-client-disconnect`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The caller abandons an open stream, and the upstream connection is closed promptly instead of being left producing events.

**Error Scenarios**:
- The upstream ignores the close and keeps writing, so the session is dropped locally and the write failure is recorded.

**Steps**:
1. [ ] - `p1` - Caller closes the client connection while the stream is Open - `inst-sse-disc-01`
2. [ ] - `p1` - Detect the client-side close on the next write or through the connection-closed signal - `inst-sse-disc-02`
3. [ ] - `p1` - Mark the session Closing and stop reading further upstream chunks - `inst-sse-disc-03`
4. [ ] - `p1` - Invoke `cpt-cf-oagw-algo-session-teardown` to close the upstream half - `inst-sse-disc-04`
5. [ ] - `p1` - Record the client-initiated close with the correlation identifier and no error response - `inst-sse-disc-05`
6. [ ] - `p1` - **RETURN** a closed session with no pending upstream read - `inst-sse-disc-06`

### SSE Stream Aborted

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-sse-stream-aborted`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- An abort before response headers were forwarded produces a gateway-sourced problem response with the proper status and error source.

**Error Scenarios**:
- An abort after response headers were forwarded cannot change the status, so the caller only observes a truncated stream.

**Steps**:
1. [ ] - `p1` - Upstream connection fails or resets while the stream is Connecting or Open - `inst-sse-abort-01`
2. [ ] - `p1` - **IF** response headers have not yet been forwarded to the client - `inst-sse-abort-02`
   1. [ ] - `p1` - Return `502` with the stream-aborted problem type and `X-OAGW-Error-Source: gateway` - `inst-sse-abort-03`
3. [ ] - `p1` - **ELSE** - `inst-sse-abort-04`
   1. [ ] - `p1` - Stop writing events, close the client response body, and emit no problem body - `inst-sse-abort-05`
   2. [ ] - `p1` - Record the abort with the correlation identifier and the gateway error attribution - `inst-sse-abort-06`
4. [ ] - `p1` - Never reopen the upstream request, because the gateway performs no automatic retry - `inst-sse-abort-07`
5. [ ] - `p1` - **RETURN** a closed session in the Closed state - `inst-sse-abort-08`

### WebSocket Session

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-websocket-session`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The caller upgrades through the proxy path, exchanges text, binary and ping frames with the upstream, and closes with a code.
- The client upgrade completes only after the upstream has accepted its own upgrade handshake.

**Error Scenarios**:
- The upstream refuses the handshake, so no client upgrade happens and a gateway-sourced error is returned instead.
- Either peer drops without a close frame, so both halves are torn down and the drop is recorded.

**Steps**:
1. [ ] - `p1` - Caller sends an upgrade request carrying `Upgrade: websocket` and `Connection: Upgrade` - `inst-ws-sess-01`
2. [ ] - `p1` - API: GET /oagw/v1/proxy/{alias}/{path} (WebSocket upgrade, no request body) - `inst-ws-sess-02`
3. [ ] - `p1` - Recognise the upgrade through `cpt-cf-oagw-algo-upgrade-negotiation` - `inst-ws-sess-03`
4. [ ] - `p1` - Reuse the ordinary pipeline stages through `cpt-cf-oagw-algo-longlived-pipeline` - `inst-ws-sess-04`
5. [ ] - `p1` - Rebuild the handshake headers through `cpt-cf-oagw-algo-upgrade-header-preservation` - `inst-ws-sess-05`
6. [ ] - `p1` - Send the handshake to the selected endpoint and await its switching-protocols response - `inst-ws-sess-06`
7. [ ] - `p1` - **IF** the upstream accepted the upgrade - `inst-ws-sess-07`
   1. [ ] - `p1` - Complete the client upgrade with `101` and mark the session Open - `inst-ws-sess-08`
   2. [ ] - `p1` - Enter `cpt-cf-oagw-algo-frame-relay` for the life of the session - `inst-ws-sess-09`
8. [ ] - `p1` - **ELSE** - `inst-ws-sess-10`
   1. [ ] - `p1` - Continue with `cpt-cf-oagw-flow-websocket-upgrade-rejected` and never upgrade the client - `inst-ws-sess-11`
9. [ ] - `p1` - **IF** either peer sends a close frame - `inst-ws-sess-12`
   1. [ ] - `p1` - Forward the close frame with its code and reason to the paired peer - `inst-ws-sess-13`
   2. [ ] - `p1` - Invoke `cpt-cf-oagw-algo-session-teardown` for both halves - `inst-ws-sess-14`
10. [ ] - `p1` - **RETURN** a closed session with both halves released - `inst-ws-sess-15`

### WebSocket Upgrade Rejected

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-websocket-upgrade-rejected`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:
- A refusal is reported as a gateway-sourced problem response, and the client connection stays an ordinary HTTP connection.

**Error Scenarios**:
- The resolved endpoint scheme cannot serve WebSocket, including the declared but unimplemented WebTransport scheme.
- A plaintext `ws` endpoint is selected while plaintext upstream connections are disallowed, so the shared link-unavailable refusal applies.

**Steps**:
1. [ ] - `p1` - **IF** the resolved endpoint scheme is neither `wss` nor `ws` - `inst-ws-rej-01`
   1. [ ] - `p1` - Return `502` with the protocol-error problem type and `X-OAGW-Error-Source: gateway` - `inst-ws-rej-02`
2. [ ] - `p1` - **IF** the endpoint scheme is `ws` and `allow_http_upstream` is false - `inst-ws-rej-03`
   1. [ ] - `p1` - Return `503` with the link-unavailable problem type, opening no upstream connection - `inst-ws-rej-04`
3. [ ] - `p1` - **IF** the upstream answered the handshake with any status other than `101` - `inst-ws-rej-05`
   1. [ ] - `p1` - Return `502` with the protocol-error problem type and `X-OAGW-Error-Source: gateway` - `inst-ws-rej-06`
4. [ ] - `p1` - Release the pending upstream connection and leave the client connection unupgraded - `inst-ws-rej-07`
5. [ ] - `p1` - Record the refusal with the correlation identifier and the endpoint host - `inst-ws-rej-08`
6. [ ] - `p1` - **RETURN** a gateway-sourced problem response in `application/problem+json` - `inst-ws-rej-09`

The plaintext refusal above is the HTTP proxy feature's plaintext-connection policy reused unchanged, so this document restates neither its condition nor its status. The `502` protocol error stays reserved for a handshake answer other than `101`, and for a scheme that cannot serve the requested transport, such as `wt`.

## 3. Processes / Business Logic (CDSL)

Internal procedures shared by the flows above. They cover stream detection, incremental forwarding, upgrade negotiation, frame relay, teardown and the pipeline reuse policy.

### Event-Stream Detection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-stream-detection`

**Input**: Upstream response status and response headers, before any body byte is read

**Output**: A decision to stream through, or to fall back to the buffered plain-HTTP response path

**Steps**:
1. [ ] - `p1` - Read the response `Content-Type` header and lowercase its media type - `inst-detect-01`
2. [ ] - `p1` - Ignore media-type parameters such as `charset` when comparing the media type - `inst-detect-02`
3. [ ] - `p1` - **IF** the media type equals `text/event-stream` - `inst-detect-03`
   1. [ ] - `p1` - Select pass-through streaming and disable response body buffering - `inst-detect-04`
   2. [ ] - `p1` - Disable response transformation and re-encoding that would break event framing - `inst-detect-05`
   3. [ ] - `p1` - Keep the upstream `Content-Type` verbatim and add no `Content-Length` header - `inst-detect-06`
4. [ ] - `p1` - **ELSE** - `inst-detect-07`
   1. [ ] - `p1` - Select the buffered response path owned by the HTTP proxy feature - `inst-detect-08`
5. [ ] - `p1` - **RETURN** the selected response-handling mode - `inst-detect-09`

### Incremental Event Forwarding

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-incremental-forwarding`

**Input**: An open upstream event-stream body and an open client response body

**Output**: Client-visible events in upstream order, or a terminated session

**Steps**:
1. [ ] - `p1` - Read the next available chunk from the upstream body without waiting for more - `inst-fwd-01`
2. [ ] - `p1` - Write the chunk to the client in the exact byte order received - `inst-fwd-02`
3. [ ] - `p1` - Flush the client write immediately, applying no aggregation window or delay timer - `inst-fwd-03`
4. [ ] - `p1` - Never split, merge, reframe or reorder the event bytes crossing the gateway - `inst-fwd-04`
5. [ ] - `p1` - **TRY** - `inst-fwd-05`
   1. [ ] - `p1` - Continue reading and flushing until the upstream signals end of stream - `inst-fwd-06`
6. [ ] - `p1` - **CATCH** upstream read failure or client write failure - `inst-fwd-07`
   1. [ ] - `p1` - Invoke `cpt-cf-oagw-algo-session-teardown` and record the failing side - `inst-fwd-08`
7. [ ] - `p1` - **RETURN** the terminal outcome of the stream - `inst-fwd-09`

### Upgrade Negotiation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upgrade-negotiation`

**Input**: An inbound proxy request and the resolved upstream endpoint

**Output**: An accepted upstream upgrade ready for client completion, or a refusal

**Steps**:
1. [ ] - `p1` - **IF** the request method is `GET` and it carries `Upgrade: websocket` - `inst-nego-01`
   1. [ ] - `p1` - Confirm `Connection` lists the `Upgrade` token case-insensitively - `inst-nego-02`
   2. [ ] - `p1` - Confirm `Sec-WebSocket-Key` is present and `Sec-WebSocket-Version` is `13` - `inst-nego-03`
   3. [ ] - `p1` - Classify the request as a WebSocket upgrade instead of a plain request - `inst-nego-04`
2. [ ] - `p1` - **ELSE** - `inst-nego-05`
   1. [ ] - `p1` - Classify the request as plain HTTP and leave it to the HTTP proxy feature - `inst-nego-06`
3. [ ] - `p1` - Verify the resolved endpoint scheme is `wss`, or `ws` when plaintext upstreams are allowed - `inst-nego-07`
4. [ ] - `p1` - Send the rebuilt handshake to the endpoint and read its response status - `inst-nego-08`
5. [ ] - `p1` - **IF** the upstream returned `101` with a matching `Sec-WebSocket-Accept` value - `inst-nego-09`
   1. [ ] - `p1` - Report acceptance so the caller-facing upgrade may now complete - `inst-nego-10`
6. [ ] - `p1` - **ELSE** - `inst-nego-11`
   1. [ ] - `p1` - Report refusal and never complete the caller-facing upgrade - `inst-nego-12`
7. [ ] - `p1` - **RETURN** the negotiation outcome with the negotiated subprotocol, when present - `inst-nego-13`

### Upgrade Header Preservation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-upgrade-header-preservation`

**Input**: Inbound request headers plus the effective header transformation rules

**Output**: Outbound handshake headers that keep the upgrade headers intact

**Steps**:
1. [ ] - `p1` - Apply the ordinary header transformation set, add and remove rules first - `inst-hdr-01`
2. [ ] - `p1` - Rewrite `Host`, or the `:authority` pseudo-header, to the selected endpoint host - `inst-hdr-02`
3. [ ] - `p1` - Strip the routing header `X-OAGW-Target-Host` after endpoint selection consumed it - `inst-hdr-03`
4. [ ] - `p1` - **FOR EACH** hop-by-hop header normally stripped from ordinary proxied requests - `inst-hdr-04`
   1. [ ] - `p1` - **IF** the header is `Connection` or `Upgrade` on a recognised upgrade request - `inst-hdr-05`
      1. [ ] - `p1` - Exempt it from stripping so the upstream handshake stays valid - `inst-hdr-06`
   2. [ ] - `p1` - **ELSE** - `inst-hdr-07`
      1. [ ] - `p1` - Strip it exactly as the plain-HTTP path does - `inst-hdr-08`
5. [ ] - `p1` - Forward `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions` unchanged - `inst-hdr-09`
6. [ ] - `p1` - **RETURN** the outbound handshake header set - `inst-hdr-10`

### Frame Relay

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-frame-relay`

**Input**: An open client WebSocket half and an open upstream WebSocket half

**Output**: Frames delivered in both directions until a close or a drop

**Steps**:
1. [ ] - `p1` - Relay client-to-upstream and upstream-to-client directions concurrently and independently - `inst-relay-01`
2. [ ] - `p1` - **FOR EACH** frame received on either half - `inst-relay-02`
   1. [ ] - `p1` - Preserve the frame kind: text, binary, ping, pong or close - `inst-relay-03`
   2. [ ] - `p1` - Write the payload bytes to the paired half without rewriting or re-encoding them - `inst-relay-04`
   3. [ ] - `p1` - Preserve per-direction frame ordering exactly as received - `inst-relay-05`
3. [ ] - `p1` - **IF** the frame is a close frame - `inst-relay-06`
   1. [ ] - `p1` - Forward the close code and reason to the paired half unchanged - `inst-relay-07`
   2. [ ] - `p1` - Mark the session Closing and stop accepting new data frames - `inst-relay-08`
4. [ ] - `p1` - **IF** either half drops without a close frame - `inst-relay-09`
   1. [ ] - `p1` - Invoke `cpt-cf-oagw-algo-session-teardown` for the surviving half - `inst-relay-10`
5. [ ] - `p1` - **RETURN** the terminal close code, when one was exchanged - `inst-relay-11`

### Session Teardown

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-session-teardown`

**Input**: A streaming or upgraded session and the side that ended first

**Output**: Both halves closed and released, with the outcome recorded

**Steps**:
1. [ ] - `p1` - Mark the session Closing so no further payload is accepted in either direction - `inst-down-01`
2. [ ] - `p1` - **IF** the client side ended first - `inst-down-02`
   1. [ ] - `p1` - Close the upstream connection promptly rather than draining remaining output - `inst-down-03`
3. [ ] - `p1` - **IF** the upstream side ended first - `inst-down-04`
   1. [ ] - `p1` - Close the client response body or client WebSocket half - `inst-down-05`
4. [ ] - `p1` - Release the endpoint connection and any buffers held for this session - `inst-down-06`
5. [ ] - `p1` - Record the close with the correlation identifier, initiating side and close code - `inst-down-07`
6. [ ] - `p1` - Mark the session Closed and never reopen it automatically - `inst-down-08`
7. [ ] - `p1` - **RETURN** the recorded close outcome - `inst-down-09`

### Long-Lived Pipeline Reuse

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-longlived-pipeline`

**Input**: An inbound proxy request classified as an event-stream request or an upgrade request

**Output**: A resolved execution plan with long-lived-safe timeout handling

**Steps**:
1. [ ] - `p1` - Reuse alias resolution, configuration merge and route matching without any change - `inst-pipe-01`
2. [ ] - `p1` - Reuse the guard rules: method allowlist, query allowlist and path-suffix mode - `inst-pipe-02`
3. [ ] - `p1` - Reuse endpoint selection, including `X-OAGW-Target-Host` handling and pool selection - `inst-pipe-03`
4. [ ] - `p1` - Reuse request header transformation, subject to `cpt-cf-oagw-algo-upgrade-header-preservation` - `inst-pipe-04`
5. [ ] - `p1` - Reuse the plugin-invocation point exactly as the plain-HTTP path established it - `inst-pipe-05`
6. [ ] - `p1` - Reuse error mapping, the problem-details body and the `X-OAGW-Error-Source` header - `inst-pipe-06`
7. [ ] - `p1` - Apply the configured proxy timeout only until response headers arrive or the upgrade completes - `inst-pipe-07`
8. [ ] - `p1` - Never terminate a healthy open stream or session because that pre-stream timeout elapsed - `inst-pipe-08`
9. [ ] - `p1` - Skip request body size validation and buffering, since neither transport sends a request body - `inst-pipe-09`
10. [ ] - `p1` - Skip response buffering, response body transformation and response length computation - `inst-pipe-10`
11. [ ] - `p1` - **RETURN** the execution plan for the streamed or upgraded request - `inst-pipe-11`

## 4. States (CDSL)

### SSE Stream Session State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-sse-session`

**States**: Connecting, Open, Closing, Closed

**Initial State**: Connecting

**Transitions**:
1. [ ] - `p1` - **FROM** Connecting **TO** Open **WHEN** event-stream headers are forwarded to the client - `inst-ssest-01`
2. [ ] - `p1` - **FROM** Connecting **TO** Closed **WHEN** the upstream fails before headers are forwarded - `inst-ssest-02`
3. [ ] - `p1` - **FROM** Open **TO** Closing **WHEN** the upstream signals end of stream - `inst-ssest-03`
4. [ ] - `p1` - **FROM** Open **TO** Closing **WHEN** the client disconnects mid-stream - `inst-ssest-04`
5. [ ] - `p1` - **FROM** Open **TO** Closing **WHEN** the stream aborts after headers were forwarded - `inst-ssest-05`
6. [ ] - `p1` - **FROM** Closing **TO** Closed **WHEN** both halves are closed and the outcome is recorded - `inst-ssest-06`

### WebSocket Proxy Session State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-websocket-session`

**States**: Connecting, Open, Closing, Closed

**Initial State**: Connecting

**Transitions**:
1. [ ] - `p1` - **FROM** Connecting **TO** Open **WHEN** the upstream accepted the upgrade and the client upgrade completed - `inst-wsst-01`
2. [ ] - `p1` - **FROM** Connecting **TO** Closed **WHEN** the upstream refuses the upgrade or the scheme cannot serve it - `inst-wsst-02`
3. [ ] - `p1` - **FROM** Open **TO** Closing **WHEN** a close frame arrives from the client or the upstream - `inst-wsst-03`
4. [ ] - `p1` - **FROM** Open **TO** Closing **WHEN** either half drops without sending a close frame - `inst-wsst-04`
5. [ ] - `p1` - **FROM** Closing **TO** Closed **WHEN** the close is propagated and both halves are released - `inst-wsst-05`

## 5. Definitions of Done

### Event-Stream Response Detection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-detection`

The system **MUST** classify an upstream response as an event stream when its response media type is `text/event-stream`, ignoring media-type parameters, and then switch that response to pass-through streaming instead of buffering it.

**Implements**:
- `cpt-cf-oagw-algo-stream-detection`
- `cpt-cf-oagw-flow-sse-stream-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Incremental Event Forwarding

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-incremental-forwarding`

The system **MUST** forward event bytes to the caller as each upstream chunk arrives, flushing immediately, preserving byte order, and adding no aggregation window or artificial buffering delay.

**Implements**:
- `cpt-cf-oagw-algo-incremental-forwarding`
- `cpt-cf-oagw-flow-sse-stream-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Event-Stream Framing Preservation

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-framing-preserved`

The system **MUST** forward the upstream event-stream content type verbatim, add no response content-length header, and disable response body transformation or re-encoding that would break event framing.

**Implements**:
- `cpt-cf-oagw-algo-stream-detection`
- `cpt-cf-oagw-algo-incremental-forwarding`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Upstream-Initiated Stream Close

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-upstream-close`

The system **MUST** close the caller connection when the upstream ends an event stream, and record that close with the correlation identifier, the initiating side and no error status change.

**Implements**:
- `cpt-cf-oagw-flow-sse-stream-session`
- `cpt-cf-oagw-algo-session-teardown`
- `cpt-cf-oagw-state-sse-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Client-Initiated Stream Close

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-client-disconnect`

The system **MUST** detect a caller disconnect on an open event stream, stop reading upstream chunks, and close the upstream connection promptly rather than draining remaining output.

**Implements**:
- `cpt-cf-oagw-flow-sse-client-disconnect`
- `cpt-cf-oagw-algo-session-teardown`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Mid-Flight Stream Abort Handling

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-abort-handling`

The system **MUST** return a gateway-sourced `502` problem response when a stream fails before headers are forwarded, and otherwise close the caller connection without any problem body.

**Implements**:
- `cpt-cf-oagw-flow-sse-stream-aborted`
- `cpt-cf-oagw-state-sse-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`

### Upgrade Request Recognition

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-upgrade-recognition`

The system **MUST** recognise a proxy request as a WebSocket upgrade from its upgrade token, connection token, key and version headers, and route it to the upgrade path instead of the plain-HTTP path.

**Implements**:
- `cpt-cf-oagw-algo-upgrade-negotiation`
- `cpt-cf-oagw-flow-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Upstream-First Upgrade Ordering

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-upstream-first-upgrade`

The system **MUST** complete the caller-facing upgrade only after the upstream has answered its own handshake with a switching-protocols response, so a refusal never leaves an upgraded caller.

**Implements**:
- `cpt-cf-oagw-algo-upgrade-negotiation`
- `cpt-cf-oagw-flow-websocket-session`
- `cpt-cf-oagw-state-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Upgrade Header Survival Through Hop-by-Hop Stripping

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-upgrade-headers-survive`

The system **MUST** exempt the `Connection` and `Upgrade` headers from hop-by-hop stripping on a recognised upgrade request, and forward the WebSocket key, version, subprotocol and extension headers unchanged to the upstream handshake.

**Implements**:
- `cpt-cf-oagw-algo-upgrade-header-preservation`
- `cpt-cf-oagw-flow-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Bidirectional Frame Relay

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-frame-relay`

The system **MUST** relay text, binary, ping and pong frames in both directions concurrently, preserving frame kind, payload bytes and per-direction ordering without rewriting any payload.

**Implements**:
- `cpt-cf-oagw-algo-frame-relay`
- `cpt-cf-oagw-flow-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Close Propagation and Session Teardown

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-close-propagation`

The system **MUST** propagate a close frame with its close code and reason to the paired peer, and tear down both halves whenever either peer closes or drops without a close frame.

**Implements**:
- `cpt-cf-oagw-algo-frame-relay`
- `cpt-cf-oagw-algo-session-teardown`
- `cpt-cf-oagw-state-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Upgrade Refusal as Gateway Error

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-upgrade-refusal`

The system **MUST** answer a refused upgrade with a gateway-sourced problem response, using `502` and the protocol-error type for an upstream answer other than `101`. The same `502` protocol error **MUST** cover an endpoint scheme that cannot serve WebSocket, including the declared but unimplemented `wt` scheme. A plaintext `ws` endpoint refused while plaintext upstream connections are disabled **MUST** instead yield `503` with the `LinkUnavailable` type. That refusal reuses the HTTP proxy feature's plaintext-connection policy unchanged and is not re-specified by this feature.

**Implements**:
- `cpt-cf-oagw-flow-websocket-upgrade-rejected`
- `cpt-cf-oagw-state-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `WebSocket proxy session`

### Long-Lived Pipeline and Timeout Policy

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-longlived-pipeline`

The system **MUST** reuse guard rules, header transformation, endpoint selection, the plugin-invocation point and error mapping for streamed requests, while bounding the configured proxy timeout to the pre-stream phase only.

**Implements**:
- `cpt-cf-oagw-algo-longlived-pipeline`
- `cpt-cf-oagw-flow-sse-stream-session`
- `cpt-cf-oagw-flow-websocket-session`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: `SSE stream session`, `WebSocket proxy session`

## 6. Acceptance Criteria

- [ ] A `GET /oagw/v1/proxy/{alias}/{path}` request whose upstream answers `200` with `Content-Type: text/event-stream` receives response headers before the first event byte.
- [ ] An upstream emitting three events one second apart delivers each event to the caller before the next one is produced, in emission order.
- [ ] The relayed event-stream response carries the upstream `Content-Type: text/event-stream` unchanged and carries no `Content-Length` header.
- [ ] Event payload bytes forwarded to the caller are byte-identical to the upstream bytes, with no reframing, merging or reordering.
- [ ] When the upstream ends the event stream, the caller connection is closed, and a close record with the correlation identifier is written.
- [ ] When the caller disconnects from an open event stream, the upstream connection is closed, and no further upstream chunk is read.
- [ ] An upstream failure before response headers are forwarded yields `502`, `Content-Type: application/problem+json`, and `X-OAGW-Error-Source: gateway`.
- [ ] An upstream abort after response headers are forwarded closes the caller connection with a truncated stream and no problem-details body.
- [ ] A `GET /oagw/v1/proxy/{alias}/{path}` request with `Upgrade: websocket` and `Connection: Upgrade` completes with `101` only after the upstream answered `101`.
- [ ] The upstream handshake recorded at a plaintext `ws` echo endpoint contains `Connection: Upgrade`, `Upgrade: websocket`, `Sec-WebSocket-Key` and `Sec-WebSocket-Version: 13`.
- [ ] A text frame sent by the caller reaches a WebSocket echo upstream, and the echoed text frame reaches the caller with identical payload bytes.
- [ ] A binary frame sent by the caller reaches the same echo upstream, and the echoed binary frame reaches the caller as a binary frame with identical bytes.
- [ ] A ping frame sent by either peer is relayed to the paired peer as a ping frame, and the matching pong frame is relayed back.
- [ ] A close frame with code `1000` sent by the caller is delivered to the upstream with code `1000`, and both halves are then released.
- [ ] A close frame with code `1001` sent by the upstream is delivered to the caller with code `1001`, and the session reaches the Closed state.
- [ ] An abrupt caller drop without a close frame closes the upstream WebSocket half, and an abrupt upstream drop closes the caller half.
- [ ] An upstream answering the handshake with `404` instead of `101` yields `502`, `X-OAGW-Error-Source: gateway`, and no caller upgrade.
- [ ] An upgrade request resolving to an endpoint whose scheme is `wt` yields `502` with `X-OAGW-Error-Source: gateway`, confirming WebTransport is unimplemented.
- [ ] An upgrade request resolving to a `ws` endpoint while `allow_http_upstream` is disabled yields `503` with `type` equal to `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`, `X-OAGW-Error-Source: gateway`, and no upstream socket.
- [ ] A guard rule violation, such as a method outside the route allowlist, is rejected before any stream opens or any upgrade is negotiated.
- [ ] An event stream and a WebSocket session each stay open past the configured proxy timeout window while data keeps flowing.
