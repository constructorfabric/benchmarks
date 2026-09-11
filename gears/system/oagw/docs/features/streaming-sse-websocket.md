# Feature: Streaming — SSE and WebSocket


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxy an SSE Response](#proxy-an-sse-response)
  - [Proxy a WebSocket Upgrade](#proxy-a-websocket-upgrade)
  - [Close a Streamed Exchange from the Upstream Side](#close-a-streamed-exchange-from-the-upstream-side)
  - [Tear Down on a Client Disconnect](#tear-down-on-a-client-disconnect)
  - [Report a Stream Error and Its Source](#report-a-stream-error-and-its-source)
  - [Tear Down an Idle Streamed Exchange](#tear-down-an-idle-streamed-exchange)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Incremental SSE Forwarding](#incremental-sse-forwarding)
  - [WebSocket Upgrade over the Selected Endpoint](#websocket-upgrade-over-the-selected-endpoint)
  - [Bidirectional Frame Relay](#bidirectional-frame-relay)
  - [Streamed Exchange Lifecycle Tracking](#streamed-exchange-lifecycle-tracking)
  - [Stream Error Classification and Source Stamping](#stream-error-classification-and-source-stamping)
  - [Idle Timeout for Streams](#idle-timeout-for-streams)
- [4. States (CDSL)](#4-states-cdsl)
  - [Streamed Exchange State Machine](#streamed-exchange-state-machine)
  - [WebSocket Session State Machine](#websocket-session-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Incremental SSE Forwarding](#incremental-sse-forwarding-1)
  - [WebSocket Upgrade Construction and Header Re-injection](#websocket-upgrade-construction-and-header-re-injection)
  - [Bidirectional Frame Relay](#bidirectional-frame-relay-1)
  - [Stream Lifecycle Close Semantics](#stream-lifecycle-close-semantics)
  - [Stream Error Classification and Error Source on Streams](#stream-error-classification-and-error-source-on-streams)
  - [Stream Scheme Posture and WebTransport Exclusion](#stream-scheme-posture-and-webtransport-exclusion)
  - [Test Layering for Streaming](#test-layering-for-streaming)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-oagw-featstatus-streaming-sse-websocket-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-streaming-sse-websocket`

<!--
=============================================================================
FEATURE SPECIFICATION
=============================================================================
PURPOSE: Define detailed implementation behavior — flows, algorithms, states,
and implementation requirements that bridge PRD and DESIGN to code.

SCOPE:
  ✓ Actor flows (user-facing interactions, step by step)
  ✓ Processes / Business Logic (incl. internal logic, validation, async jobs, etc)
  ✓ State machines (entity lifecycle)
  ✓ Implementation requirements (what to build)
  ✓ Acceptance criteria (how to verify)

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Architecture, components, APIs → DESIGN.md
  ✗ Why a specific approach was chosen → ADR/

CDSL PSEUDO-CODE:
  Optional. Use for complex flows or when precise behavior must be
  communicated. Skip for simple features to avoid overhead.
=============================================================================
-->
## 1. Feature Context

### 1.1 Overview

This feature owns the streamed half of the OAGW data plane: everything that happens after
DECOMPOSITION entry 2.4 classifies an upstream exchange as streamed and hands it over still open. It
detects a server-sent events exchange — a request carrying `Accept: text/event-stream`, a response
whose `Content-Type` is `text/event-stream`, or both — and forwards the upstream's bytes to the caller
incrementally as they arrive, with no full-body buffering, no re-framing and no line rewriting; it runs
the open, data, close and error lifecycle of that exchange, closing the client connection when the
upstream ends the stream and closing the upstream connection when the client disconnects; it proxies a
WebSocket upgrade bidirectionally over a `wss` upstream endpoint (or over an `http` upstream endpoint
admitted by `allow_http_upstream`), handling `Upgrade`, `Connection` and the `Sec-WebSocket-*` header
set and relaying frames in both directions until either side closes; and it classifies every stream
failure onto the canonical error table, so an aborted stream is reported as `502` StreamAborted and
every stream response carries `X-OAGW-Error-Source`. It owns nothing before the handoff and nothing
after the stream: the alias walk, route match, configuration merge, plugin hook points, endpoint
selection, request and body validation, header transformation, the upstream call and the circuit
breaker are entry 2.4's pipeline, the plugin chain behaviour inside those hooks is entry 2.5's, and the
audit record and the metrics are entry 2.7's.

### 1.2 Purpose

Entry 2.6 completes `cpt-cf-oagw-fr-streaming` for the transports this gear can actually carry. Entry
2.4 hands over an open exchange when it classifies the response as streamed (`text/event-stream`, or an
upgrade negotiated with the upstream) and stops owning it there; without this feature that exchange has
no defined lifecycle — no incremental forwarding, no close semantics on either side, no error
classification — and a caller using an SSE API such as a streaming chat-completions endpoint would
receive nothing at all. The decomposition's shared-requirement split assigns the stream half of
`cpt-cf-oagw-fr-error-codes` to this entry, entry 2.4 owning the mapping for buffered responses, and
`cpt-cf-oagw-usecase-sse-streaming` names this feature's two close semantics directly: the upstream
closes the connection and the system closes the client connection and records the event, and the client
disconnects and the system closes the upstream connection. It realizes
`cpt-cf-oagw-principle-error-source` on a streamed response by stamping the source on the response head
before the first byte of the body is forwarded, and `cpt-cf-oagw-principle-rfc9457` by emitting every
gateway-generated stream failure through the entry-2.1 mapping layer as an
`application/problem+json` document with a GTS `type` identifier.

**Requirements**:

- [ ] `p1` - `cpt-cf-oagw-fr-streaming`
- [ ] `p1` - `cpt-cf-oagw-usecase-sse-streaming`
- [ ] `p1` - `cpt-cf-oagw-fr-error-codes`

  StreamAborted 502 on aborted streams.

**Principles**: `cpt-cf-oagw-principle-error-source`, `cpt-cf-oagw-principle-rfc9457`

**Feature-local deviations and recorded boundaries** (each inherited from the decomposition's
task-level assumptions, from the PRD's own wording, or recorded as a scope boundary this feature
implements against; none is a new decision taken here):

- WebTransport has no session flow — DECOMPOSITION assumption 6 and entry 2.6 out of scope, recorded as
  an explicit deviation from `cpt-cf-oagw-fr-streaming`, whose second sentence requires WebSocket and
  WebTransport session flows. No QUIC transport dependency exists in the crate, so no WebTransport
  session flow is implemented: this feature implements the SSE and the WebSocket halves of that
  requirement and no third transport. The `wt` scheme is still accepted at upstream-create validation
  time and stays in the stored endpoint model verbatim — entry 2.2 owns that acceptance and this
  feature does not narrow it. Boundary assumption, stated once and used everywhere in this artifact:
  **the request-time exclusion of a `wt`-scheme upstream is `cpt-cf-oagw-feature-proxy-engine`'s, which
  records it at its alias-resolution step**, so no proxy request ever reaches this
  feature's handoff with a `wt` upstream; this feature owns only the consequence that it contains no
  `wt` code path and asserts no disposition of its own for a request that resolves to one — the `404`
  RouteNotFound the caller observes is that entry's behaviour, not this feature's. Review owner: OAGW
  component maintainer. Validation: an in-crate test asserts this feature's detection, upgrade and
  relay code carries no `wt` branch at all, and the resolution-step test that answers a request whose
  alias resolves to a `wt`-scheme upstream with `404` belongs to `cpt-cf-oagw-feature-proxy-engine`.
- The WebSocket upgrade re-injects headers the entry-2.4 transform strips — DESIGN's header
  transformation table strips `Connection` and `Upgrade` from every request (with `Keep-Alive`, `TE`,
  `Trailer`, `Transfer-Encoding` and the proxy headers), while a WebSocket upgrade requires
  `Upgrade: websocket` and `Connection: Upgrade` to reach the upstream. Rather than exempting the
  upgrade from the transformation — which would reopen a hop-by-hop leak on every other header — this
  feature re-injects, after `cpt-cf-oagw-algo-header-transform` has run and onto the upstream exchange
  only, the validated `Upgrade: websocket` and `Connection: Upgrade` pair plus the client's
  `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions`
  headers. `Sec-WebSocket-Accept` is **not** injected and **not** computed here: it is computed by the
  peer that accepts the upgrade, which is the upstream, and the `101 Switching Protocols` response is
  relayed to the client with its headers verbatim. Review owner: OAGW component maintainer, with the
  security reviewer as second approver. Validation: an in-crate test asserts the outbound upgrade
  request carries exactly the re-injected upgrade header set, no `Sec-WebSocket-Accept` and no other
  hop-by-hop header, and a second test asserts the `101` response reaches the client with its headers
  unchanged.
- `cpt-cf-oagw-constraint-https-only` is covered with the assumption-2 correction, mirrored from
  DECOMPOSITION entry 2.6: a `ws` upgrade over an `http` upstream endpoint is permitted when
  `allow_http_upstream` is enabled; the default posture stays HTTPS-only. A `wss` endpoint always
  carries the upgrade over TLS. With `allow_http_upstream: false` (the recorded default), an upgrade
  attempted against an `http`-scheme endpoint is refused as a gateway error before the upstream call,
  using the existing `503` LinkUnavailable row of the canonical error table — this feature invents no
  new error type for it. With `allow_http_upstream: true` (the graded configuration), the plaintext
  upgrade proceeds over HTTP/1.1, which is the only version an upgrade can be negotiated on. Review
  owner: OAGW component maintainer, with the security reviewer as second approver. Validation: an
  in-crate test asserts the recorded default refuses an `http`-endpoint upgrade with `503` before any
  connection attempt, and a second test asserts the graded configuration admits one.
- Aborted streams reuse the existing `StreamAborted` row — DESIGN's canonical error table already
  carries `StreamAborted | 502 | gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` with
  `Retriable: No`, and this feature maps an aborted stream onto it rather than adding a stream-specific
  error type. An aborted stream is a client disconnect mid-body, an upstream reset or abort mid-body,
  or an upstream abort before the first body byte; a client disconnect is additionally the one case
  that produces no response at all, because there is no client connection left to write one to. Review
  owner: OAGW component maintainer. Validation: an in-crate test asserts an upstream abort before the
  first body byte yields a `502` problem+json response with that GTS `type` identifier and
  `X-OAGW-Error-Source: gateway`, and a second test asserts an abort after the head was committed tears
  the exchange down without fabricating a mid-stream body.
- The idle timeout applies to streams — the gear configuration key `oagw.config.proxy_timeout_secs`
  read by entry 2.4 bounds the buffered call and bounds a stream as an **idle** window: a stream with
  no bytes in either direction for that window is torn down. Disposition, stated identically in the
  flow, the algorithm and the DoD below: the teardown is mapped onto the existing `504` IdleTimeout row
  (`gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`, `Retriable: Yes`) when a response is still
  writable — that is, when no body byte has been flushed to the client yet — and is otherwise recorded
  as a teardown with the idle-timeout error type on the request context, with no response written. This
  feature adds no second timeout knob and no per-stream configuration. Review owner: OAGW component
  maintainer. Validation: an in-crate test asserts a silent upstream past the configured window yields
  a `504` with the idle-timeout `type` identifier when the head was not committed, and a teardown with
  the outcome recorded when it was.
- The entry-2.4 handoff for a WebSocket upgrade happens on the request side — this feature RECORDS AN
  AMENDMENT to DECOMPOSITION entry 2.4's streamed-handoff wording, which reads as handing over an open
  exchange once the response is classified as streamed (`text/event-stream`, or an upgrade negotiated
  with the upstream) and therefore as a response-side handoff for an upgrade too. The behaviour
  implemented here is that for an upgrade request the entry-2.4 handoff occurs on the request side —
  after entry 2.4's header transformation and endpoint selection and before entry 2.4's upstream call —
  because entry 2.4's header transformation strips `Upgrade` and `Connection` while the upgrade needs
  them, and `Sec-WebSocket-*` handling and the upgrade dial are entry 2.6's; entry 2.4
  (`cpt-cf-oagw-feature-proxy-engine`, which records the same two handoff forms in
  `cpt-cf-oagw-dod-streaming-handoff`) hands this feature the request-side context — the selected
  endpoint, the transformed header set and the request context — and no upstream connection is open at
  the handoff, so this feature dials the selected endpoint itself. The response-side open-exchange
  handoff continues to apply to SSE. The DECOMPOSITION sentence is superseded by this reading; the
  correction is recorded here because the decomposition is read-only upstream. Review owner: OAGW
  component maintainer. Validation: an in-crate test asserts an upgrade request reaches this feature
  with the entry-2.4 header set already transformed and with no upstream connection opened by entry 2.4,
  and a second test asserts a `text/event-stream` response is still handed over on the response side
  with its body unread.
- The entry-2.4 pipeline and its handoff are consumed, not re-declared — entry 2.4 owns alias walk,
  route match, configuration merge, plugin hook points, endpoint selection, request and body
  validation, header transformation, the upstream call, the circuit breaker, error mapping and the
  error-source header for non-streamed responses, and `cpt-cf-oagw-dod-streaming-handoff` fixes the
  handoff contract: an open exchange, the body unread, the response head already carrying
  `X-OAGW-Error-Source`, and the body-limit checks not having consumed a streamed body. This feature
  owns only what happens after that point — incremental forwarding, the open/close/error lifecycle,
  bidirectional relay and stream error classification — and re-declares no pipeline stage.
- The entry-2.5 plugin boundary is referenced, not re-decided — `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting`
  owns the plugin chain, and its request-phase hooks (auth, guards, request transforms, rate limiting)
  run before the upstream call as part of entry 2.4's pipeline, so a streamed exchange always arrives
  at this feature already authenticated, already authorized and already rate-limited: there is no auth
  bypass on a stream and this feature runs no hook of its own. Its response-phase transforms do **not**
  run on a streamed exchange — that feature's response-phase criteria already state that the handoff
  precedes the hook — so no transform mutates a stream after it starts.
- Metrics and audit records are entry 2.7's — this feature records the outcome, the error type, the
  byte counts and the close reason on the request context and emits no metric family and no audit
  record of its own (DECOMPOSITION assumption 9).

Coverage note: the reference ids cited on the DoDs below that are not carried in the **Requirements**
list above are inherited baselines, not requirements this feature adopts on its own.
`cpt-cf-oagw-constraint-no-direct-internet` applies through the entry-2.4 posture this feature
inherits: the upgrade dials the selected endpoint taken from the store, never a client-supplied host.
`cpt-cf-oagw-constraint-toolkit-deploy` is inherited from dependency entry 2.1, which delivers the gear
deployment and the canonical error mapping every stream response below is written through.

**Cross-cutting concerns**:

- Security: a streamed or upgraded exchange is **not** a bypass. Authentication, the
  `gts.cf.core.oagw.proxy.v1~:invoke` permission, the guard decisions and the rate-limit evaluation all
  ran in the entry-2.5 request-phase hooks before the handoff, and this feature neither re-runs nor
  skips them. The upgrade path re-injects only the validated `Upgrade`, `Connection` and
  `Sec-WebSocket-*` headers onto the upstream exchange and forwards no other hop-by-hop header; it
  never injects or rewrites `Sec-WebSocket-Accept`, which the accepting upstream computes. Origin
  posture: this feature adds no origin allowlist and no `Sec-WebSocket-Origin` handling of its own —
  the client's `Origin` header is an ordinary passthrough header subject to the entry-2.4 passthrough
  policy and the entry-2.5 CORS enforcement that already ran, and the decision to accept an upgrade
  from an origin belongs to the upstream that answers it, so a caller cannot obtain an origin grant
  from the gateway that the upstream did not make. The connect target is always the selected endpoint
  from the store, so no upgrade can be aimed at an arbitrary host. No response, error body or
  context-bound value carries credential material — this feature never reads secret material at all,
  because credential resolution happened upstream of the handoff.
- Versioning: the proxy contract this feature streams over is `cpt-cf-oagw-interface-proxy-api`'s, and
  this feature registers no endpoint, no version and no breaking change of its own; the streamed
  behaviour is an additive property of the existing `{METHOD} /oagw/v1/proxy/{alias}/{path}` operations.
- Reliability: every stream is bounded — by the idle window of `oagw.config.proxy_timeout_secs` in both
  directions, by the closure of either side, and by the failure classification below — so no stream can
  hold a connection, a buffer or a task indefinitely. Failures are classified and recorded, never
  swallowed. There is no persisted stream state to recover: all lifecycle tracking is in-memory
  (DECOMPOSITION assumption 3), and a restart ends every stream it was relaying, because the
  connections it held die with the process.
- Data integrity: the stream is a passthrough — bytes are forwarded in the order received, with no
  re-framing, no line rewriting, no inserted `Content-Length` on a streamed response and no
  transformation of the upstream's framing, so the client receives exactly the byte sequence the
  upstream produced. The body-limit checks of `cpt-cf-oagw-constraint-body-limit` do not consume a
  streamed body, per the entry-2.4 handoff contract, so a long-lived stream is never truncated or
  rejected by a limit that exists to bound buffered bodies. A request resolves one configuration
  snapshot for its whole lifetime, so a management write during a stream cannot re-target it mid-flight.
- Observability: delegated to DECOMPOSITION entry 2.7. This feature writes the close reason, the error
  type, the direction that closed the stream and the byte counts onto the request context, and emits no
  audit record and registers no metric family — including no connection-gauge series, which is entry
  2.7's surface.
- Rollback: no persistence and no migration exist, so rollback is the operational act of redeploying
  the previous executable; the only in-gear recovery action is the teardown of the streams the previous
  executable was relaying, which the redeploy performs by ending their processes.
- Test layering: coverage is in-crate Rust tests only — unit tests inside `#[cfg(test)]` modules per
  layer for the detection, the upgrade header handling, the relay, the lifecycle transitions and the
  error classification, and integration tests under the crate's `tests/` directory that boot the gear
  router and drive the proxy endpoint against a stub upstream listener provided by the test harness,
  including a stub that speaks SSE and one that completes or refuses a WebSocket upgrade — and the
  `testing/e2e/gears/oagw/` directory is not used (DECOMPOSITION assumption 5). There is **no** e2e
  streaming suite and no QUIC-aware test harness, because the `wt` path does not exist to test.
- Compile-time gate: this feature adds no gate, no new gear and no endpoint. It exists inside the
  `cf-gears-oagw` crate behind the same host feature `oagw` link-time registration entry 2.1 defines,
  and its code is present in the host executable exactly when the crate is linked.
- Performance: applicable and owned here. The controlling property is that a stream is never buffered
  as a whole: forwarding is bounded to one chunk in flight, each chunk is flushed before the next is
  read, and the memory a stream holds is independent of the body length, which is what keeps an SSE
  response of unbounded duration from exhausting the gear. Relay is a bidirectional copy with no
  interpretation of frame or event content, so the per-chunk cost is a copy and a flush. The gateway
  work this feature adds is the classification and the head relay, which happen once per stream; the
  per-chunk path performs no configuration lookup, no store read and no plugin execution. The parts of
  the budget this feature does not own are named: the sub-10ms p95 gateway-added budget of
  `cpt-cf-oagw-nfr-low-latency` is measured and owned by entry 2.4's pipeline, non-blocking audit
  logging is entry 2.7's requirement, and the upstream's own delivery rate is excluded. Stream state is
  per connection and per instance: any gear instance can hold a stream, and no cross-instance session
  affinity and no shared stream state are required — a stream lives and dies with the instance that is
  relaying it (DECOMPOSITION assumption 3).
- Compliance/Privacy: not applicable in this feature — nothing is persisted (assumption 3), no personal
  data is processed, and the request context this feature writes carries a close reason, an error type
  and byte counts, never a header value, a body byte or a query string, so there is no retention,
  residency or subject-right surface here.
- Accessibility: not applicable in this feature — no user-facing interface is authored beyond the
  `application/problem+json` stream error contract, whose machine-readable `type`, `title` and `detail`
  fields are the only surface an accessibility concern could attach to.
- Explicitly not applicable — WebSocket subprotocol negotiation policy: this feature forwards
  `Sec-WebSocket-Protocol` and relays the upstream's chosen subprotocol back inside the verbatim `101`
  response, and applies no preference, allowlist or default of its own, because no requirement names
  one and inventing a policy here would change the protocol outcome the upstream decided. Explicitly
  not applicable — message compression: `Sec-WebSocket-Extensions` is forwarded as received and the
  extension set in force is whatever the upstream answered, so the relay stays a byte copy and no
  per-message decompression is implemented; and explicitly not applicable — HTTP/2 and HTTP/3
  streaming semantics beyond what the entry-2.4 client stack negotiates, since an upgrade is
  an HTTP/1.1 mechanism and no decomposition entry delivers an HTTP/2 upgrade path.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Opens SSE streams and WebSocket upgrades against `{METHOD} /oagw/v1/proxy/{alias}/{path}`, consumes the forwarded bytes or frames, and reads `X-OAGW-Error-Source` and the problem+json `type` identifier from a stream that fails before its head was committed. |
| `cpt-cf-oagw-actor-platform-operator` | Owns the configuration this feature reads and the runtime keys that govern it — `allow_http_upstream`, `proxy_timeout_secs` and the `wss`/`http` endpoint schemes of `cpt-cf-oagw-feature-upstream-route-management` — and observes stream outcomes through entry 2.7. |
| `cpt-cf-oagw-actor-upstream-service` | Produces the streamed body or accepts the upgrade, chooses when the stream ends or aborts, computes `Sec-WebSocket-Accept`, and its silence past the idle window is the timeout trigger. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — `cpt-cf-oagw-component-model` (DataPlaneService and the proxy
  infrastructure this feature extends), `cpt-cf-oagw-design-domain-model` (`Endpoint` with a `wss`
  scheme, and the request and response contexts this feature advances), the header transformation table
  whose hop-by-hop rows the upgrade path re-injects against, the canonical error table rows
  `StreamAborted` (`502`, `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, not retriable) and
  `IdleTimeout` (`504`, `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`, retriable), and the error
  source distinction rules
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) — entry 2.6 and assumptions 1 to 9, of
  which assumption 2 (the `http` endpoint scheme and `allow_http_upstream`), assumption 3 (in-memory
  state), assumption 5 (in-crate tests only), assumption 6 (WebTransport descope) and assumption 8
  (`toolkit-http`/`pingora-*` transport substitution) are the ones this feature implements against
- **ADRs**: [0007 Error Source Distinction](../ADR/0007-error-source-distinction.md)
  (`cpt-cf-oagw-adr-error-source-distinction` — the header values `gateway` and `upstream`, and the
  fallback of inspecting the response structure when an intermediary strips the header, which is the
  fallback a stream consumer has too); supporting baselines:
  [0006 State Management](../ADR/0006-state-management.md) (`cpt-cf-oagw-adr-state-management` —
  in-process data-plane state ownership, which is why the stream lifecycle is per-instance and dies
  with the process) and [0001 Request Routing](../ADR/0001-request-routing.md)
  (`cpt-cf-oagw-adr-request-routing` — the proxy operations this feature streams on)
- **Dependencies**: `cpt-cf-oagw-feature-auth-plugins-and-rate-limiting` — the decomposition's declared
  dependency, which guarantees a streamed exchange reaches this feature only after the plugin chain,
  the credential injection and the rate limit have run, and whose response-phase criteria this feature
  relies on to keep transforms off a streamed exchange
- **Consumed contracts, available transitively through that dependency**: `cpt-cf-oagw-feature-proxy-engine`
  owns the pipeline and the streamed handoff this feature starts at (`cpt-cf-oagw-dod-streaming-handoff`,
  `cpt-cf-oagw-algo-upstream-call`, `cpt-cf-oagw-algo-header-transform`), and
  `cpt-cf-oagw-feature-upstream-route-management` owns the `Endpoint` model, its `scheme` values and the
  enabled states this feature reads
- **Resolved gear dependencies used here**: none directly — this feature calls no `tenant-resolver`,
  `authz-resolver`, `credstore` or `types-registry` of its own; tenant resolution and the permission
  check already happened in the entry-2.4 pipeline before the handoff, and credential material never
  reaches this feature
- **Platform baselines**: the crate `cf-gears-oagw` and its existing `toolkit-http`/`pingora-*`
  transport stack, through which the upstream connection and the bidirectional relay are performed, with
  no new client or QUIC dependency added (DECOMPOSITION assumption 8); the canonical error contract
  `toolkit_canonical_errors::CanonicalError` serialized as RFC 9457 `application/problem+json` with GTS
  `type` identifiers in the `gts.cf.core.errors.err.v1~cf.oagw....v1` space and the
  `X-OAGW-Error-Source` response header, both delivered by the entry-2.1 cross-cutting layer; the gear
  configuration keys `oagw.config.proxy_timeout_secs` and `oagw.config.allow_http_upstream` loaded by
  entry 2.1 (graded values `2` and `true`; entry-2.1 recorded defaults `30` and `false`); the
  `dashmap`/`parking_lot`/`arc-swap` snapshot reads of the entry-2.2 store, from which this feature
  reads the selected endpoint and nothing else

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the
end-to-end flow of a use case. Every flow below begins at the entry-2.4 handoff and ends when both
sides of the exchange are closed; the steps before the handoff are named only to locate the boundary,
and their behaviour is entry 2.4's and entry 2.5's. Every gateway error below returns through the
entry-2.1 mapping layer, so each one is an `application/problem+json` body with a GTS `type`
identifier, and every stream response carries `X-OAGW-Error-Source` on its head.

**Use cases**: `cpt-cf-oagw-usecase-sse-streaming`

**Referenced, not covered here**:

- `cpt-cf-oagw-fr-request-proxy` and `cpt-cf-oagw-usecase-proxy-request` — covered by DECOMPOSITION
  entry 2.4, which owns the pipeline up to and including the classification that produces the handoff
  this feature starts from.
- `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-auth-injection` and `cpt-cf-oagw-fr-rate-limiting` —
  covered by DECOMPOSITION entry 2.5. Their request-phase hooks run before the handoff and their
  response-phase transforms do not run on a streamed exchange; this feature provides neither.
- `cpt-cf-oagw-nfr-observability` — covered by DECOMPOSITION entry 2.7, which emits the audit record
  and the connection and stream metric families from the request context this feature completes.
- [ ] `p2` - `cpt-cf-oagw-nfr-low-latency` — covered by DECOMPOSITION entry 2.4
  (`cpt-cf-oagw-feature-proxy-engine`), which owns the proxy hot path and the sub-10ms p95 budget the
  per-chunk copy of this feature sits under.
- [ ] `p2` - `cpt-cf-oagw-interface-proxy-api` — the proxy contract and its versioning policy are
  covered by DECOMPOSITION entry 2.4 (`cpt-cf-oagw-feature-proxy-engine`), and the upstream endpoint
  surface that contract streams against is entry 2.2's
  (`cpt-cf-oagw-feature-upstream-route-management`).
- `cpt-cf-oagw-fr-upstream-mgmt` and the `wt` scheme acceptance of entry 2.2 — covered by DECOMPOSITION
  entry 2.2; this feature consumes the stored `Endpoint` and narrows nothing of what create time
  accepts, and the request-time exclusion of a `wt` upstream is
  `cpt-cf-oagw-feature-proxy-engine`'s resolution step, not this feature's.
- gRPC proxying — phase 4, out of scope for the gear and therefore for this feature; the `grpc`
  protocol has no reachable proxy path in entry 2.4 and no stream surface here.

### Proxy an SSE Response

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-sse-proxy`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- An actor sends `{METHOD} /oagw/v1/proxy/{alias}/{path}` with `Accept: text/event-stream` and the
  upstream answers `Content-Type: text/event-stream`; the events reach the caller as the upstream
  produces them, in order and unreframed, and the connection closes cleanly when the upstream ends the
  stream.
- An SSE request whose response is **not** `text/event-stream` is forwarded on the ordinary buffered
  path of entry 2.4: the `Accept` header alone does not make a response a stream, and this feature is
  never entered for it.
- A response whose `Content-Type` is `text/event-stream` is forwarded as a stream even when the request
  carried no `Accept: text/event-stream`: the response header is what classifies the exchange.
- The upstream's `Cache-Control` posture (including `no-cache` and `no-transform`) reaches the client
  unchanged, no `Content-Length` is inserted on the streamed response, and the upstream's own framing —
  including any `Transfer-Encoding` it declared — is passed through.

**Error Scenarios**:

- The upstream aborts or resets the connection before any body byte has been flushed: the caller
  receives `502` with the `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` `type` identifier and
  `X-OAGW-Error-Source: gateway`.
- The upstream aborts after the head and some body bytes were flushed: the stream is torn down, no
  fabricated body is appended, and the abort is recorded on the request context for entry 2.7.
- The client disconnects mid-stream: the upstream connection is closed, the buffers are released, no
  response is written and the client-initiated close is recorded.
- No bytes arrive in either direction for the configured idle window: the stream is torn down as an
  idle timeout, `504` with the idle-timeout `type` identifier when a response is still writable,
  otherwise a recorded teardown.

**Steps**:

1. [ ] - `p1` - Actor sends the SSE request to `/oagw/v1/proxy/{alias}/{path}` with - `inst-ss-sse-01`
   `Accept: text/event-stream`
2. [ ] - `p1` - The entry-2.4 pipeline resolves the alias, matches the route, merges the configuration, - `inst-ss-sse-02`
   runs the entry-2.5 request-phase hooks, selects the endpoint, validates and transforms the request
   and calls the upstream; this flow changes none of that and starts at the classification
3. [ ] - `p1` - **IF** the upstream response's `Content-Type` is `text/event-stream`, or the response is - `inst-ss-sse-03`
   otherwise classified as streamed by entry 2.4
   1. [ ] - `p1` - Receive the open upstream exchange at the handoff point - `inst-ss-sse-04`
      `cpt-cf-oagw-dod-streaming-handoff` records, with the body unread, and open the streamed
      lifecycle with `cpt-cf-oagw-algo-stream-lifecycle`
4. [ ] - `p1` - **ELSE** the response is a complete buffered response - `inst-ss-sse-05`
   1. [ ] - `p1` - Return the exchange to the entry-2.4 pipeline for its buffered passthrough, the - `inst-ss-sse-06`
      response-phase hook of entry 2.5 and its error-source stamping, and **RETURN** there; this flow is
      not entered and no step below runs for it
5. [ ] - `p1` - **On the streamed branch above**, restate the stream kind that produced the handoff and - `inst-ss-sse-07`
   detect it with `cpt-cf-oagw-algo-sse-forward`: an SSE **response** is declared by the `Content-Type`
   of `text/event-stream`, an SSE **request** by the request's `Accept: text/event-stream`; the two are
   independent facts and either one is recorded on the request context
6. [ ] - `p1` - Relay the upstream response head to the client as received, with `X-OAGW-Error-Source` - `inst-ss-sse-08`
   already stamped on it by the entry-2.4 header layer, adding no `Content-Length`, altering no
   `Cache-Control` value and forwarding the upstream's framing unchanged
7. [ ] - `p1` - **FOR EACH** chunk the upstream delivers on the open exchange - `inst-ss-sse-09`
   1. [ ] - `p1` - Forward the chunk to the client in the order received with no buffering beyond one - `inst-ss-sse-10`
      chunk in flight, no re-framing, no line rewriting and no event re-serialization, and flush it
      before reading the next
   2. [ ] - `p1` - Reset the idle window and record the forwarded byte count on the request context - `inst-ss-sse-11`
8. [ ] - `p1` - **IF** the upstream ends the stream (end of body) - `inst-ss-sse-12`
   1. [ ] - `p1` - Close the client connection cleanly and record the upstream-initiated close, with its - `inst-ss-sse-13`
      close reason and byte counts, on the request context for entry 2.7
9. [ ] - `p1` - **ELSE IF** the upstream aborts, resets or fails before the stream ends - `inst-ss-sse-14`
   1. [ ] - `p1` - Classify the failure with `cpt-cf-oagw-algo-stream-error-classify` and **RETURN** - `inst-ss-sse-15`
      `502` StreamAborted as a problem+json body with `X-OAGW-Error-Source: gateway` when no body byte
      has been flushed, and tear the exchange down with the abort recorded when it has
10. [ ] - `p1` - **ELSE IF** the client disconnects - `inst-ss-sse-16`
    1. [ ] - `p1` - Close the upstream connection, release the buffers and the lifecycle entry, record - `inst-ss-sse-17`
       the client-initiated close, and write no response
11. [ ] - `p1` - **RETURN** the streamed response to the client; this flow's responsibility ends when - `inst-ss-sse-18`
    both sides are closed and the outcome is on the request context

### Proxy a WebSocket Upgrade

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-ws-proxy`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- An actor sends an upgrade request with `Upgrade: websocket` and `Connection: Upgrade` and the upstream
  answers `101 Switching Protocols`; the `101` reaches the client with its headers verbatim, and frames
  then flow in both directions until one side closes.
- The upgrade is negotiated over a `wss` upstream endpoint, so the upgrade request and every frame
  travel over TLS.
- The upgrade is negotiated over an `http` upstream endpoint because `allow_http_upstream` is enabled in
  the runtime configuration, over HTTP/1.1.
- The upstream refuses the upgrade: its status, headers and body are passed through to the client
  unchanged with `X-OAGW-Error-Source: upstream`, and no relay is started.

**Error Scenarios**:

- The upgrade request is not a well-formed upgrade (`Upgrade` not `websocket`, missing
  `Connection: Upgrade`, missing `Sec-WebSocket-Key`, or a `Sec-WebSocket-Version` this gateway does not
  forward): `400` with the validation error `type` identifier and `X-OAGW-Error-Source: gateway`, before
  any upstream call.
- The selected endpoint's `scheme` is `http` and `allow_http_upstream` is `false`: `503` LinkUnavailable
  before any connection attempt.
- The upstream connection cannot be established or the upgrade negotiation fails: the mapped gateway
  error of the entry-2.4 error table (`502` DownstreamError or ProtocolError, `504` connection timeout,
  `503` LinkUnavailable), with `X-OAGW-Error-Source: gateway`.
- Either side closes or a frame read or write fails after `101`: both connections are torn down, the
  abort is recorded, and nothing is written where the closing side is the client.

**Steps**:

1. [ ] - `p1` - Actor sends the upgrade request to `/oagw/v1/proxy/{alias}/{path}` carrying - `inst-ss-wsx-01`
   `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Key` and `Sec-WebSocket-Version`
2. [ ] - `p1` - The entry-2.4 pipeline resolves, matches, merges, hooks, selects, validates and - `inst-ss-wsx-02`
   transforms the request, strips the hop-by-hop headers including `Upgrade` and `Connection`, and
   hands this feature the request-side context — the selected endpoint, the transformed header set and
   the request context — before its own upstream call, so this feature receives an undialed request and
   not an open exchange
3. [ ] - `p1` - Validate the upgrade header set of the handed-off request with - `inst-ss-wsx-03`
   `cpt-cf-oagw-algo-ws-upgrade` before any upstream call; a malformed upgrade is **RETURN**ed as `400`
   with `X-OAGW-Error-Source: gateway`
4. [ ] - `p1` - **IF** the selected endpoint's `scheme` is `http` and `allow_http_upstream` is `false` - `inst-ss-wsx-04`
   1. [ ] - `p1` - **RETURN** `503` LinkUnavailable as a problem+json body with - `inst-ss-wsx-05`
      `X-OAGW-Error-Source: gateway` before any connection attempt; the default posture stays
      HTTPS-only and the graded configuration opts in explicitly (DECOMPOSITION assumption 2)
5. [ ] - `p1` - **ELSE** re-inject the validated `Upgrade: websocket` and `Connection: Upgrade` pair and - `inst-ss-wsx-06`
   the client's `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and
   `Sec-WebSocket-Extensions` headers onto the upstream exchange after the entry-2.4 transformation, and
   dial the selected endpoint over the crate's `toolkit-http`/`pingora-*` stack — TLS for `wss`,
   plaintext for an admitted `http`
6. [ ] - `p1` - **IF** the upstream answers `101 Switching Protocols` - `inst-ss-wsx-07`
   1. [ ] - `p1` - Relay the `101` response and its headers verbatim to the client, adding no - `inst-ss-wsx-08`
      `Sec-WebSocket-Accept` and altering none the upstream computed, stamp
      `X-OAGW-Error-Source` on the head, and mark the session established in
      `cpt-cf-oagw-state-ws-session`
   2. [ ] - `p1` - Relay frames in both directions with `cpt-cf-oagw-algo-ws-relay` until either side - `inst-ss-wsx-09`
      closes, resetting the idle window on every frame relayed in either direction
   3. [ ] - `p1` - **IF** the client closes - `inst-ss-wsx-10`
      1. [ ] - `p1` - Close the upstream side and record the client-initiated close on the request - `inst-ss-wsx-11`
         context
   4. [ ] - `p1` - **ELSE IF** the upstream closes or a frame read or write fails - `inst-ss-wsx-12`
      1. [ ] - `p1` - Close the client side, record the outcome with `cpt-cf-oagw-algo-stream-error-classify`, - `inst-ss-wsx-13`
         and write no further frame
7. [ ] - `p1` - **ELSE** the upstream refused the upgrade or the upgrade attempt failed - `inst-ss-wsx-14`
   1. [ ] - `p1` - **IF** the upstream produced a response - `inst-ss-wsx-15`
      1. [ ] - `p1` - Pass its status, headers and body through unchanged with - `inst-ss-wsx-16`
         `X-OAGW-Error-Source: upstream` and start no relay
   2. [ ] - `p1` - **ELSE** the attempt itself failed - `inst-ss-wsx-17`
      1. [ ] - `p1` - Map the failure through the entry-2.4 error table and **RETURN** it with - `inst-ss-wsx-18`
         `X-OAGW-Error-Source: gateway`
8. [ ] - `p1` - **RETURN** the outcome — an established relay that ends when both sides close, or the - `inst-ss-wsx-19`
   passed-through refusal, or the mapped gateway error — with the request context closed for entry 2.7

### Close a Streamed Exchange from the Upstream Side

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-stream-close`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:

- The upstream ends the streamed body normally: the client connection is closed cleanly, the close is
  recorded with the reason `upstream_closed` and the byte counts, and the request context is closed.
- The upstream ends a WebSocket session with a close frame: the close is relayed to the client and both
  sides are closed, with the same recorded reason.

**Error Scenarios**:

- The upstream aborts or resets the connection mid-body instead of ending it: the abort path of
  `cpt-cf-oagw-algo-stream-error-classify` runs rather than the clean close, producing `502`
  StreamAborted when no body byte was flushed and a recorded teardown otherwise.
- The upstream goes silent rather than closing: the idle timeout of `cpt-cf-oagw-algo-stream-timeout`
  ends the exchange instead of a close event.

**Steps**:

1. [ ] - `p1` - Upstream ends the streamed exchange — end of the response body for an SSE stream, or a - `inst-ss-ucl-01`
   WebSocket close frame for an upgraded session
2. [ ] - `p1` - Observe the end of the stream in `cpt-cf-oagw-algo-sse-forward` or - `inst-ss-ucl-02`
   `cpt-cf-oagw-algo-ws-relay`, distinguishing a clean end from an abort by the exchange's own
   termination signal, not by the presence or absence of body bytes
3. [ ] - `p1` - **IF** the exchange ended cleanly - `inst-ss-ucl-03`
   1. [ ] - `p1` - Close the client connection cleanly and flush any pending outbound chunk first, so no - `inst-ss-ucl-04`
      tail byte the upstream already sent is dropped
   2. [ ] - `p1` - Record the close on the request context with the reason `upstream_closed`, the - `inst-ss-ucl-05`
      direction, the byte counts and the duration, for entry 2.7
   3. [ ] - `p1` - Advance `cpt-cf-oagw-state-stream-lifecycle` to its terminal `closed` state - `inst-ss-ucl-06`
4. [ ] - `p1` - **ELSE** the upstream aborted or reset the connection - `inst-ss-ucl-07`
   1. [ ] - `p1` - Run `cpt-cf-oagw-algo-stream-error-classify` and take the aborted outcome it returns, - `inst-ss-ucl-08`
      writing a `502` StreamAborted problem+json response only where no body byte has been flushed
5. [ ] - `p1` - Release the exchange's buffers, the relay task and the upstream connection, so a closed - `inst-ss-ucl-09`
   stream holds no resource
6. [ ] - `p1` - **RETURN** the closed outcome to the caller of the flow; no audit record and no metric - `inst-ss-ucl-10`
   is emitted here

### Tear Down on a Client Disconnect

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-stream-disconnect`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- A client closes its connection mid-stream — an SSE consumer that stops reading, a WebSocket client
  that sends a close frame or simply vanishes — and the upstream connection is closed as a result, so
  the gateway holds no orphaned upstream exchange.
- The disconnect happens before any body byte was flushed: no response is written, because there is no
  client connection to write one to, and the teardown is still recorded.

**Error Scenarios**:

- The upstream connection cannot be closed promptly after the client disconnect: the teardown is
  recorded with the close reason `client_disconnected` and the upstream side is abandoned to the
  transport's own close, rather than blocking the teardown on it.
- A client disconnect races a chunk that was already read from the upstream: the chunk is discarded, not
  buffered for a client that is gone, and no error is raised to the caller.

**Steps**:

1. [ ] - `p1` - Actor disconnects — the client connection errors, closes or half-closes while a stream - `inst-ss-cdc-01`
   is open or relaying
2. [ ] - `p1` - Detect the disconnect in the read or write half of the relay in - `inst-ss-cdc-02`
   `cpt-cf-oagw-algo-stream-lifecycle`, before any further upstream read is issued
3. [ ] - `p1` - Close the upstream connection immediately; do not drain the upstream body and do not - `inst-ss-cdc-03`
   read to the end of it
4. [ ] - `p1` - Release the in-flight chunk, the relay buffers and the relay task - `inst-ss-cdc-04`
5. [ ] - `p1` - Record the close on the request context with the reason `client_disconnected`, the - `inst-ss-cdc-05`
   direction and the byte counts, for entry 2.7
6. [ ] - `p1` - Advance `cpt-cf-oagw-state-stream-lifecycle` to its terminal `aborted` state and write - `inst-ss-cdc-06`
   no response to the client
7. [ ] - `p1` - **RETURN** the torn-down outcome; the abort is recorded, not reported to a client that - `inst-ss-cdc-07`
   is no longer there

### Report a Stream Error and Its Source

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-stream-error`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:

- A gateway-generated stream failure — a refused plaintext upgrade, a malformed upgrade request, an
  upstream abort before the first body byte, an idle timeout before the head was committed — reaches the
  caller as an `application/problem+json` body with the GTS `type` identifier of the canonical error
  table and `X-OAGW-Error-Source: gateway`.
- An upstream error response on a stream — an upstream that answers the upgrade with a refusal status,
  or a non-2xx status on a request that asked for `text/event-stream` — is passed through with its own
  status, headers and body and carries `X-OAGW-Error-Source: upstream`.
- The header is present on a stream response that succeeds as well: it is stamped on the response head
  before the stream starts, per the entry-2.4 rule, so a consumer never has to wait for the stream to
  end to learn the source.
- The error source of a streamed exchange is decided once, at the head, and is never changed by what
  happens later in the body: an abort after the head cannot rewrite a `gateway` head into an
  `upstream` one or the reverse.

**Error Scenarios**:

- An intermediary strips `X-OAGW-Error-Source`: the consumer falls back to inspecting the response
  structure — a problem+json body with a GTS `type` identifier indicates a gateway error — which is the
  fallback ADR 0007 records for buffered responses and the only one available mid-stream too.
- A failure occurs after the head and body bytes were committed: no problem+json document is written
  into a body that already carries upstream bytes, and the abort is recorded on the request context
  instead.

**Steps**:

1. [ ] - `p1` - Actor sends a streaming or upgrade request whose exchange fails - `inst-ss-err-01`
2. [ ] - `p1` - Classify the failure with `cpt-cf-oagw-algo-stream-error-classify`, from the failure - `inst-ss-err-02`
   kind, the point in the lifecycle it occurred at, and the stream kind
3. [ ] - `p1` - **IF** the failure originated inside the gateway and the response head has not been - `inst-ss-err-03`
   committed
   1. [ ] - `p1` - Build the canonical problem document through the entry-2.1 mapping layer with the - `inst-ss-err-04`
      `type` identifier the failure maps to (`502` StreamAborted, `504` IdleTimeout, `503`
      LinkUnavailable, `400` ValidationError, `502` DownstreamError or ProtocolError), attaching the
      extension fields the request context provides
   2. [ ] - `p1` - Stamp `X-OAGW-Error-Source: gateway` on the response head - `inst-ss-err-05`
4. [ ] - `p1` - **ELSE IF** the upstream produced an error response - `inst-ss-err-06`
   1. [ ] - `p1` - Pass the status, headers and body through unchanged, add no gateway-generated body, - `inst-ss-err-07`
      and stamp `X-OAGW-Error-Source: upstream` on the head before the body is forwarded
5. [ ] - `p1` - **ELSE** the head was already committed and the failure is a mid-stream abort or - `inst-ss-err-08`
   teardown
   1. [ ] - `p1` - Tear the exchange down, write no body, and record the error type and close reason on - `inst-ss-err-09`
      the request context for entry 2.7
6. [ ] - `p1` - Never place credential material, a resolved secret, a request body byte or a header - `inst-ss-err-10`
   value in a problem document or in the recorded context
7. [ ] - `p1` - **RETURN** the response, or the teardown, with the error source fixed on the head - `inst-ss-err-11`

### Tear Down an Idle Streamed Exchange

- [ ] `p1` - **ID**: `cpt-cf-oagw-flow-stream-timeout`

**Actor**: `cpt-cf-oagw-actor-upstream-service`

**Success Scenarios**:

- The upstream stops sending bytes — and the client stops sending frames, for an upgraded session — for
  the idle window of `oagw.config.proxy_timeout_secs`; the exchange is torn down, the resources are
  released, and the teardown is recorded with the error type of the idle timeout.
- A stream that keeps producing bytes, or an upgraded session that keeps exchanging frames, is never
  torn down by this path however long it runs: the window is an idle window, not a total-duration cap,
  and it resets on every byte or frame in either direction.

**Error Scenarios**:

- The idle window elapses before the response head was committed to the client: the teardown is
  reported as `504` with the `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` `type` identifier and
  `X-OAGW-Error-Source: gateway`.
- The idle window elapses after the head was committed: no body is fabricated, the exchange is torn
  down, and the teardown is recorded with the idle-timeout error type on the request context.
- The window is misconfigured as zero or absent: the recorded entry-2.1 default of
  `oagw.config.proxy_timeout_secs` applies, and the stream is never left unbounded.

**Steps**:

1. [ ] - `p1` - Upstream falls silent on an open or relaying exchange; no byte arrives in either - `inst-ss-idle-01`
   direction
2. [ ] - `p1` - Run `cpt-cf-oagw-algo-stream-timeout` with the configured idle window taken from - `inst-ss-idle-02`
   `oagw.config.proxy_timeout_secs`
3. [ ] - `p1` - **IF** a byte or a frame is relayed in either direction before the window elapses - `inst-ss-idle-03`
   1. [ ] - `p1` - Reset the window and continue relaying; no teardown, no error and no state change - `inst-ss-idle-04`
      follows from the elapsed portion of the window
4. [ ] - `p1` - **ELSE** the window elapses with no bytes in either direction - `inst-ss-idle-05`
   1. [ ] - `p1` - Tear down the exchange: close the client and the upstream sides, release the buffers - `inst-ss-idle-06`
      and the relay task
   2. [ ] - `p1` - **IF** no body byte has been flushed to the client and the head is still writable - `inst-ss-idle-07`
      1. [ ] - `p1` - **RETURN** `504` IdleTimeout as a problem+json body with the - `inst-ss-idle-08`
         `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` `type` identifier and
         `X-OAGW-Error-Source: gateway`
   3. [ ] - `p1` - **ELSE** record the teardown with the idle-timeout error type on the request context - `inst-ss-idle-09`
      and write no body
5. [ ] - `p1` - Advance `cpt-cf-oagw-state-stream-lifecycle` to its terminal `timed_out` state and close - `inst-ss-idle-10`
   the request context for entry 2.7
6. [ ] - `p1` - **RETURN** the torn-down outcome - `inst-ss-idle-11`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. These are the
stages of the streamed half of the exchange, in the order an exchange meets them; each is entered from
the entry-2.4 handoff or from another stage below, and each records its outcome on the request context
rather than emitting an audit record or a metric.

### Incremental SSE Forwarding

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-sse-forward`

**Input**: the open upstream exchange handed off by entry 2.4 (response head and unread body), the
client connection, the request context, the request's `Accept` header value and the response's
`Content-Type` header value.

**Output**: the forwarded stream on the client connection, and the terminal outcome recorded on the
request context — `upstream_closed`, `client_disconnected`, `aborted` or `idle_timeout` — with the byte
counts and the close reason.

**Steps**:

1. [ ] - `p1` - Classify the exchange from the two independent facts: an SSE **response** is declared by - `inst-ss-fwd-01`
   a `Content-Type` of `text/event-stream`, an SSE **request** by an `Accept` header that names
   `text/event-stream`; record both on the request context and treat a streamed body declared either
   way as a stream this algorithm forwards
2. [ ] - `p1` - Relay the response head to the client as received: add no `Content-Length`, do not - `inst-ss-fwd-02`
   recompute or alter `Content-Type`, preserve the upstream's `Cache-Control` values including
   `no-cache` and `no-transform`, and forward the upstream's declared framing; the head already carries
   `X-OAGW-Error-Source` from the entry-2.4 header layer
3. [ ] - `p1` - Assert the body-limit checks of `cpt-cf-oagw-constraint-body-limit` were not applied to - `inst-ss-fwd-03`
   this body — the entry-2.4 handoff contract leaves it unread — and read no more than one chunk ahead
4. [ ] - `p1` - **FOR EACH** chunk the upstream body yields - `inst-ss-fwd-04`
   1. [ ] - `p1` - Write the chunk to the client connection unchanged, preserving chunk boundaries as - `inst-ss-fwd-05`
      received where the transport exposes them and performing no event re-serialization, no field
      rewriting and no re-chunking of the byte sequence
   2. [ ] - `p1` - Flush the client connection before reading the next chunk, so an event reaches the - `inst-ss-fwd-06`
      caller when the upstream produced it and not when the stream ends
   3. [ ] - `p1` - Reset the idle window of `cpt-cf-oagw-algo-stream-timeout` and add the chunk's byte - `inst-ss-fwd-07`
      count to the request context
   4. [ ] - `p1` - **IF** the client write fails or the client connection is gone - `inst-ss-fwd-08`
      1. [ ] - `p1` - Take the client-disconnect outcome of `cpt-cf-oagw-flow-stream-disconnect`: close - `inst-ss-fwd-09`
         the upstream connection, release the buffers, record `client_disconnected`, write no response
5. [ ] - `p1` - **TRY** to read the next chunk from the upstream body - `inst-ss-fwd-10`
6. [ ] - `p1` - **CATCH** an upstream read failure, a connection reset or an aborted exchange - `inst-ss-fwd-11`
   1. [ ] - `p1` - Hand the failure to `cpt-cf-oagw-algo-stream-error-classify`, which returns the - `inst-ss-fwd-12`
      aborted outcome, and emit a `502` StreamAborted problem+json response only when no body byte has
      been flushed to the client
7. [ ] - `p1` - **IF** the upstream body ends without error - `inst-ss-fwd-13`
   1. [ ] - `p1` - Take the clean-close outcome of `cpt-cf-oagw-flow-stream-close`: flush the tail, - `inst-ss-fwd-14`
      close the client connection cleanly, record `upstream_closed` with the byte counts
8. [ ] - `p1` - **RETURN** the terminal outcome and the closed exchange; no buffered residue of the body - `inst-ss-fwd-15`
   is retained after the return

### WebSocket Upgrade over the Selected Endpoint

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ws-upgrade`

**Input**: the request-side context handed over by entry 2.4 before its upstream call — the client
upgrade request with its header set as the entry-2.4 transformation left it (the hop-by-hop headers
stripped), the selected `Endpoint` with its `scheme`, `host` and `port`, and the request context —
together with the effective configuration and the gear configuration key
`oagw.config.allow_http_upstream`. No upstream connection is open when the algorithm starts, and this
algorithm dials the selected endpoint itself.

**Output**: an established upstream connection with the upgrade accepted and the `101 Switching
Protocols` response relayed verbatim to the client, or the classified failure that the entry-2.4 error
table maps.

**Steps**:

1. [ ] - `p1` - Validate the upgrade request before any upstream call: `Upgrade` names `websocket`, - `inst-ss-upg-01`
   `Connection` names `Upgrade`, `Sec-WebSocket-Key` is present and a malformed or unsupported
   `Sec-WebSocket-Version` is rejected
2. [ ] - `p1` - **IF** the upgrade request is not well formed - `inst-ss-upg-02`
   1. [ ] - `p1` - **RETURN** `400` with the `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` - `inst-ss-upg-03`
      `type` identifier and `X-OAGW-Error-Source: gateway`; no upstream connection is opened
3. [ ] - `p1` - Apply the scheme posture of `cpt-cf-oagw-constraint-https-only` with the assumption-2 - `inst-ss-upg-04`
   correction: a `wss` endpoint carries the upgrade over TLS, and an `http` endpoint is admitted only
   when `oagw.config.allow_http_upstream` is `true`
4. [ ] - `p1` - **IF** the selected endpoint's `scheme` is `http` and `allow_http_upstream` is `false` - `inst-ss-upg-05`
   1. [ ] - `p1` - **RETURN** `503` with the `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` - `inst-ss-upg-06`
      `type` identifier and `X-OAGW-Error-Source: gateway` before any connection attempt; no new error
      type is introduced for this refusal
5. [ ] - `p1` - **ELSE** dial the selected endpoint over the crate's existing - `inst-ss-upg-07`
   `toolkit-http`/`pingora-*` stack — TLS for `wss`, plaintext HTTP/1.1 for an admitted `http` — with
   the connect target always the stored endpoint and never a client-supplied host
6. [ ] - `p1` - Re-inject onto the upstream exchange, after `cpt-cf-oagw-algo-header-transform` has run - `inst-ss-upg-08`
   and in addition to the headers that transformation left in place, exactly: `Upgrade: websocket`,
   `Connection: Upgrade`, and the client's `Sec-WebSocket-Key`, `Sec-WebSocket-Version`,
   `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions` where the client supplied them
7. [ ] - `p1` - Inject no `Sec-WebSocket-Accept`: that header is computed by the peer that accepts the - `inst-ss-upg-09`
   upgrade, which is the upstream, and this gateway computes or validates it nowhere
8. [ ] - `p1` - Send the upgrade request as exactly one request, with no re-issue of the client request - `inst-ss-upg-10`
   as a whole
9. [ ] - `p1` - **IF** the upstream answers `101 Switching Protocols` - `inst-ss-upg-11`
   1. [ ] - `p1` - Relay the `101` status line and its headers verbatim to the client, including the - `inst-ss-upg-12`
      upstream's `Sec-WebSocket-Accept` and any negotiated `Sec-WebSocket-Protocol` or
      `Sec-WebSocket-Extensions`, adding none and altering none
   2. [ ] - `p1` - Stamp `X-OAGW-Error-Source` on the head through the entry-2.1 header layer and mark - `inst-ss-upg-13`
      the session established in `cpt-cf-oagw-state-ws-session`
   3. [ ] - `p1` - **RETURN** the established pair of connections to the caller for - `inst-ss-upg-14`
      `cpt-cf-oagw-algo-ws-relay`
10. [ ] - `p1` - **ELSE IF** the upstream answered with any other status - `inst-ss-upg-15`
    1. [ ] - `p1` - Pass the status, headers and body through unchanged, stamp - `inst-ss-upg-16`
       `X-OAGW-Error-Source: upstream` on the head, and start no relay
11. [ ] - `p1` - **ELSE** the upgrade attempt itself failed - `inst-ss-upg-17`
    1. [ ] - `p1` - Classify the failure onto the entry-2.4 error table — connection or request timeout, - `inst-ss-upg-18`
       unreachable link, protocol error, downstream error — and **RETURN** the mapped gateway error
12. [ ] - `p1` - Record the upgrade outcome, the endpoint and the negotiated headers on the request - `inst-ss-upg-19`
    context for entry 2.7, and no header value beyond that

### Bidirectional Frame Relay

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-ws-relay`

**Input**: the established client connection and the established upstream connection from
`cpt-cf-oagw-algo-ws-upgrade`, and the configured idle window.

**Output**: the frames relayed in both directions for the life of the session, and the terminal outcome
recorded on the request context — `closed` with the closing side, `aborted` or `idle_timeout`.

**Steps**:

1. [ ] - `p1` - Run two relay directions over the established pair — client to upstream and upstream to - `inst-ss-rlb-01`
   client — concurrently, with neither direction blocking or starving the other
2. [ ] - `p1` - **FOR EACH** frame read on either direction - `inst-ss-rlb-02`
   1. [ ] - `p1` - Write the frame to the other side verbatim, with no interpretation of payload - `inst-ss-rlb-03`
      content, no reassembly of fragmented messages, no subprotocol translation and no buffering beyond
      the frame in flight
   2. [ ] - `p1` - Reset the idle window and add the frame's byte count to the request context - `inst-ss-rlb-04`
3. [ ] - `p1` - **IF** a side sends a close frame or closes its connection - `inst-ss-rlb-05`
   1. [ ] - `p1` - Relay the close to the other side, close both sides, and record `closed` with the - `inst-ss-rlb-06`
      side that closed first
4. [ ] - `p1` - **ELSE IF** a read or a write fails on either side - `inst-ss-rlb-07`
   1. [ ] - `p1` - Tear both sides down immediately, take the aborted outcome of - `inst-ss-rlb-08`
      `cpt-cf-oagw-algo-stream-error-classify`, and write no frame to a side that has gone away
5. [ ] - `p1` - **ELSE IF** the idle window elapses with no frame in either direction - `inst-ss-rlb-09`
   1. [ ] - `p1` - Take the teardown of `cpt-cf-oagw-algo-stream-timeout`, close both sides, and record - `inst-ss-rlb-10`
      the idle-timeout outcome
6. [ ] - `p1` - Release both connections, the relay tasks and the buffers when the relay ends, so a - `inst-ss-rlb-11`
   finished session holds no resource
7. [ ] - `p1` - **RETURN** the terminal outcome with the closing side and the byte counts in each - `inst-ss-rlb-12`
   direction

### Streamed Exchange Lifecycle Tracking

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-stream-lifecycle`

**Input**: the handed-off open exchange, the stream kind (SSE response, SSE request, negotiated
upgrade), the client connection, the request context, and the events the forwarding and relay stages
raise.

**Output**: the lifecycle state of the exchange as `cpt-cf-oagw-state-stream-lifecycle` declares it,
and the recorded close reason, error type, direction and byte counts entry 2.7 consumes.

**Steps**:

1. [ ] - `p1` - Open the lifecycle at the handoff, in the `handed_off` state, recording the stream kind - `inst-ss-lif-01`
   and the identity of the selected endpoint on the request context
2. [ ] - `p1` - Advance the state on exactly the transitions `cpt-cf-oagw-state-stream-lifecycle` - `inst-ss-lif-02`
   declares, and on no others: the head relayed, the first chunk flushed, the clean end, the abort, the
   client disconnect and the idle teardown
3. [ ] - `p1` - Record on the context, at each transition, the close reason, the failing direction, the - `inst-ss-lif-03`
   byte counts in each direction and the error type, and no header value, body byte or query string
4. [ ] - `p1` - **IF** the client side goes away while the lifecycle is in `handed_off`, `open` or - `inst-ss-lif-04`
   `relaying`
   1. [ ] - `p1` - Close the upstream connection before issuing any further upstream read, and take the - `inst-ss-lif-05`
      `aborted` terminal state with the reason `client_disconnected`
5. [ ] - `p1` - **IF** the upstream side goes away while the lifecycle is in `handed_off`, `open` or - `inst-ss-lif-06`
   `relaying`
   1. [ ] - `p1` - Classify the loss with `cpt-cf-oagw-algo-stream-error-classify` and take the terminal - `inst-ss-lif-07`
      state it returns — `closed` for a clean end, `aborted` for a reset or abort
6. [ ] - `p1` - Treat `closed`, `aborted` and `timed_out` as terminal: no transition leaves them, no - `inst-ss-lif-08`
   stream is re-entered after it ends, and the same exchange is never relayed twice
7. [ ] - `p1` - Hold the lifecycle in memory only, per DECOMPOSITION assumption 3: a process restart - `inst-ss-lif-09`
   ends every stream the process was relaying and leaves no lifecycle to recover
8. [ ] - `p1` - **RETURN** the lifecycle state and the recorded outcome - `inst-ss-lif-10`

### Stream Error Classification and Source Stamping

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-stream-error-classify`

**Input**: the failure raised on a streamed exchange, the lifecycle state it occurred in, the stream
kind, whether the response head has been committed to the client, whether a body byte has been
flushed, and the request context.

**Output**: the terminal outcome for the exchange and, where the head is still writable, the mapped
gateway response with its status, GTS `type` identifier, problem+json body and
`X-OAGW-Error-Source: gateway`.

**Steps**:

1. [ ] - `p1` - Determine whether the response head has been committed and whether any body byte has - `inst-ss-cls-01`
   been flushed; these two facts decide whether a problem+json response is still possible
2. [ ] - `p1` - Classify the failure onto the canonical error table rows this feature uses, adding no - `inst-ss-cls-02`
   row: an upstream reset or abort, a mid-body loss or an upstream abort before the first body byte map
   to `502` StreamAborted (`gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, not retriable); a
   silent exchange past the idle window maps to `504` IdleTimeout
   (`gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`); a refused plaintext upgrade maps to `503`
   LinkUnavailable; a malformed upgrade request maps to `400` ValidationError; a failed upgrade attempt
   maps to the `502` DownstreamError, `502` ProtocolError, `503` LinkUnavailable or `504` connection
   timeout row the entry-2.4 table already fixes for that cause
3. [ ] - `p1` - **IF** the failure originated inside the gateway and the head has not been committed - `inst-ss-cls-03`
   1. [ ] - `p1` - Build the canonical `application/problem+json` document through the entry-2.1 - `inst-ss-cls-04`
      mapping layer with the `type` identifier, `title`, `status`, `detail`, `instance` and the extension
      fields the request context provides, and stamp `X-OAGW-Error-Source: gateway`
4. [ ] - `p1` - **ELSE IF** the upstream produced an error response and the head has not been committed - `inst-ss-cls-05`
   1. [ ] - `p1` - Pass the status, headers and body through unchanged, add no gateway-generated body, - `inst-ss-cls-06`
      and stamp `X-OAGW-Error-Source: upstream` on the head
5. [ ] - `p1` - **ELSE** the head was committed or a body byte was flushed - `inst-ss-cls-07`
   1. [ ] - `p1` - Tear the exchange down without writing a body, and record the error type and the - `inst-ss-cls-08`
      close reason on the request context; a mid-stream problem document is never spliced into a body
      the client already receives as upstream bytes
6. [ ] - `p1` - Keep the error source of the exchange fixed once the head is stamped: no later event on - `inst-ss-cls-09`
   the stream rewrites it
7. [ ] - `p1` - Record the classified error type on the request context for entry 2.7 and never place - `inst-ss-cls-10`
   credential material, a request body byte or a header value in the document or the context
8. [ ] - `p1` - **RETURN** the terminal outcome and the mapped response where one is produced - `inst-ss-cls-11`

### Idle Timeout for Streams

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-stream-timeout`

**Input**: the open or relaying exchange, the idle window read from `oagw.config.proxy_timeout_secs`,
and the last activity timestamp in each direction.

**Output**: the decision to keep relaying or to tear the exchange down, and the recorded
idle-timeout outcome where the teardown happens.

**Steps**:

1. [ ] - `p1` - Arm one idle window per streamed exchange, spanning both directions, from the same - `inst-ss-tmo-01`
   `oagw.config.proxy_timeout_secs` value the entry-2.4 call used; add no separate stream timeout
   configuration
2. [ ] - `p1` - Reset the window on every byte forwarded downstream and every byte or frame received - `inst-ss-tmo-02`
   upstream, so a long-lived but active stream never expires
3. [ ] - `p1` - **IF** the window elapses with no byte or frame in either direction - `inst-ss-tmo-03`
   1. [ ] - `p1` - Tear the exchange down: close the client and the upstream sides, cancel the relay - `inst-ss-tmo-04`
      tasks and release the buffers
   2. [ ] - `p1` - Map the teardown onto the existing `504` IdleTimeout row - `inst-ss-tmo-05`
      (`gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`, retriable) with
      `X-OAGW-Error-Source: gateway` where the head is still writable, and record the teardown with the
      idle-timeout error type on the request context where it is not
4. [ ] - `p1` - **ELSE** keep relaying and re-arm the window - `inst-ss-tmo-06`
5. [ ] - `p1` - Advance `cpt-cf-oagw-state-stream-lifecycle` to `timed_out` on a teardown, and record - `inst-ss-tmo-07`
   the window value that fired on the request context
6. [ ] - `p1` - **RETURN** the decision and the recorded outcome - `inst-ss-tmo-08`

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

Two lifecycles belong to this feature: the streamed exchange this feature is handed at the entry-2.4
handoff, and the WebSocket session an upgrade establishes inside it. Both are held in memory for the
life of the exchange only (DECOMPOSITION assumption 3). The resources the exchange is built on have no
lifecycle here — the upstream and route lifecycles are entry 2.2's state machines, the circuit breaker
and the buffered request context are entry 2.4's, and this feature only observes the handed-off result
of both.

### Streamed Exchange State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-stream-lifecycle`

**States**: `handed_off`, `open`, `relaying`, `closed`, `aborted`, `timed_out`

**Initial State**: `handed_off`

**Transitions**:

1. [ ] - `p1` - **FROM** `handed_off` **TO** `open` **WHEN** the upstream response head has been relayed - `inst-ss-stl-01`
   to the client with `X-OAGW-Error-Source` on it
2. [ ] - `p1` - **FROM** `handed_off` **TO** `aborted` **WHEN** the head cannot be relayed — the client - `inst-ss-stl-02`
   connection is already gone or the head write fails — and the upstream exchange is closed
3. [ ] - `p1` - **FROM** `handed_off` **TO** `timed_out` **WHEN** the idle window elapses with no byte - `inst-ss-stl-03`
   in either direction before the head is relayed
4. [ ] - `p1` - **FROM** `open` **TO** `relaying` **WHEN** the first body chunk is received from the - `inst-ss-stl-04`
   upstream and flushed to the client
5. [ ] - `p1` - **FROM** `open` **TO** `closed` **WHEN** the upstream ends the streamed response with an - `inst-ss-stl-05`
   empty body
6. [ ] - `p1` - **FROM** `open` **TO** `aborted` **WHEN** the upstream connection fails, resets or aborts - `inst-ss-stl-06`
   before the first chunk, or the client disconnects
7. [ ] - `p1` - **FROM** `open` **TO** `timed_out` **WHEN** the idle window elapses with no byte in - `inst-ss-stl-07`
   either direction
8. [ ] - `p1` - **FROM** `relaying` **TO** `closed` **WHEN** the upstream ends the stream normally, the - `inst-ss-stl-08`
   client connection is closed cleanly and the close is recorded
9. [ ] - `p1` - **FROM** `relaying` **TO** `aborted` **WHEN** the client disconnects, or the upstream - `inst-ss-stl-09`
   resets or aborts mid-body
10. [ ] - `p1` - **FROM** `relaying` **TO** `timed_out` **WHEN** the idle window elapses with no byte in - `inst-ss-stl-10`
    either direction

**Closed transition set**: the transitions above are the only ones possible. `closed`, `aborted` and
`timed_out` are terminal — no transition leaves them, no stream is re-entered after it ends and no
state is skipped or re-entered on its own; a `handed_off` exchange that is never classified as a stream
returns to the entry-2.4 buffered path and enters none of these transitions. A `closed` stream ended
because the upstream ended it, an `aborted` stream ended because a side dropped or failed, and a
`timed_out` stream ended because the idle window elapsed; the three are recorded distinctly on the
request context for entry 2.7. The machine is per exchange and process-local (in-memory, DECOMPOSITION
assumption 3), so a host process restart leaves no stream state behind.

### WebSocket Session State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-ws-session`

**States**: `upgrading`, `established`, `relaying`, `rejected`, `closed`, `aborted`, `timed_out`

**Initial State**: `upgrading`

**Transitions**:

1. [ ] - `p1` - **FROM** `upgrading` **TO** `established` **WHEN** the upstream answers `101 Switching - `inst-ss-stw-01`
   Protocols` and the response has been relayed verbatim to the client
2. [ ] - `p1` - **FROM** `upgrading` **TO** `rejected` **WHEN** the upstream refuses the upgrade with a - `inst-ss-stw-02`
   status other than `101`, the plaintext gate refuses an `http` endpoint with `allow_http_upstream:
   false`, or the upgrade request is malformed
3. [ ] - `p1` - **FROM** `upgrading` **TO** `aborted` **WHEN** the upgrade attempt fails — an - `inst-ss-stw-03`
   unreachable upstream, a connection or request timeout, or a client disconnect during the upgrade
4. [ ] - `p1` - **FROM** `established` **TO** `relaying` **WHEN** the first frame is read on either side - `inst-ss-stw-04`
   and forwarded to the other
5. [ ] - `p1` - **FROM** `established` **TO** `closed` **WHEN** either side closes before any frame is - `inst-ss-stw-05`
   relayed, and the close is relayed to the other side
6. [ ] - `p1` - **FROM** `established` **TO** `aborted` **WHEN** a read or write fails, or the client - `inst-ss-stw-06`
   disconnects, before any frame is relayed
7. [ ] - `p1` - **FROM** `relaying` **TO** `closed` **WHEN** either side sends a close frame or closes - `inst-ss-stw-07`
   its connection and the close reaches the other side
8. [ ] - `p1` - **FROM** `relaying` **TO** `aborted` **WHEN** a frame read or write fails on either - `inst-ss-stw-08`
   side, or the client disconnects abruptly
9. [ ] - `p1` - **FROM** `relaying` **TO** `timed_out` **WHEN** no frame is relayed in either direction - `inst-ss-stw-09`
   for the idle window
10. [ ] - `p1` - **FROM** `established` **TO** `timed_out` **WHEN** the idle window elapses before any - `inst-ss-stw-10`
    frame is relayed

**Closed transition set**: the transitions above are the only ones possible. `rejected`, `closed`,
`aborted` and `timed_out` are terminal — no transition leaves them, a session is never upgraded twice
and no state is skipped or re-entered. A `rejected` session produced a response to the client (the
upstream's own refusal passed through, or a gateway problem+json error), an `aborted` or `timed_out`
session was torn down with the outcome recorded on the request context, and a `closed` session ended
because one of its sides ended it. The machine is per session and process-local (in-memory,
DECOMPOSITION assumption 3); a restart ends every session the process held and there is no persisted
session state to recover.

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Incremental SSE Forwarding

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-sse-forwarding`

The system **MUST** forward a response whose `Content-Type` is `text/event-stream` incrementally from
the entry-2.4 handoff to the client connection, relaying the response head as received with no inserted
`Content-Length`, the upstream's `Cache-Control` posture unchanged and the upstream's framing passed
through, and writing and flushing each chunk as it arrives with no full-body buffering, no buffering
beyond one chunk in flight, no re-framing, no line rewriting and no event re-serialization. The system
**MUST** classify an SSE request by its `Accept: text/event-stream` header and an SSE response by its
`Content-Type`, treat the two as independent facts, and leave a request that asked for
`text/event-stream` but received a non-streamed response on the entry-2.4 buffered path. The system
**MUST** ensure the body-limit checks of `cpt-cf-oagw-constraint-body-limit` do not consume a streamed
body.

**Implements**:

- `cpt-cf-oagw-flow-sse-proxy`
- `cpt-cf-oagw-algo-sse-forward`
- `cpt-cf-oagw-algo-stream-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-body-limit`

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (SSE responses and SSE requests; no new endpoint)
- DB: none
- Entities: `RequestContext`, `ResponseContext`, `Endpoint`

### WebSocket Upgrade Construction and Header Re-injection

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-upgrade`

The system **MUST** validate an upgrade request (`Upgrade: websocket`, `Connection: Upgrade`, a present
`Sec-WebSocket-Key`, a supported `Sec-WebSocket-Version`) before any upstream call and reject a
malformed one with `400` and `X-OAGW-Error-Source: gateway`; **MUST** apply the
`cpt-cf-oagw-constraint-https-only` posture with the recorded assumption-2 correction — a `wss`
endpoint carries the upgrade over TLS, an `http` endpoint is admitted only when `allow_http_upstream`
is `true`, and a refused plaintext upgrade returns `503` LinkUnavailable before any connection attempt;
**MUST** re-inject onto the upstream exchange after the entry-2.4 header transformation exactly the
validated `Upgrade: websocket` and `Connection: Upgrade` pair plus the client's `Sec-WebSocket-Key`,
`Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions`, rather than exempting
the upgrade from the hop-by-hop stripping; **MUST NOT** compute, inject or validate
`Sec-WebSocket-Accept`, which the accepting upstream computes; **MUST** relay the `101 Switching
Protocols` response and its headers verbatim to the client; and **MUST** pass an upstream refusal
through with its own status, headers and body and `X-OAGW-Error-Source: upstream`.

**Implements**:

- `cpt-cf-oagw-flow-ws-proxy`
- `cpt-cf-oagw-algo-ws-upgrade`

**Constraints**: `cpt-cf-oagw-constraint-https-only`

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (WebSocket upgrade; no new endpoint)
- DB: none
- Entities: `Endpoint`, `RequestContext`, `ResponseContext`

### Bidirectional Frame Relay

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-ws-relay`

The system **MUST** relay frames in both directions between the client and the upstream over an
established upgrade, running the two directions concurrently without either blocking the other, copying
each frame verbatim with no payload interpretation, no message reassembly, no subprotocol translation
and no buffering beyond the frame in flight, until either side closes; **MUST** relay a close frame or
connection close to the other side and then close both sides; **MUST** tear both sides down immediately
when a read or a write fails on either direction; and **MUST** release both connections, the relay tasks
and the buffers when the relay ends.

**Implements**:

- `cpt-cf-oagw-flow-ws-proxy`
- `cpt-cf-oagw-algo-ws-relay`
- `cpt-cf-oagw-state-ws-session`

**Constraints**: None

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (upgraded sessions; no new endpoint)
- DB: none
- Entities: `RequestContext`, `ResponseContext`, `Endpoint`

### Stream Lifecycle Close Semantics

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-stream-lifecycle`

The system **MUST** run the open, data, close and error lifecycle of every streamed exchange through
`cpt-cf-oagw-state-stream-lifecycle`, and **MUST** close the client connection cleanly and record the
close when the upstream ends the stream, and close the upstream connection — without draining its body
— when the client disconnects, recording the close reason (`upstream_closed` or `client_disconnected`),
the direction, the byte counts and the error type on the request context for entry 2.7. The system
**MUST** release every buffer, task and connection a stream held once it ends, and **MUST NOT** re-enter
a terminal state, relay the same exchange twice, or hold any stream state across a process restart.

**Implements**:

- `cpt-cf-oagw-flow-stream-close`
- `cpt-cf-oagw-flow-stream-disconnect`
- `cpt-cf-oagw-algo-stream-lifecycle`
- `cpt-cf-oagw-state-stream-lifecycle`

**Constraints**: None

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (streamed exchanges; no new endpoint)
- DB: none
- Entities: `RequestContext`, `ResponseContext`

### Stream Error Classification and Error Source on Streams

- [ ] `p1` - **ID**: `cpt-cf-oagw-dod-stream-errors`

The system **MUST** classify every stream failure onto the existing canonical error table rows — an
aborted stream onto `502` StreamAborted
(`gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, not retriable), an idle teardown onto `504`
IdleTimeout, a refused plaintext upgrade onto `503` LinkUnavailable, a malformed upgrade onto `400`
ValidationError — through the entry-2.1 mapping layer as `application/problem+json`, and **MUST NOT**
add a new error type; **MUST** stamp `X-OAGW-Error-Source` on the response head before the stream
starts, with `gateway` on a gateway-generated stream error and `upstream` on a passed-through upstream
error response, and **MUST NOT** rewrite that value after the head is committed; **MUST** write a
problem+json stream error only when the head has not been committed and no body byte has been flushed;
and **MUST** tear the exchange down with the error type recorded on the request context when it has.

**Implements**:

- `cpt-cf-oagw-flow-stream-error`
- `cpt-cf-oagw-flow-stream-close`
- `cpt-cf-oagw-algo-stream-error-classify`
- `cpt-cf-oagw-algo-sse-forward`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (stream error responses; no new endpoint)
- DB: none
- Entities: `RequestContext`, `ResponseContext`

### Stream Scheme Posture and WebTransport Exclusion

- [x] `p1` - **ID**: `cpt-cf-oagw-dod-stream-scheme-posture`

The system **MUST** admit a WebSocket upgrade on a `wss` upstream endpoint unconditionally and on an
`http` upstream endpoint only when `oagw.config.allow_http_upstream` is `true`, refusing it with `503`
LinkUnavailable before any connection attempt otherwise, so the default posture stays HTTPS-only and
the graded configuration opts in explicitly; **MUST** dial only the selected endpoint taken from the
store, never a client-supplied host; and **MUST** implement no WebTransport session flow and no `wt`
code path of any kind here: the request-time exclusion of a `wt`-scheme upstream — accepted and stored
by entry 2.2 — is `cpt-cf-oagw-feature-proxy-engine`'s and is recorded at that entry's resolution step,
so no proxy request reaches this feature with a `wt` upstream and this feature implements and asserts
no part of that exclusion. The system **MUST** apply the idle window of
`oagw.config.proxy_timeout_secs` to every stream, in both directions, with no separate
stream timeout configuration, mapping the teardown onto the `504` IdleTimeout row where a response is
still writable and recording it otherwise.

**Implements**:

- `cpt-cf-oagw-flow-ws-proxy`
- `cpt-cf-oagw-flow-stream-timeout`
- `cpt-cf-oagw-algo-ws-upgrade`
- `cpt-cf-oagw-algo-stream-timeout`

**Constraints**: `cpt-cf-oagw-constraint-https-only`, `cpt-cf-oagw-constraint-no-direct-internet`

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (upgrade scheme posture; no new endpoint)
- DB: none
- Entities: `Endpoint`, `RequestContext`

### Test Layering for Streaming

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-stream-test-coverage`

The system **MUST** cover this feature with in-crate Rust tests only — unit tests inside
`#[cfg(test)]` modules per layer for the SSE detection and its two independent facts, the bounded
chunk forwarding and flush behaviour, the upgrade header validation and re-injection, the verbatim
`101` relay, the bidirectional frame relay and its close handling, the lifecycle transitions, the idle
window reset and expiry, and the stream error classification — and integration tests under the crate's
`tests/` directory that boot the gear router and drive the proxy endpoint against a stub upstream
listener provided by the test harness, including a stub that emits a `text/event-stream` body
incrementally and one that completes or refuses a WebSocket upgrade, asserting the forwarding contract,
the two close semantics, `X-OAGW-Error-Source` on streamed responses, the `StreamAborted`, IdleTimeout
and LinkUnavailable mappings and the absence of a `wt` code path in this feature's own detection,
upgrade and relay code. The system **MUST NOT** add an e2e suite under
`testing/e2e/gears/oagw/` and **MUST NOT** require a QUIC-capable test harness (DECOMPOSITION
assumptions 5 and 6).

**Implements**:

- `cpt-cf-oagw-flow-sse-proxy`
- `cpt-cf-oagw-flow-ws-proxy`
- `cpt-cf-oagw-flow-stream-error`
- `cpt-cf-oagw-algo-ws-upgrade`
- `cpt-cf-oagw-algo-ws-relay`
- `cpt-cf-oagw-state-stream-lifecycle`

**Constraints**: `cpt-cf-oagw-constraint-toolkit-deploy`

**Touches**:

- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (asserted by the integration tests; no new endpoint)
- DB: none — the DoD asserts behaviour, not storage
- Entities: `RequestContext`, `ResponseContext`, `Endpoint`

## 6. Acceptance Criteria

- [x] A response whose `Content-Type` is `text/event-stream` reaches the client as a stream: the first event arrives before the upstream ends the body, events arrive in upstream order, and the byte sequence the client receives is exactly the byte sequence the upstream produced.
- [x] No streamed response carries a `Content-Length` inserted by the gateway, and the upstream's `Cache-Control` values, including `no-cache` and `no-transform`, reach the client unchanged.
- [x] A request carrying `Accept: text/event-stream` whose response is not `text/event-stream` is served by the entry-2.4 buffered path with its response-phase plugin hook run, and no streaming path is entered for it.
- [x] When the upstream ends the stream, the client connection is closed cleanly, any tail byte the upstream already sent is flushed first, and the close is recorded with the reason `upstream_closed` on the request context.
- [x] When the client disconnects mid-stream, the upstream connection is closed without draining its body, the in-flight chunk is discarded, no response is written, and the close is recorded with the reason `client_disconnected`.
- [x] An upstream reset or abort before the first body byte is flushed returns `502` with the `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1` `type` identifier, an `application/problem+json` body and `X-OAGW-Error-Source: gateway`.
- [x] An upstream abort after the head was committed tears the exchange down, writes no fabricated body into the stream, and records the abort on the request context.
- [x] `X-OAGW-Error-Source` is present on the head of every streamed response — `gateway` on a gateway-generated stream error and `upstream` on a passed-through upstream error response — and is stamped before the first body byte is forwarded.
- [x] An upgrade request that is malformed (`Upgrade` not `websocket`, missing `Connection: Upgrade`, missing `Sec-WebSocket-Key`, unsupported `Sec-WebSocket-Version`) returns `400` with a problem+json body and `X-OAGW-Error-Source: gateway` before any upstream connection is opened.
- [x] The outbound upgrade request carries the re-injected `Upgrade: websocket` and `Connection: Upgrade` pair and the client's `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions`, carries no `Sec-WebSocket-Accept` and no other hop-by-hop header, and is sent to a `wss` endpoint over TLS.
- [x] A `101 Switching Protocols` response reaches the client with its status and headers verbatim, including the upstream's `Sec-WebSocket-Accept` and any negotiated `Sec-WebSocket-Protocol`, and frames then flow in both directions until either side closes.
- [x] When the client closes an upgraded session, the upstream side is closed and the close is recorded; when the upstream closes, the client side is closed with the close relayed to it.
- [x] An upgrade against an `http`-scheme endpoint with the recorded default `allow_http_upstream: false` returns `503` with the `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1` `type` identifier before any connection attempt; with the graded configuration the plaintext upgrade proceeds over HTTP/1.1.
- [x] No `wt`-scheme upstream is reachable from this feature: the request-time exclusion is recorded in `cpt-cf-oagw-feature-proxy-engine`'s alias-resolution step, so no streamed exchange is ever handed to this feature with a `wt` upstream and no `wt` branch exists in this feature's detection, upgrade or relay code; the `404` a caller sees for such a request is that entry's behaviour, asserted by its tests.
- [x] A stream with no bytes in either direction for `oagw.config.proxy_timeout_secs` is torn down, returning `504` with the `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1` `type` identifier and `X-OAGW-Error-Source: gateway` when the head is still writable and recording the teardown otherwise; an actively producing stream of any duration is not torn down by the window.
- [x] The lifecycle states `closed`, `aborted` and `timed_out` are terminal: no stream transitions out of them, no exchange is relayed twice, and the close reason, direction, byte counts and error type recorded for entry 2.7 contain no header value, body byte or query string.
- [x] The request-phase plugin hooks of entry 2.5 run before the handoff on a streaming request, and no response-phase transform runs on a streamed exchange; authentication, the proxy permission and the rate limit are enforced on an SSE request and on a WebSocket upgrade exactly as on a buffered request.
- [x] The in-crate unit and integration tests pass, the streaming assertions run against a stub upstream listener inside the `cf-gears-oagw` crate's own test targets, and no test artifact is added under `testing/e2e/gears/oagw/`.
