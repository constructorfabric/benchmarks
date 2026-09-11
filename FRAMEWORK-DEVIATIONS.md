# FRAMEWORK-DEVIATIONS.md — `/speckit-*` run for `oagw`

Recorded decisions where the implementation deliberately departed from a
document's letter while keeping its intent. Nothing here stalled the workflow:
every `/speckit-constitution` → `/speckit-specify` → `/speckit-plan` →
`/speckit-tasks` → `/speckit-analyze` → `/speckit-implement` step ran in order in
this session, and no step needed the fallback provision. This file is a
deviation ledger, not a fallback declaration.

## Contract deviations

1. **In-memory repositories.** `docs/DESIGN.md` sketches `infra/repo_pg.rs`
   (PostgreSQL). The gear ships `infra/memory_repo.rs` (`dashmap` +
   `parking_lot`) behind the same `src/domain/repo.rs` traits. The wire contract
   and the service seams are unchanged; persistence is the only deferred axis,
   and it is the one the PRD leaves to the deployment layer.

2. **`hyper-util` for the outbound leg, not Pingora.** ADR-0003 selects Pingora
   as the proxy engine; `pingora-proxy`/`pingora-core` are declared dependencies
   of the crate, but the data path dispatches through `hyper-util`'s client so
   the gear can be driven in-process (the integration harness calls the gear's
   own axum router with `tower::ServiceExt::oneshot`, and `axum::serve` carries
   the WebSocket upgrades in `tests/websocket_test.rs`). Streaming, upgrades and
   timeout semantics are implemented at this layer.

3. **`OagwError::Conflict`.** The error table in `docs/DESIGN.md` §688-712 has no
   `conflict` row, yet `PluginInUse` (409) and the alias-conflict path both need
   a 409 family distinct from `validation.error`. Added `OagwError::Conflict`
   mapping to `gts.cf.core.errors.err.v1~cf.oagw.conflict.v1` / 409.

4. **Plaintext refusal status.** An `http` endpoint is accepted by the schema and
   by the management API (see the wire-contract note that `http` is a legal
   scheme); when `allow_http_upstream` is false the *connection* is refused at
   dispatch with 503 `link.unavailable` rather than a 400 — the config flag
   governs reachability, not the shape of a stored upstream.

5. **Alias update allowance.** `docs/DESIGN.md` says an alias transition is
   operator-managed; `PUT /oagw/v1/upstreams/{id}` rejects a changed alias with
   400 unless the new value is the same name in a different case or with a
   trailing dot, which is normalised rather than rejected.

6. **Error details are scrubbed.** `docs/DESIGN.md` asks that no credential
   material reach a response body or log record; `src/domain/error.rs` scrubs
   `cred://` values and anything looking like a bearer/api-key token out of
   `detail` before the problem document is written.

7. **Flat route registration.** Besides the nested
   `POST /oagw/v1/upstreams/{id}/routes`, `POST /oagw/v1/routes` (with
   `upstream_id` in the body) is accepted, because `specs/001-oagw-gear/quickstart.md`
   smoke scenario 3 uses it.

## Model additions required by the wire format

8. **`PluginRef::Configured`.** The upstream/route payload carries plugins either
   as a bare id (`Uuid`), a bare GTS id (`String`) or a
   `{plugin_ref, config}` object; `merge::PluginBinding` carries the resolved
   `(id, config)` pair into the plugin chain.

9. **`Endpoint::authority`.** Endpoints are validated as a homogeneous pool, so
   the outbound leg can take one authority per endpoint; endpoint selection is
   round-robin with the `X-OAGW-Target-Host` override resolving *inside* the
   pool.

## Behavioural details the integration tests forced out

10. **CORS preflight route selection** (`negotiated_method`). A preflight
    (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) names no route —
    no route lists `OPTIONS` — so the route is matched by the negotiated method
    and per-request plugins are skipped, per ADR-0004.

11. **Upgrade handshake headers survive the header rules.** `build_context`
    re-applies `connection` / `upgrade` / the `sec-websocket-*` set after the
    request rules, otherwise the outbound handshake is invalid; the 101 answer
    copies back `connection`, `upgrade`, `sec-websocket-accept`,
    `sec-websocket-protocol` and `sec-websocket-extensions`.

12. **`streams_aborted` counter.** `infra/metrics.rs` gained the counter and the
    proxy handler marks a stream that ends with an error, so the aborted-stream
    condition is auditable the way FR-064 asks for.

13. **`secret_ref` accepted by the apikey plugin** alongside `credential_ref`,
    because DESIGN.md names the `cred://` reference "the secret reference".

14. **The proxy path is authenticated.** The gear initially declared
    `/oagw/v1/proxy/...` anonymous; the host's auth middleware then skips bearer
    validation entirely and inserts an anonymous `SecurityContext`, so the proxy
    handler always saw the nil tenant and could never resolve a tenant-scoped
    alias (`docs/DESIGN.md` §644: alias resolution walks the caller's tenant
    chain, which is meaningless without a caller). The proxy operations now
    declare `.authenticated()`, like the management API; a CORS preflight stays
    exempt because the authn middleware answers preflights anonymously first
    (ADR-0004).
