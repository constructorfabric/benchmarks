# Implementation Notes

Deviations and platform limitations observed while implementing the `oagw` gear
against `gears/system/oagw/docs/`. The documents remain the contract; everything
below is a place where the contract could not be met as written, with the reason
and the fail-safe behaviour that stands in.

## Forced deviations

| Area | Contract | Actual | Reason |
|---|---|---|---|
| WebTransport upstreams | `wt://` scheme in `upstream.v1` | Configured, but every proxied request answers `503 LinkUnavailable` | No WebTransport/QUIC client is in the workspace lockfile and the build is offline; failing closed keeps an unconfigured transport from silently degrading to HTTP |
| WebSocket over TLS (`wss://`) | `wss://` endpoints should upgrade | `wss://` upstreams answer `502 LinkUnavailable` at connect time | The locked `tokio-tungstenite` has no TLS feature enabled; `ws://` upgrades work end to end |
| `ConnectionTimeout` / `IdleTimeout` error types | Distinct `504` types | Both surface as `504 RequestTimeout` | The toolkit transport exposes one per-request timeout and no separate connect/idle budget, so the distinction is not observable |
| `connect_timeout_secs` knob | TCP connect timeout for the upstream leg | Accepted in configuration, not applied | `toolkit-http::HttpClientConfig` has no connect-timeout field; only the request timeout can be set |

## Deliberate deferrals (documented, not missing)

- **Config caching (ADR-0005, DESIGN §4.1)**: explicitly deferred by the design to
  a future consideration. The `hot_cache_capacity` knob is accepted and retained
  for that layer, but no L1 cache is built — the control plane is already
  in-memory at this milestone, so a cache would front nothing.
- **Upstream-health and connection-pool gauges** (DESIGN §4.2): not instrumented;
  the toolkit transport does not expose pool counters. Breaker state, transitions,
  requests, durations, errors, rate-limit rejections and routing selections are.
- **Fine-grained authorization** (DESIGN "Authentication & Authorization" table):
  the platform's gear API exposes only authenticated/anonymous/public gates and no
  per-permission registration, so all seventeen oagw operations are registered
  `.authenticated()`. The permission strings in the design are not enforceable
  from a gear with the current `toolkit-security` surface.

## Security posture worth restating

- Credentials, secrets, request/response bodies, query strings and header values
  never reach logs, audit events, metrics labels or problem documents. The audit
  event field set is closed (see `src/infra/audit.rs`) and the error table's
  `detail` strings are the only user-controlled text echoed back.
- Upstream URLs are HTTPS-only unless `allow_http_upstream` is explicitly `true`;
  the SSRF guard refuses private, loopback, link-local and unique-local targets
  unless `ssrf_policy.allow_private_addresses` is `true`.
- Rate-limit counters are keyed so that a missing client identity falls back to
  the tenant rather than collapsing into one global bucket.
- The apikey plugin's query delivery mode appends the credential *after* the
  route's query allowlist filter; caller parameters remain subject to it.

## Testing

Automated tests live in this crate (`tests/`, plus unit tests beside the code)
and cover the management CRUD surface, alias resolution and shadowing, config
merge semantics, the plugin chain order and rejection statuses, rate limiting,
circuit breaking, CORS preflight, SSRF and protocol policy, plain HTTP proxying,
server-sent-event streaming and WebSocket upgrades.
