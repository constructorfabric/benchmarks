# oagw implementation slice plan

Workflow: `cf-gears-coding` (DESIGN-led CODE preset, ARTIFACT_KIND=CODE).

`source_design_context` = `gears/system/oagw/docs/` — `PRD.md`, `DESIGN.md`,
`ADR/0001..0009`, `schemas/upstream.v1.schema.json`, `schemas/route.v1.schema.json`.

`phase-plan`, `phase-dod`, `acceptance-criteria`, `relevant-files-map` are satisfied
by the operator-approved override naming the accepted specification documents as the
design contract and acceptance criteria (PRD §9, DESIGN §3.3 error/API tables,
DESIGN §3.2/§3.6 constraints, ADR confirmation criteria).

## Slices

| # | Slice | Design element implemented | Key files |
|---|---|---|---|
| 1 | Foundation: crate tree, config, error model, domain model, alias/hostname rules | DESIGN §3.2 crate layout, §3.1 domain model, `cpt-cf-oagw-interface-api` error table, alias enforcement + normalization + hostname validation, ADR-0008 config keys | `lib.rs`, `config.rs`, `api/error.rs`, `domain/model.rs`, `domain/alias.rs`, `domain/error.rs`, `domain/validation.rs` |
| 2 | Control plane: repositories + management service | DESIGN §3.2 ControlPlaneService, §3.3 CRUD semantics (tenant scoping, alias uniqueness/bind, route match uniqueness, immutable fields, PluginInUse 409), §3.6 invariants, ADR-0001 plugin deletion | `domain/repo.rs`, `infra/storage/*.rs`, `domain/services/management.rs`, `domain/merge.rs` |
| 3 | Plugin system | ADR-0002 (traits, execution order, built-ins, registries), ADR-0008 (OAuth2 CC + token cache), ADR-0009 (required headers guard), request_id transform | `domain/plugin/*.rs`, `infra/plugin/*.rs` |
| 4 | REST transport (management API) | DESIGN §3.3 management endpoints, OData list params, problem+json projection with OAGW GTS types, authn/authz, ADR-0001 PluginInUse body | `api/rest/routes.rs`, `api/rest/handlers/*.rs`, `api/rest/dto.rs` |
| 5 | Data plane core | DESIGN §3.2 hierarchical config/alias resolution/shadowing, guard rules, §3.5 proxy flow, ADR-0003 rate limiting, ADR-0004 CORS, ADR-0007 error source, ADR-0005/0006 caching + state | `domain/services/proxy.rs`, `infra/ratelimit.rs`, `infra/cors.rs`, `infra/cache.rs` |
| 6 | Proxy transport (HTTP + SSE + WebSocket) | DESIGN §3.2 header transformation tables, target-host matrix (ADR-0001), body validation rules, streaming (SSE + WS), SSRF/scheme policy, pingora connector for TLS | `infra/proxy/*.rs`, `infra/transport.rs` |
| 7 | Gear wiring + proxy REST + observability | DESIGN §3.2 gear structure/registration, §4.2 metrics, §4.3 audit logging, GTS type provisioning, proxy endpoint registration | `gear.rs`, `infra/metrics.rs`, `infra/audit.rs`, `infra/type_provisioning.rs`, `api/rest/handlers/proxy.rs` |

Per slice: failing tests first, smallest passing implementation, deterministic
validation (`cargo check/clippy/test -p cf-gears-oagw --offline`), then the semantic
review loop. Security-boundary slices (5, 6) record review evidence explicitly.

## Slice status

| # | Slice | Deterministic validation | Semantic review | Tests |
|---|---|---|---|---|
| 1 | Foundation | PASS | PASS | 38 unit |
| 2 | Control plane | PASS | PASS | 32 unit |
| 3 | Plugin system | PASS | PASS | 20 unit |
| 4 | Management REST | PASS | PASS | 11 unit |
| 5 | Data plane core | PASS | PASS + evidence (`docs/reviews/slice-5-data-plane.md`) | 32 unit |
| 6 | Proxy transport | PASS | PASS + evidence (`docs/reviews/slice-6-transport.md`) | 32 unit |
| 7 | Gear wiring + proxy REST | PASS | PASS + evidence (`docs/reviews/slice-7-wiring.md`) | 20 unit + 8 `tests/proxy_plane.rs` |

Full suite: `cargo test -p cf-gears-oagw --offline` → 185 unit tests + 8 integration
tests in `tests/proxy_plane.rs`, 0 failed; `cargo clippy -p cf-gears-oagw --offline
--all-targets` → clean. Deployment check: the release server on `config/e2e-local.yaml`
answers the 32-check smoke suite (`passed=32 failed=0`) covering management CRUD,
auth 401s, validation 400s, proxy HTTP/SSE, error attribution and CORS.

## Override record (prerequisite gates bypassed, operator-approved)

`cf-coding-gen` reported `blocked` because this cohort runs without the planning
artifacts it normally requires. The operator-approved override named the accepted
specification as the contract instead of running producer skills, and the run
therefore finishes `completed-with-assumptions`. Each bypassed gate, why it was
acceptable here, and its residual risk:

| Missing artifact / gate | Working assumption | Risk |
|---|---|---|
| `phase-plan` | `gears/system/oagw/docs/` (PRD, DESIGN §3.2/§3.3/§3.6, ADR 0001–0009, JSON Schemas) is the approved scope; `SLICE-PLAN.md` derives the slice order from it. | Slice ordering is reviewer-derived rather than planner-approved; mitigated by per-slice deterministic validation and review. |
| `phase-dod` | Acceptance criteria = PRD §9 plus DESIGN §3.3 error/API tables and §3.2/§3.6 constraints. | Completion is checked against the spec, not a separately signed-off DoD. |
| `acceptance-criteria` | Same source as above; the 32-check deployment smoke suite plus 193 automated tests are the executable form. | Behaviour the spec states only in prose (e.g. audit field wording) is verified by review rather than a test. |
| `relevant-files-map` | Files follow DESIGN §3.2's crate layout, so the touched surface is the `oagw` crate plus the workspace manifest/config sections that already existed. | No formal path whitelist; drift outside the crate is reviewed in the diffs. |
| Artifact registry gates (`cfs validate --artifact`) | `/app/.cf-studio/config/artifacts.toml` registers no artifacts, so the CLI validator has nothing to cross-reference; the studio gate is recorded as `SKIPPED` for registry-scoped checks while `check-language` and the project's cargo gates run. | Studio cross-reference/traceability validation is not exercised; the spec-fidelity check is done by the semantic review loop instead. |

`ARTIFACT_KIND = CODE`: no FEATURE artifact is authored and no `@cpt-*` markers are
added, per the DESIGN-led preset.

## Findings fixed during the review loop

- No-route requests dialled the upstream at `/`: `resolve()` and `outbound_path()`
  dropped the request path when no route matched. Both now preserve it, so
  `/oagw/v1/proxy/{alias}/v1/chat` reaches the upstream at `/v1/chat` without a
  route (DESIGN §3.6 routing is an overlay, not a requirement).
- WebSocket dials used the wire scheme (`ws://`), which reqwest rejects; the
  transport URL now maps `ws`→`http` and `wss`→`https` while the domain vocabulary
  stays unchanged.
- CORS preflights read no `Origin` and always answered 403; the preflight path now
  takes the caller's `Origin` header (ADR-0004).
- `infra/credentials.rs` passed the `cred://`-prefixed reference straight to
  `SecretRef::new`, which rejects `:`/`/`, so every credstore lookup failed; the
  scheme is stripped before the handle is minted.
- `x-oagw-error-source` was built with `HeaderName::from_static("X-OAGW-…")`, which
  panics on uppercase; the constant carries the lowercase wire spelling.

A second review round over slices 5–7 (security and cross-plane seams) fixed:

- `scope: ip` charged a client-supplied header; the identity is now the socket peer
  (`PeerAddr`) and a forwarded header never wins.
- A missing `SecurityContext` degraded to the nil tenant; `identity()` returns 401.
- The query string was glued onto the routing path; it is now separated end to end,
  filtered against the route's `query_allowlist`, and re-joined only when dialling.
- The rate-limit table grew without bound and kept stale shapes; buckets are capped
  at `MAX_BUCKETS` and restart full when the charged config changes.
- `User`/`Route` counters collapsed across tenants; both carry the tenant.
- Endpoint selection never rotated; a multi-endpoint upstream rotates per ADR-0001
  and a common-suffix alias requires `X-OAGW-Target-Host`.
- Gateway control headers crossed the hop; all five are stripped on the request leg
  and re-stamped on the response, and a relayed 4xx/5xx carries
  `X-OAGW-Error-Source: upstream`.
- `..`/percent-encoded path segments escaped the proxy root; `is_canonical_path()`
  rejects them at resolve time.
- Response buffering was unbounded; it is capped by `max_body_size_bytes` (413).
- The outbound client installed a client-wide deadline that severed SSE streams;
  the header budget now wraps `send()` only.
- The WebSocket tunnel forwarded a compression extension it cannot honour and
  skipped the request-transform phase; both are fixed.
- CORS `Vary` clobbered an upstream value; tokens are merged per-token.
- Metrics/audit call sites (`record_error`, `record_request`, `enter`, `auth_failure`,
  `config_change`) were computed-and-dropped or missing; all now fire, with the
  documented label sets and a bounded in-flight gauge.

## Recorded deviations (shared-baseline policy)

- **Storage**: DESIGN §3.6 specifies SeaORM/`toolkit-db` persistence. The crate
  manifest has no `toolkit-db`/`sea-orm` dependency and the task forbids adding
  dependencies outside the lockfile, so repositories are implemented in-memory
  behind the `domain/repo.rs` trait boundary. Persistence is a follow-up slice;
  the API surface does not change.
- **Proxy engine**: DESIGN §3.2 names a "Pingora in-memory bridge". pingora-proxy's
  session model cannot be embedded behind an axum handler without a separate server
  bootstrap; the outbound path uses `reqwest` request/response sessions with
  streaming (SSE/WebSocket) preserved. The manifest's `pingora-core`/`pingora-proxy`
  dependencies remain unused and should be dropped in a dependency-hygiene pass.
- **Tenant hierarchy**: DESIGN §3.5 resolves budget inheritance through the platform
  tenant tree. No platform API exposes ancestors, so `infra/hierarchy.rs` ships
  `FlatTenantHierarchy` (a tenant is its own chain) behind the
  `domain::services::proxy::TenantHierarchy` trait; a real source is a drop-in
  replacement for the type parameter.
- **TLS HTTP/2**: adaptive per-host HTTP/2 detection (DESIGN §4.4) is limited to
  ALPN-negotiated HTTP/2 for TLS upstreams; plain-HTTP upstreams use HTTP/1.1.
- **GTS error types**: the platform `CanonicalError` carries fixed per-category GTS
  types, so the OAGW problem+json projection (`api/error.rs`) renders the OAGW
  identifiers from DESIGN §3.3 directly.
