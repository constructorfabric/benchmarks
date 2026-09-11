# oagw — Outbound API Gateway

`oagw` is the Constructor Fabric gear that lets tenant workloads reach *external*
services through a managed gateway. It owns two planes:

- a **control plane** that stores tenant-scoped upstream, route and plugin definitions, and
- a **data plane** that resolves an alias to an upstream, matches a route, runs the plugin
  chain and relays the request — including server-sent-event streams and WebSocket
  upgrades, not just plain HTTP.

The component contract lives in [`../docs/`](../docs/). `PRD.md` is the requirement set,
`DESIGN.md` the component design, and `ADR/` the decisions that shaped it.

## Layout

| Path | Contents |
|---|---|
| `src/gear.rs` | Gear registration (`#[toolkit::gear]`) and wiring into the host server. |
| `src/api/` | The `/oagw/v1/*` control-plane operations and the data-plane proxy endpoint. |
| `src/domain/` | `Upstream`, `Route` and `Plugin` entities, alias derivation and validation. |
| `src/store/` | The in-memory control-plane store and its tenant-chain scoping rules. |
| `src/proxy/` | Alias resolution, target building, header rewriting, CORS, SSE and WebSocket relaying, and request observability. |
| `src/plugins/` | The plugin chain runner and the built-in auth and guard plugins. |
| `src/ratelimit/` | The token-bucket rate limiter. |
| `src/security.rs` | `SecurityContextHolder` and the credential-resolver seam. |
| `src/error.rs` | `ErrorKind`, the canonical-error projection and the problem-document renderer. |
| `src/types.rs` | The GTS type catalog the gear publishes to the types registry. |

## Running

The gear is compiled into the example server:

```sh
cargo build --release --bin cf-gears-example-server --features "$(cat config/e2e-features.txt)"
./target/release/cf-gears-example-server --config config/e2e-local.yaml
```

The server answers `GET /healthz` and serves the gateway on port 8086 in the local
end-to-end configuration. Routes are registered gear-relative, so the API lives under
`/oagw/v1/...` — the `/api` prefix the component contract shows is added by the api-gateway
gear that fronts it.

## Control plane

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/oagw/v1/upstreams` | Create an upstream |
| `GET` | `/oagw/v1/upstreams` | List upstreams |
| `GET` | `/oagw/v1/upstreams/{id}` | Read one upstream |
| `PUT` | `/oagw/v1/upstreams/{id}` | Replace an upstream |
| `DELETE` | `/oagw/v1/upstreams/{id}` | Delete an upstream (cascades to its routes) |
| `POST` | `/oagw/v1/routes` | Create a route |
| `GET` | `/oagw/v1/routes` | List routes |
| `GET` | `/oagw/v1/routes/{id}` | Read one route |
| `PUT` | `/oagw/v1/routes/{id}` | Replace a route |
| `DELETE` | `/oagw/v1/routes/{id}` | Delete a route |
| `POST` | `/oagw/v1/plugins` | Create a custom plugin |
| `GET` | `/oagw/v1/plugins` | List plugins |
| `GET` | `/oagw/v1/plugins/{id}` | Read one plugin |
| `DELETE` | `/oagw/v1/plugins/{id}` | Delete a plugin when nothing references it |
| `GET` | `/oagw/v1/plugins/{id}/source` | Read a plugin's source |

List operations accept `$filter` (a conjunction of `field eq value` terms), `$select`,
`$orderby`, `$top` and `$skip`. A field that is absent is indistinguishable from a null
field when filtering.

## Data plane

```
{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]
```

The gateway resolves `alias` against the caller's tenant chain (closest match wins),
picks an enabled route whose method and path match, merges configuration
(upstream → route → tenant), runs the plugin chain and relays the request. A response
whose media type is `text/event-stream` is forwarded frame by frame as the upstream
produces it; an upgrade request is bridged bidirectionally to the upstream WebSocket.

Errors are RFC 9457 problem documents whose `type` carries the documented GTS error
identifier, with the OAGW extension fields (`upstream_id`, `host`, `path`,
`retry_after_seconds`) filled in when they apply.

## Aliases

An alias is derived from the endpoint set when that is possible and must be supplied
explicitly otherwise:

- a single hostname derives the hostname, suffixed with `:port` when the port is
  non-standard for the endpoint's scheme;
- a multi-endpoint pool derives the longest label-aligned common suffix, which must be a
  registrable domain — `co.uk` and a single label are not derivable;
- IP literals and non-derivable pools require an explicit alias.

Aliases are normalized to ASCII lowercase with trailing dots stripped and are unique per
tenant. The alias is immutable once set: it is the routing key in the proxy URL, so an
endpoint change that would alter it is rejected and the operator deletes and re-creates the
upstream instead.

## Tenancy

Every entity is stamped with the caller's tenant on write and resolved through the
caller's tenant chain on read. A descendant sees its ancestors' upstreams, routes and
plugins; an ancestor never sees a descendant's. Shadowing selects the routing target only —
an ancestor's enforced rate limits stay in effect.

## Tests

```sh
cargo test -p cf-gears-oagw
```

The suite covers the entity validation rules, alias derivation and normalization, the
store's tenant-scoping rules, the built-in auth and guard plugins (including the rule that
resolved credential material never reaches an error surface), the rate limiter, header
rewriting, CORS, SSE framing, WebSocket frame conversion, the metric registry and the API
DTOs.
