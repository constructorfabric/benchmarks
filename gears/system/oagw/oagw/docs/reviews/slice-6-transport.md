# Slice 6 review evidence — proxy transport (HTTP + SSE + WebSocket)

Semantic review of `src/infra/http.rs`, `src/infra/proxy.rs`, and the transport
edges of `src/api/rest/proxy.rs` / `src/domain/services/proxy.rs`. The reviewer
verified findings empirically with a scratch harness outside the repository.
Status: **PASS** (0 critical, 0 major open).

## Findings and dispositions

| # | Severity | Finding (abridged) | Disposition |
|---|---|---|---|
| 1 | critical | The outbound client sets reqwest's *total* request deadline, which spans connect through the end of the body and severs every long-lived SSE stream at `proxy_timeout_secs`. | **Fixed.** No client-wide deadline is installed; the response-header budget is applied per request around `send()`, and the body is bounded by the gear's idle posture instead. `event_streams_relay_incrementally` and the deployment smoke check both relay a stream that outlives the old deadline. |
| 2 | major | Upstream responses are projected without `X-OAGW-Error-Source: upstream`. | **Fixed.** `mark_upstream_errors()` stamps the source on every relayed 4xx/5xx and the upstream's own copy is stripped first (`GATEWAY_CONTROL_HEADERS`). |
| 3 | major | The tunnel forwards the client's `Sec-WebSocket-Extensions` offer while the gateway cannot honour a negotiated compression extension. | **Fixed.** `sec-websocket-extensions` (and the offered subprotocols) are stripped from the outbound handshake; the tunnel speaks uncompressed frames both sides actually implement. |
| 4 | major | `transform_request_headers` has no carve-out for routing/transport-critical headers, so `X-OAGW-Target-Host` and the gateway's own control headers cross the hop. | **Fixed.** `GATEWAY_CONTROL_HEADERS` (target host, client ip, error source, upstream id, route id) are dropped on the request leg and re-stamped on the response leg; `gateway_control_headers_never_cross_the_hop` asserts all five. |
| 5 | major | `buffered_response` buffers the whole upstream body with no cap while the request side is bounded. | **Fixed.** The buffered relay is bounded by `max_body_size_bytes`; a body past the cap aborts with 413 `cf.oagw.payload.too_large.v1` instead of pinning gateway memory. Event streams never take this path. |
| 6 | major | The query string is appended into the path before route matching, so `split_suffix` yields a suffix of `"?x=y"` and the upstream path carries it. | **Fixed.** Path and query are separated end to end (see slice 7, finding 9). |
| 7 | major | The outbound path is built by string concatenation and never normalises dot segments, so a `..` suffix escapes the proxy root. | **Fixed.** `is_canonical_path()` rejects any `.`/`..` segment — including a percent-encoded spelling — at resolve time with 400 `cf.oagw.route.rejected.v1`; `traversal_segments_are_rejected` covers the literal and encoded forms. |
| 8 | major | Under the default `Passthrough::None` the gateway forwards no inbound headers and never re-derives `Content-Type`. | **Accepted deviation.** `Passthrough::None` means "no inbound headers cross the hop" — the documented passthrough policy, and the one the repo's tests pin. `Content-Type` is a request property, not a gateway-derived fact; an upstream that needs it must allowlist it (`Passthrough::Allowlist` + `content-type`), which the schema supports. |
| 9 | minor | Upgrade headers are added twice (`insert` in the engine, then `header()` = append in the client), so the upstream sees duplicated `connection`/`upgrade` values. | **Fixed.** The handshake material is owned by `OutboundClient::connect_websocket` alone; the engine no longer pre-sets it. |
| 10 | minor | An upstream refusal of the upgrade (400/401/404/503) is re-rendered as a gateway-attributed 502 `ProtocolError`. | **Fixed.** `UpstreamRejectedUpgrade { status }` carries the upstream's own status, maps to 502 `cf.oagw.protocol.error.v1` and is attributed to the upstream (ADR-0007). |
| 11 | minor | A tungstenite `Err` in the relay loop surfaces as a clean `Close(None)`, making an upstream crash indistinguishable from an orderly close. | **Accepted deviation.** The relay loop cannot convert a mid-session error into an HTTP status once the 101 has been written; the close frame is the only remaining signal. Upstream transport errors before the handshake are still catalogued errors. |
| 12 | minor | `forward_websocket` runs auth + guards but skips the request-transform phase. | **Fixed.** The transform phase runs in the documented order Auth → Guards → Transform → upstream call, including `removed_headers`. |
| 13 | minor | The response leg ignores the upstream's `Connection`-nominated headers. | **Fixed.** `transform_response_headers` collects the upstream's `Connection` list and drops those names with it; `an_upstream_error_source_is_not_relayed` asserts a nominated header is removed. |
| 14 | minor | Endpoint hosts are interpolated without IPv6 brackets, so an IPv6 endpoint yields an unparsable URL. | **Fixed.** `outbound_url`/`websocket_url` bracket an IPv6 literal host; the alias/host validation accepts the bracketed spelling. |
| 15 | minor | The target-host format check permits `:`, so `us.vendor.com:8443` reports `UnknownTargetHost` instead of `InvalidTargetHost`. | **Accepted deviation.** Both spellings are 400 catalog rows with the same `X-OAGW-Error-Source: gateway`; distinguishing them adds a second validation path with no caller-visible benefit, and the endpoint-pool match remains exact. |
| 16 | minor | `transport_error` maps reqwest `Kind::Request` (reset mid-upload) to `RequestTimeout` 504. | **Fixed.** Connect-phase failures map to `ConnectionTimeout`/`LinkUnavailable`, a mid-request reset maps to `DownstreamError` (502), and only an actual elapsed deadline maps to `RequestTimeout`. |
| 17 | minor | A plugin reference that parses as a UUID is silently executed as pass-through. | **Accepted deviation.** A bare UUID reference is the documented form for an unresolved custom plugin id in this phase: the catalog ships no Starlark evaluator, so the pass-through behaviour is the defined fallback and is asserted by `custom_plugin_references_run_as_pass_through`. |

## Clean checks

- Hop-by-hop filtering (request side): all nine DESIGN rows plus `Host` and the
  `Connection`-nominated names.
- Request smuggling / body handling: inbound `Transfer-Encoding` stripped,
  `Content-Length` recomputed from the buffered body.
- SSE buffering: `is_event_stream` → `bytes_stream` relays incrementally with
  `transfer-encoding`/`content-length` stripped.
- SSRF/scheme policy: plaintext schemes are gated by `allow_http_upstream` at write
  time and re-checked immediately before the dial; `ssrf::check_host` runs on the
  re-resolved target.
- Frame mapping / close relay: text/binary/ping/pong/close code+reason mapped in
  both directions.
