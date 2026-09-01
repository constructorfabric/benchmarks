# oagw data plane — slice plan (dispatch B, slices 6..9)

Data plane for the `oagw` outbound API gateway. The control plane (slices 1..5)
is complete and reviewed; this plan only adds the forwarding path.

## Slice 6 — resolution + data-plane error identities — DONE
Implements:
- DESIGN §3.3 error table: new canonical identities
  `cf.oagw.protocol.protocol_error` (502), `.downstream_error` (502),
  `.stream_aborted` (502), `.circuit_breaker_open` (503),
  `.plugin_not_found` (503), `.connection_timeout` (504),
  `.request_timeout` (504), `.idle_timeout` (504).
- DESIGN ~395 (alias resolution): walk the tenant hierarchy descendant → root,
  closest (most derived) match wins; unknown alias → UnknownTargetHost.
- DESIGN ~338 (infra/proxy): route selection (longest matching path prefix,
  ties by lower `priority`), `path_suffix_mode` splicing, `query_allowlist`.
- DESIGN ~446 step 2: `x-oagw-target-host` override of the alias identity →
  MissingTargetHost / InvalidTargetHost / UnknownTargetHost.
- DESIGN §2.2: HTTPS-only upstream unless `allow_http_upstream`.
Files: `domain/error.rs`, `api/rest/error.rs`, `domain/repo.rs` (ports),
`infra/proxy/{mod,resolver,endpoint}.rs`.

## Slice 7 — plugin engine (ADR-0002, ADR-0008, ADR-0009) — DONE
Implements:
- ADR-0002 execution order Auth → Guards → Transform(on_request) → Upstream →
  Transform(on_response / on_error); upstream plugins before route plugins.
- ADR-0008 `ApiKeyAuthPlugin`, `OAuth2ClientCredAuthPlugin` (+ `_basic`) with
  `pingora_memory_cache::MemoryCache`, `toolkit_auth::oauth2::fetch_token`,
  TTL `min(config_ttl, expires_in − 30s)`, cache key encoding
  (tenant, subject, auth method tag, config hash), `CachedToken` key check.
- ADR-0009 `RequiredHeadersGuardPlugin` (presence-only, case-insensitive,
  first missing header, request 400 / response 502, fail-open on blank).
- `NoopAuthPlugin`, `RequestIdTransformPlugin`,
  `AuthPluginRegistry::with_builtins`, guard/transform registries.
Files: `domain/plugin.rs` (extend), `infra/plugin/{mod,registry,noop_auth,
apikey_auth,oauth2_client_cred_auth,required_headers_guard,
request_id_transform,secrets}.rs`.

## Slice 8 — policy enforcement — DONE
Implements:
- ADR-0003 token bucket, dual rate (sustained + burst) → 429 with
  `Retry-After` and `X-RateLimit-Limit/Remaining/Reset`.
- ADR-0004 CORS: preflight OPTIONS → 204, actual requests validate
  origin/method → 403, `Vary: Origin` always.
- DESIGN §2.2 body size hard limit 100MB → 413 PayloadTooLarge (no
  `body_limit_bytes` field exists in `upstream.v1.schema.json`).
- DESIGN §2.2 `proxy_timeout_secs` → 504 RequestTimeout; no automatic retries.
- DESIGN §2.2 `ssrf_policy` enforcement point (no-op while disabled, as
  configured in `config/e2e-local.yaml`).
Files: `infra/proxy/{ratelimit,cors,body,ssrf,policy}.rs` (+ tests).

## Slice 9 — transport (acceptance critical) — DONE
Implements:
- PRD §5.4 / FR streaming: plain HTTP proxy (streamed both ways), SSE
  chunk-by-chunk forwarding, WebSocket upgrade + bidirectional copy.
- DESIGN ~446 header rules: hop-by-hop strip both directions, `Host` rewrite,
  `upstream.headers.request/response` verbs
  set/add/remove/passthrough/passthrough_allowlist.
- ADR-0007 error mapping + `X-OAGW-Error-Source: gateway|upstream`.
- Replaces the `proxy_stub` 503 tail in `api/rest/handlers.rs`.
Files: `infra/proxy/{transport,forward}.rs`, `api/rest/handlers.rs`,
`gear.rs`, `api/rest/routes.rs`.

Evidence (all in-crate tests, `cargo test -p cf-gears-oagw --all-features`):
- plain HTTP end to end: `api::rest::handlers::tests::
  a_proxied_call_returns_the_upstream_status_body_and_source` and
  `infra::proxy::forward_tests::
  a_request_reaches_its_upstream_with_the_route_path_and_the_caller_headers`.
- SSE chunk-by-chunk, observable before the stream ends:
  `infra::proxy::transport_tests::
  a_streamed_response_body_arrives_before_it_is_complete`,
  `infra::proxy::forward_tests::
  a_streaming_answer_is_observable_before_the_upstream_finishes` and
  `api::rest::handlers::tests::
  a_streamed_answer_reaches_the_caller_before_the_upstream_finishes`.
- WebSocket upgrade: `infra::proxy::transport_tests::
  an_upgrade_handshake_is_tunnelled` and
  `api::rest::handlers::tests::a_websocket_upgrade_is_tunnelled_to_the_upstream`
  (raw TCP caller, masked client frame echoed back through the tunnel).
- A streamed request body reaches the upstream intact:
  `infra::proxy::transport_tests::a_streamed_request_body_reaches_the_upstream`.
- Budget: `infra::proxy::transport_tests::
  an_exchange_past_its_budget_is_a_504_request_timeout`; no retries.
- `ADR`-0002 phase order: `infra::proxy::forward_tests::
  the_guards_run_before_the_transforms_of_the_same_call` and
  `infra::proxy::forward_tests::the_upstream_chain_runs_before_the_route_chain`.
- Header rules: `infra::proxy::headers_tests` (13 cases) and
  `api::rest::error::tests::every_error_response_carries_the_gateway_source_header`.

Implementation notes the acceptance criteria forced:
- the hyper client keeps the upstream response body alive until its
  `OnUpgrade` future resolves, otherwise the h1 dispatcher closes the
  connection and the tunnel never completes (`transport::tunnel`);
- the outbound target is built as one `scheme://authority/path?query` URI,
  because `Uri::builder().path_and_query(..)` rejects an authority;
- `{*path_suffix}` arrives without its leading slash, so the proxied path is
  the one `ProxyContext` already spliced, never the gateway's own URI.
