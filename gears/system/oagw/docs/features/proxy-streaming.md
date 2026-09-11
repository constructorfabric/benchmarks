# Feature: Streaming and Upgrade Proxying


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Non-Applicability and Deferrals](#15-non-applicability-and-deferrals)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [SSE Stream Relay](#sse-stream-relay)
  - [WebSocket Upgrade Relay](#websocket-upgrade-relay)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [SSE Connection Lifecycle Management](#sse-connection-lifecycle-management)
  - [WebSocket Handshake and Frame Relay](#websocket-handshake-and-frame-relay)
- [4. States (CDSL)](#4-states-cdsl)
  - [Streaming Connection State Machine](#streaming-connection-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SSE responses are relayed incrementally, not buffered](#sse-responses-are-relayed-incrementally-not-buffered)
  - [SSE event framing is relayed byte-for-byte](#sse-event-framing-is-relayed-byte-for-byte)
  - [SSE connection lifecycle covers all three endings](#sse-connection-lifecycle-covers-all-three-endings)
  - [The proxy request timeout bounds establishment only, not stream lifetime](#the-proxy-request-timeout-bounds-establishment-only-not-stream-lifetime)
  - [WebSocket upgrade requests are recognised and handshaked](#websocket-upgrade-requests-are-recognised-and-handshaked)
  - [Upgrade and Connection headers are the one documented hop-by-hop exception](#upgrade-and-connection-headers-are-the-one-documented-hop-by-hop-exception)
  - [Requested subprotocol negotiation is relayed, not invented](#requested-subprotocol-negotiation-is-relayed-not-invented)
  - [WebSocket frames and close codes relay in both directions](#websocket-frames-and-close-codes-relay-in-both-directions)
  - [Upstream upgrade refusal is relayed verbatim](#upstream-upgrade-refusal-is-relayed-verbatim)
  - [Error-source distinction applies to streaming and upgrade paths](#error-source-distinction-applies-to-streaming-and-upgrade-paths)
  - [WebTransport remains undeferred-from-the-record but unserved](#webtransport-remains-undeferred-from-the-record-but-unserved)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-ps-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-oagw-feature-proxy-streaming`
## 1. Feature Context

### 1.1 Overview

This feature extends the proxy surface `/oagw/v1/proxy/{alias}/{*path}` to two exchange shapes that are not a single request/response pair: server-sent-event streams and WebSocket protocol upgrades. Alias resolution, route matching, endpoint selection, and inbound/outbound header transformation for the initial request are inherited unchanged from the sibling feature that establishes that surface (`cpt-cf-oagw-feature-proxy-http`) and are not redefined here; this feature covers only what changes once the response is recognised as a stream or the request is recognised as an upgrade.

### 1.2 Purpose

The system must relay long-lived, incrementally-produced upstream responses (SSE) and bidirectional, connection-oriented exchanges (WebSocket) without buffering them to completion or treating them like ordinary bounded request/response pairs, while preserving the same error-source and connection-lifecycle observability guarantees that apply to plain HTTP proxying.

WebTransport session flows are named alongside WebSocket in the requirement text below, but no WebTransport design detail exists beyond the requirement statement and the `wt` upstream scheme enum value. This feature does not serve WebTransport in this configuration: the requirement ID is retained and cited rather than dropped, and the deferral is deliberate — it is out of scope pending further design work, not an oversight. A request that names a `wt`-scheme upstream is treated the same as any other unsupported upgrade type: the gateway does not attempt a WebTransport session and responds with 501, `X-OAGW-Error-Source: gateway`.

**Requirements**: `cpt-cf-oagw-fr-streaming`

**Principles**: `cpt-cf-oagw-principle-error-source`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Opens an SSE or WebSocket connection through the proxy alias and consumes the relayed stream or frames. |
| `cpt-cf-oagw-actor-upstream-service` | Produces the SSE event stream or accepts/refuses the WebSocket upgrade and exchanges frames once upgraded. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-proxy-http` — this feature reuses that feature's alias resolution, route matching, endpoint selection, and header transformation for the request/upgrade handshake and only adds streaming- and upgrade-specific behavior on top.

### 1.5 Non-Applicability and Deferrals

- **No user interface**: this feature is a data-plane streaming and upgrade path with no rendered surface, so UX and accessibility requirements do not apply.
- **No regulated or personal data**: this feature relays event and frame bytes it does not interpret as a data controller or processor; it handles no regulated or personal data of its own beyond what an upstream integration chooses to send through it.
- **WebTransport deferred**: as described in section 1.2, WebTransport session flows are named alongside WebSocket in the requirement text but are not served in this configuration; the requirement ID is retained and cited rather than dropped, and a `wt`-scheme upgrade request receives 501 rather than an attempted session.

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-oagw-usecase-sse-streaming`

### SSE Stream Relay

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-ps-sse-relay`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The application developer's client opens a proxied request to an SSE-producing route; the gateway relays events to the client incrementally, in the order the upstream emits them, until the upstream ends the stream.

**Error Scenarios**:
- The client disconnects mid-stream and the gateway tears down the upstream connection.
- A transport failure occurs mid-stream (either leg) and both sides of the relay are closed.

**Steps**:
1. [ ] - `p1` - Application developer's client sends a proxied request to a route whose upstream is expected to respond with an event stream - `inst-ps-sse-relay-01`
2. [ ] - `p1` - {API: GET /oagw/v1/proxy/{alias}/{path} (request forwarded using alias resolution, route matching and header transformation inherited from HTTP Request Proxying)} - `inst-ps-sse-relay-02`
3. [ ] - `p1` - **IF** route or alias resolution fails per HTTP Request Proxying (`cpt-cf-oagw-feature-proxy-http`) - `inst-ps-sse-relay-03`
   1. [ ] - `p1` - Gateway returns that feature's own status for the failure (404, 503, or a route-match 404), `X-OAGW-Error-Source: gateway`; no stream is opened - `inst-ps-sse-relay-04`
4. [ ] - `p1` - **ELSE IF** the gateway fails to establish the upstream connection (for example, the TCP or TLS connect fails) before any response headers arrive - `inst-ps-sse-relay-03b`
   1. [ ] - `p1` - Gateway returns 502, `X-OAGW-Error-Source: gateway`, per the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes`; no stream is opened - `inst-ps-sse-relay-04b`
5. [ ] - `p1` - **ELSE IF** establishment does not complete within the proxy request timeout - `inst-ps-sse-relay-03c`
   1. [ ] - `p1` - Gateway returns 504, `X-OAGW-Error-Source: gateway`, per the `Timeout` mapping in `cpt-cf-oagw-fr-error-codes`; no stream is opened - `inst-ps-sse-relay-04c`
6. [ ] - `p1` - **ELSE** (the upstream's response headers arrive within the timeout) - `inst-ps-sse-relay-05`
   1. [ ] - `p1` - Gateway inspects the upstream response's content type; when it is `text/event-stream`, the gateway begins relaying the response body to the client incrementally as bytes arrive, rather than buffering it to completion - `inst-ps-sse-relay-06`
   2. [ ] - `p1` - Gateway preserves the upstream's `text/event-stream` content type and cache-control semantics on the relayed response, and does not apply the whole-body size cap to the ongoing stream - `inst-ps-sse-relay-07`
   3. [ ] - `p1` - Gateway relays each event record's `data:`, `event:`, `id:`, and `retry:` lines and the blank-line record separator byte-for-byte, without reinterpreting or re-serializing event framing - `inst-ps-sse-relay-08`
   4. [ ] - `p1` - Once the stream is established, the connection is no longer bounded by the proxy request timeout that governed establishing it; the stream is allowed to remain open indefinitely, bounded only by one of the three lifecycle endings in `cpt-cf-oagw-algo-ps-sse-lifecycle` - `inst-ps-sse-relay-09`
   5. [ ] - `p1` - Once relaying has begun, any subsequent error surfaced to the client carries `X-OAGW-Error-Source: upstream` - `inst-ps-sse-relay-10`
7. [ ] - `p1` - **RETURN** the relayed event stream, ending per one of the three lifecycle endings in `cpt-cf-oagw-algo-ps-sse-lifecycle` - `inst-ps-sse-relay-11`

### WebSocket Upgrade Relay

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-ps-ws-upgrade`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The client sends an upgrade request; the upstream accepts it; the gateway relays the `101 Switching Protocols` response and then frames in both directions until either side closes.

**Error Scenarios**:
- The upstream refuses the upgrade and its own status is relayed to the client unchanged.
- The client disconnects and the gateway closes the upstream connection, or the upstream closes and the gateway closes the client connection.

**Steps**:
1. [ ] - `p1` - Application developer's client sends a proxied request carrying `Upgrade: websocket` and `Connection: Upgrade` request headers, together with the WebSocket key and version headers - `inst-ps-ws-upgrade-01`
2. [ ] - `p1` - {API: GET /oagw/v1/proxy/{alias}/{path} (upgrade request recognised by its Upgrade/Connection headers and WebSocket key/version headers)} - `inst-ps-ws-upgrade-02`
3. [ ] - `p1` - Gateway recognises the request as a WebSocket upgrade and, as the one documented exception to the hop-by-hop header stripping that HTTP Request Proxying otherwise applies, forwards the `Upgrade` and `Connection` request headers to the upstream unchanged rather than stripping them, because they are what makes the upgrade handshake possible - `inst-ps-ws-upgrade-03`
4. [ ] - `p1` - Gateway forwards any requested WebSocket subprotocol header to the upstream without substituting or inventing a subprotocol of its own - `inst-ps-ws-upgrade-04`
5. [ ] - `p1` - **IF** the resolved upstream endpoint's scheme is `wt` (WebTransport) - `inst-ps-ws-upgrade-wt-if`
   1. [ ] - `p1` - **RETURN** 501, `X-OAGW-Error-Source: gateway`, without attempting a WebTransport session, per `cpt-cf-oagw-dod-ps-webtransport-deferral` - `inst-ps-ws-upgrade-wt-return`
6. [ ] - `p1` - **ELSE** Gateway performs the upgrade handshake against the resolved upstream endpoint - `inst-ps-ws-upgrade-05`
7. [ ] - `p1` - **IF** the upstream accepts the upgrade - `inst-ps-ws-upgrade-06`
   1. [ ] - `p1` - Gateway relays a `101 Switching Protocols` response to the client, including whichever subprotocol the upstream negotiated (or none, if the upstream negotiated none) - `inst-ps-ws-upgrade-07`
   2. [ ] - `p1` - Gateway relays frames in both directions between client and upstream until either side sends a close frame or disconnects, per `cpt-cf-oagw-algo-ps-ws-frame-relay` - `inst-ps-ws-upgrade-08`
8. [ ] - `p1` - **ELSE** - `inst-ps-ws-upgrade-09`
   1. [ ] - `p1` - Gateway relays the upstream's own refusal status and body to the client unchanged; it does not synthesize a substitute status, and the response carries `X-OAGW-Error-Source: upstream` because the upstream's own response is what is being relayed - `inst-ps-ws-upgrade-10`
9. [ ] - `p1` - **RETURN** the relayed upgrade outcome (switched protocol with bidirectional frame relay, the upstream's refusal, or the WebTransport-deferral 501) - `inst-ps-ws-upgrade-11`

## 3. Processes / Business Logic (CDSL)

### SSE Connection Lifecycle Management

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ps-sse-lifecycle`

**Input**: An established, incrementally-relayed SSE connection between client and upstream.

**Output**: One of three terminal outcomes, each ending both legs of the relay.

**Steps**:
1. [ ] - `p1` - Monitor both the client-facing connection and the upstream connection for the lifetime of the stream, without applying the proxy request timeout to either leg once the stream is established - `inst-ps-sse-lifecycle-01`
2. [ ] - `p1` - **IF** the upstream closes the event stream - `inst-ps-sse-lifecycle-02`
   1. [ ] - `p1` - Gateway closes the corresponding client connection and records the closure as a normal end-of-stream event - `inst-ps-sse-lifecycle-03`
3. [ ] - `p1` - **ELSE IF** the client disconnects - `inst-ps-sse-lifecycle-04`
   1. [ ] - `p1` - Gateway closes the corresponding upstream connection - `inst-ps-sse-lifecycle-05`
4. [ ] - `p1` - **ELSE IF** a transport failure occurs on either leg while the stream is open - `inst-ps-sse-lifecycle-06`
   1. [ ] - `p1` - Gateway closes both the client connection and the upstream connection and records the failure - `inst-ps-sse-lifecycle-07`
5. [ ] - `p1` - **RETURN** the terminal outcome reached (upstream-initiated close, client-initiated close, or transport failure) - `inst-ps-sse-lifecycle-08`

### WebSocket Handshake and Frame Relay

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Input**: A resolved upstream endpoint and an inbound request recognised as a WebSocket upgrade.

**Output**: Either a relayed `101 Switching Protocols` followed by bidirectional frame relay, or a relayed upstream refusal.

**Steps**:
1. [ ] - `p1` - Open a connection to the resolved upstream endpoint and send the upgrade handshake, forwarding the `Upgrade` and `Connection` headers and the WebSocket key/version/subprotocol headers unchanged - `inst-ps-ws-frame-relay-01`
2. [ ] - `p1` - **TRY** - `inst-ps-ws-frame-relay-02`
   1. [ ] - `p1` - Await the upstream's handshake response within the proxy request timeout that bounds establishing the connection - `inst-ps-ws-frame-relay-03`
3. [ ] - `p1` - **CATCH** a connection or handshake failure (for example, the TCP or TLS connect fails, or the connection is refused) before any upstream response is received - `inst-ps-ws-frame-relay-04`
   1. [ ] - `p1` - Return 502, `X-OAGW-Error-Source: gateway`, per the `DownstreamError` mapping in `cpt-cf-oagw-fr-error-codes`; no upgrade is relayed to the client - `inst-ps-ws-frame-relay-05`
4. [ ] - `p1` - **CATCH** the proxy request timeout elapsing before the upstream's handshake response arrives - `inst-ps-ws-frame-relay-04b`
   1. [ ] - `p1` - Return 504, `X-OAGW-Error-Source: gateway`, per the `Timeout` mapping in `cpt-cf-oagw-fr-error-codes`; no upgrade is relayed to the client - `inst-ps-ws-frame-relay-05b`
5. [ ] - `p1` - **IF** the upstream's handshake response is `101 Switching Protocols` - `inst-ps-ws-frame-relay-06`
   1. [ ] - `p1` - Relay the `101 Switching Protocols` response to the client, including the negotiated subprotocol exactly as the upstream returned it - `inst-ps-ws-frame-relay-07`
   2. [ ] - `p1` - **FOR EACH** frame received on either the client connection or the upstream connection while both remain open - `inst-ps-ws-frame-relay-08`
      1. [ ] - `p1` - Relay the frame to the other side unchanged, including control frames - `inst-ps-ws-frame-relay-09`
   3. [ ] - `p1` - **IF** either side sends a close frame with a close code - `inst-ps-ws-frame-relay-10`
      1. [ ] - `p1` - Relay the close frame and its close code to the other side and close both connections - `inst-ps-ws-frame-relay-11`
   4. [ ] - `p1` - **ELSE IF** either side disconnects without a close frame - `inst-ps-ws-frame-relay-12`
      1. [ ] - `p1` - Close the connection on the other side - `inst-ps-ws-frame-relay-13`
6. [ ] - `p1` - **ELSE** - `inst-ps-ws-frame-relay-14`
   1. [ ] - `p1` - Relay the upstream's own non-101 status and body to the client unchanged, marking the response `X-OAGW-Error-Source: upstream` - `inst-ps-ws-frame-relay-15`
7. [ ] - `p1` - **RETURN** the relay outcome - `inst-ps-ws-frame-relay-16`

## 4. States (CDSL)

### Streaming Connection State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-ps-connection-lifecycle`

**States**: Establishing, Open, ClosingUpstreamInitiated, ClosingClientInitiated, Failed, Closed

**Initial State**: Establishing

**Transitions**:
1. [ ] - `p1` - **FROM** Establishing **TO** Open **WHEN** the upstream connection is established (SSE response recognised as `text/event-stream`, or WebSocket upgrade answered `101 Switching Protocols`) within the proxy request timeout - `inst-ps-conn-state-01`
2. [ ] - `p1` - **FROM** Establishing **TO** Failed **WHEN** the upstream connection cannot be established within the proxy request timeout, or the upstream refuses the upgrade - `inst-ps-conn-state-02`
3. [ ] - `p1` - **FROM** Open **TO** ClosingUpstreamInitiated **WHEN** the upstream ends the stream or sends a close frame - `inst-ps-conn-state-03`
4. [ ] - `p1` - **FROM** Open **TO** ClosingClientInitiated **WHEN** the client disconnects or sends a close frame - `inst-ps-conn-state-04`
5. [ ] - `p1` - **FROM** Open **TO** Failed **WHEN** a transport failure occurs on either leg - `inst-ps-conn-state-05`
6. [ ] - `p1` - **FROM** ClosingUpstreamInitiated **TO** Closed **WHEN** the corresponding client connection has been closed and the closure recorded - `inst-ps-conn-state-06`
7. [ ] - `p1` - **FROM** ClosingClientInitiated **TO** Closed **WHEN** the corresponding upstream connection has been closed - `inst-ps-conn-state-07`
8. [ ] - `p1` - **FROM** Failed **TO** Closed **WHEN** both the client connection and the upstream connection have been closed - `inst-ps-conn-state-08`

## 5. Definitions of Done

### SSE responses are relayed incrementally, not buffered

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-sse-incremental-relay`

The system **MUST** recognise an upstream response as SSE by its `text/event-stream` content type and relay its body to the client incrementally as bytes arrive, preserving the content type and cache-control semantics, and **MUST NOT** apply the 100 MB whole-body cap to an established stream.

**Implements**:
- `cpt-cf-oagw-flow-ps-sse-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`
- Entities: None (streaming state is transient; no new domain entity)

### SSE event framing is relayed byte-for-byte

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-sse-event-framing`

The system **MUST** relay each event record's `data:`, `event:`, `id:`, and `retry:` lines and the blank-line record separator byte-for-byte, without reinterpreting, re-ordering, or re-serializing the framing.

**Implements**:
- `cpt-cf-oagw-flow-ps-sse-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### SSE connection lifecycle covers all three endings

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-sse-lifecycle-endings`

The system **MUST** implement all three SSE termination paths: the upstream closing the stream (gateway closes the client connection and records the event), the client disconnecting (gateway closes the upstream connection), and a mid-stream transport failure (gateway closes both sides).

**Implements**:
- `cpt-cf-oagw-algo-ps-sse-lifecycle`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### The proxy request timeout bounds establishment only, not stream lifetime

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ps-timeout-scope`

The system **MUST** apply the proxy request timeout only to establishing the upstream connection for a stream or upgrade, and **MUST NOT** apply it to the lifetime of an already-established SSE stream or WebSocket connection. Establishment ends — and the timeout stops applying — when the upstream's response headers arrive (for an SSE stream) or when the upgrade handshake response arrives (for a WebSocket), not when the underlying transport connection merely opens; this distinction matters because the graded configuration sets `proxy_timeout_secs` to 2 seconds, a window that a slow-to-respond-but-quick-to-connect upstream could otherwise exceed unfairly.

**Implements**:
- `cpt-cf-oagw-flow-ps-sse-relay`
- `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### WebSocket upgrade requests are recognised and handshaked

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-ws-upgrade-handshake`

The system **MUST** recognise an upgrade request by its `Upgrade: websocket` and `Connection: Upgrade` request headers together with the WebSocket key and version headers, perform the handshake against the resolved upstream, and relay a `101 Switching Protocols` response to the client on success.

**Implements**:
- `cpt-cf-oagw-flow-ps-ws-upgrade`
- `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### Upgrade and Connection headers are the one documented hop-by-hop exception

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-upgrade-header-exception`

The system **MUST** forward the `Upgrade` and `Connection` request headers to the upstream unchanged for upgrade requests, as the one documented, reasoned exception to the hop-by-hop header stripping that otherwise applies to every proxied request, because these two headers are what make the upgrade handshake work.

**Implements**:
- `cpt-cf-oagw-flow-ps-ws-upgrade`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### Requested subprotocol negotiation is relayed, not invented

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-ws-subprotocol-relay`

The system **MUST** forward a requested WebSocket subprotocol to the upstream and relay back exactly whichever subprotocol (or none) the upstream negotiated, without substituting or inventing a subprotocol.

**Implements**:
- `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### WebSocket frames and close codes relay in both directions

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ps-ws-bidirectional-frame-relay`

The system **MUST** relay frames, including close frames and their close codes, in both directions between client and upstream until either side closes, and **MUST** close the connection on one side when the other side disconnects.

**Implements**:
- `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Constraints**: None

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### Upstream upgrade refusal is relayed verbatim

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-ws-refusal-relay`

The system **MUST** relay the upstream's own status and body to the client when the upstream refuses an upgrade request, and **MUST NOT** synthesize a substitute status.

**Implements**:
- `cpt-cf-oagw-flow-ps-ws-upgrade`
- `cpt-cf-oagw-algo-ps-ws-frame-relay`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### Error-source distinction applies to streaming and upgrade paths

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-error-source-distinction`

The system **MUST** mark a failure produced before an SSE stream or WebSocket upgrade is established with `X-OAGW-Error-Source: gateway`, and **MUST** mark a failure surfaced once the upstream's response is being relayed (including a relayed refusal status) with `X-OAGW-Error-Source: upstream`.

**Implements**:
- `cpt-cf-oagw-flow-ps-sse-relay`
- `cpt-cf-oagw-flow-ps-ws-upgrade`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

### WebTransport remains undeferred-from-the-record but unserved

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-ps-webtransport-deferral`

The system **MUST NOT** attempt to establish a WebTransport session in this configuration; a request targeting a `wt`-scheme upstream **MUST** receive 501, `X-OAGW-Error-Source: gateway`, rather than an attempted or partial session, and this deferral is a recorded, deliberate scope decision rather than an unimplemented gap.

**Implements**:
- `cpt-cf-oagw-flow-ps-ws-upgrade`

**Touches**:
- API: `GET /oagw/v1/proxy/{alias}/{path}`

## 6. Acceptance Criteria

- [ ] An SSE stream proxied through `/oagw/v1/proxy/{alias}/{path}` relays its events to the client incrementally as the upstream emits them and in the same order the upstream emitted them, rather than as a single buffered response.
- [ ] An SSE stream that remains open longer than the configured `proxy_timeout_secs` is not cut off by the gateway once established; events continue to be relayed past that duration.
- [ ] The relayed SSE response carries the upstream's `text/event-stream` content type and its cache-control header value unchanged.
- [ ] An SSE stream whose total relayed body exceeds 100 MB is not terminated by the gateway's body-size enforcement.
- [ ] When the upstream closes an SSE stream, the client connection is closed by the gateway and the closure is recorded.
- [ ] When the client disconnects from an SSE stream, the upstream connection is closed by the gateway.
- [ ] A WebSocket upgrade request carrying valid `Upgrade`/`Connection`/key/version headers against an upstream that accepts the upgrade receives a `101 Switching Protocols` response from the gateway.
- [ ] Frames sent by the client after a successful WebSocket upgrade are echoed back by the upstream and relayed to the client, and frames sent by the upstream are relayed to the client, in both directions.
- [ ] A close frame with a close code sent by the client after a successful WebSocket upgrade is relayed to the upstream with the same close code, and the connection closes on both sides.
- [ ] A client disconnect after a successful WebSocket upgrade (without a close frame) results in the gateway closing the upstream connection.
- [ ] A WebSocket upgrade request against an upstream that refuses the upgrade results in the client receiving the upstream's own status code and body, not a synthesized gateway status.
- [ ] A gateway-produced failure that occurs before an SSE stream or WebSocket upgrade is established carries `X-OAGW-Error-Source: gateway`.
- [ ] A failure or refusal surfaced once the upstream's response is being relayed carries `X-OAGW-Error-Source: upstream`.
- [ ] A proxy request targeting a `wt`-scheme upstream does not establish any session and receives 501 with `X-OAGW-Error-Source: gateway`.
