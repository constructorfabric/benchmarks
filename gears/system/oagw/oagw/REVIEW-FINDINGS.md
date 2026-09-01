# Review Findings Report — oagw gear (semantic review loop, slice 1)

Aggregated + deduplicated from three independent reviewer passes over
`gears/system/oagw/oagw/src` + `tests` cross-checked against
`gears/system/oagw/docs/` (PRD.md, DESIGN.md, ADR/0001..0009, schemas/).

- `Rc-*` = code-checklist semantic review
- `Rf-*` = bug-finding review
- `Rs-*` = consistency review (PRD/DESIGN/ADR/schema vs code)

**Traceability mode: DOCS-ONLY** (no FEATURE artifact, no `@cpt-*` markers).
Design sources: `gears/system/oagw/docs/`.

| ID | Sev | Theme | Sources | Location |
|---|---|---|---|---|
| F-001 | CRITICAL | SSRF guard IPv6 bypass (IPv4-mapped, ULA, link-local) | Rf-001, Rc-016 | `src/infra/proxy/service.rs:163-188` |
| F-002 | CRITICAL | CORS contract unimplemented (no preflight, no ACAO, no wildcard+credentials validation) | Rf-002, Rc-018, Rs-005, Rs-006 | `src/domain/cors.rs:11-87`, `src/infra/proxy/service.rs:227-320` |
| F-003 | MAJOR | `effective_auth`: ancestor `inherit` beats descendant own auth | Rc-003, Rs-011 | `src/domain/merge.rs:31-50` |
| F-004 | MAJOR | `effective_plugins`: `inherit` ancestor chains skipped | Rc-004, Rs-012 | `src/domain/merge.rs:79-102` |
| F-005 | MAJOR | `effective_cors`: `inherit` origins ignored; `enforce` widened by descendant | Rc-005, Rf-005 | `src/domain/merge.rs:110-163` |
| F-006 | MAJOR | `effective_limit`: `inherit` ancestor limits dropped entirely | Rc-007 | `src/domain/ratelimit.rs:27-38` |
| F-007 | MAJOR | `effective_headers`: keyed on `auth.sharing`; replaces instead of composing | Rf-004, Rc-027 | `src/domain/merge.rs:58-73` |
| F-008 | MAJOR | Disabled upstream → 404 not 503; ancestor-disable not inherited; disabled ancestor still contributes enforce config | Rc-010, Rc-013, Rf-008 | `src/infra/controlplane/mod.rs:127-137, 686-693, 674-678` |
| F-009 | MAJOR | `tenant_chain` swallows resolver errors → ancestor enforce constraints silently dropped | Rc-009 | `src/infra/controlplane/mod.rs:105-122` |
| F-010 | MAJOR | `Route::match_key` omits methods → method-split routes get 409 | Rc-006, Rf-007, Rs-030 | `src/domain/model.rs:544-552` |
| F-011 | MAJOR | Rate-limit scope key omits resource id + tenant; capacity `max()`; unbounded bucket growth | Rf-015, Rf-016, Rc-022, Rf-027 | `src/infra/ratelimit.rs:135-199` |
| F-012 | MAJOR | `client_ip` trusts client-supplied `Forwarded`/`X-Forwarded-For` | Rf-014, Rc-021 | `src/infra/proxy/service.rs:860-870` |
| F-013 | MAJOR | WS upgrade path bypasses CORS/payload/breaker/audit/query; forwards `Authorization`; drops subprotocol/query | Rf-011, Rf-012, Rc-019, Rc-020 | `src/infra/proxy/service.rs:691-790` |
| F-014 | MAJOR | TLS + HTTP handshake outside any timeout; hardcoded connect timeout | Rc-015 | `src/infra/proxy/client.rs:174-192` |
| F-015 | MAJOR | Problem responses served as `application/json`, not `application/problem+json` | Rf-003, Rc-024, Rs-002 | `src/api/rest/error.rs:55-77` |
| F-016 | MAJOR | Problem body omits `instance` + documented extension fields | Rc-023, Rs-003 | `src/api/rest/error.rs:55-93` |
| F-017 | MAJOR | No permission checks; `authz_resolver`/`types_registry` declared but never resolved | Rc-008, Rs-009, Rs-008 | `src/gear.rs:34-38` |
| F-018 | MAJOR | Alias bind to ancestor alias: no bind permission, `enforce` does not block override | Rc-012 | `src/infra/controlplane/mod.rs:419-421` |
| F-019 | MAJOR | OData `$skip`/`$orderby`/`$select` ignored; `list_plugins` ignores all params | Rf-019, Rc-011, Rs-010 | `src/infra/controlplane/mod.rs:307-322`, `src/api/rest/handlers.rs:211-223` |
| F-020 | MAJOR | Data plane wired ad hoc; undocumented `/ws` + `/events`; 413 undeclared | Rc-002, Rs-032 | `src/api/rest/routes.rs:301-330` |
| F-021 | MAJOR | Malformed-body/query/path rejections bypass the problem shape; 422 undeclared | Rc-001 | `src/api/rest/handlers.rs:44-221` |
| F-022 | MAJOR | Correlation id minted fresh at every call site (audit / transform / plugin ctx) | Rf-021, Rf-025, Rc-014 | `src/domain/plugin/mod.rs:42-53` |
| F-023 | MAJOR | `X-RateLimit-*` headers never emitted despite `response_headers: true` default | Rf-006, Rs-013 | `src/domain/model.rs:276-278`, `src/infra/proxy/service.rs:243-272` |
| F-024 | MAJOR | OAuth2 token cached forever when `expires_in <= margin` | Rf-010 | `src/infra/plugin/oauth2_cc_auth.rs:163-176` |
| F-025 | MAJOR | apikey query param not percent-encoded; duplicate param appended | Rf-009 | `src/infra/plugin/apikey_auth.rs:76-88` |
| F-026 | MAJOR | Four designed metrics never emitted; `selection_method` hardcoded | Rf-020, Rc-031, Rs-014 | `src/infra/metrics.rs:142-223`, `src/infra/proxy/service.rs:570` |
| F-027 | MAJOR | Audit `path` logs the route match key, not the request path | Rs-033 | `src/infra/proxy/service.rs:885-892` |
| F-028 | MAJOR | Chunked bodies bypass the 100 MB hard limit | Rc-017 | `src/infra/proxy/service.rs:210-216` |
| F-029 | MAJOR | IPv6 endpoints fail every request (unbracketed URI authority) | Rf-013, Rc-032 | `src/infra/proxy/service.rs:821-858` |
| F-030 | MAJOR | Route uniqueness check-then-act race (no check in `RouteRepository::insert`) | Rf-018 | `src/infra/storage/mod.rs:190-193` |
| F-031 | MAJOR | Hop-by-hop: `Connection` tokens not parsed, response side not stripped, repeated headers dropped | Rf-022, Rf-023, Rf-024 | `src/infra/proxy/service.rs:28-40, 290-312, 652-675` |
| F-032 | MAJOR | `OagwConfig` rejects ADR-0008 `token_cache_*` keys (`deny_unknown_fields`) | Rs-004 | `src/config.rs:30-41` |
| F-033 | MAJOR | `/metrics` not exposed; metrics only exported via OTLP push | Rs-018 | `src/api/rest/routes.rs`, PRD §6.1 |
| F-034 | MAJOR | Persistence architecture deviation (DESIGN §3.4/§3.6 + ADR-0006 vs in-memory store) | Rs-001 | `src/infra/storage/mod.rs:1-7` |
| F-035 | MAJOR | Published JSON schemas drift from the API (route `priority/enabled/cors/tenant_id`; upstream `tenant_id`; plugin bindings as objects) | Rs-015, Rs-016, Rs-017 | `docs/schemas/*.schema.json` |
| F-036 | MAJOR | No create-time plugin-reference validation (catalog-only / unresolvable refs accepted → runtime 503) | Rc-030 | `src/infra/controlplane/mod.rs:139-167` |
| F-037 | MAJOR | Target-host validation: single-label names rejected; port-bearing value yields wrong error type | Rf-017, Rc-029 | `src/domain/routing.rs:192-225` |
| F-038 | MINOR | `delete_upstream` OpenAPI promises cascade, code returns 409 | Rc-025 | `src/api/rest/routes.rs:106-121` |
| F-039 | MINOR | Plugin ops declare 404 but can only return 503; management 404s reuse `RouteNotFound` | Rc-026, Rs-031 | `src/api/rest/routes.rs:244-294` |
| F-040 | MINOR | Timeout errors carry no `Retry-After` though marked retriable | Rs-028 | `src/domain/error.rs:267-278` |
| F-041 | MINOR | 409 `already_exists` absent from the published error tables | Rs-027 | `src/domain/error.rs:8-13` |
| F-042 | MINOR | Doc drift: DESIGN names non-existent symbols/files; ADR-0008/0009 cite wrong paths + missing from ADR coverage; ADR-0002 example contradicts catalog-only `basic`; `REQUIRED_HEADER_MISSING` never emitted; `gc_eligible_at` never set; `phases` field absent | Rs-019..Rs-026, Rs-029, Rs-031(phases) | `docs/` |
| F-043 | MINOR | `now_ms` uses wall clock; doc claims monotonic; `u32` truncation | Rf-026, Rc-033 | `src/infra/ratelimit.rs:108-121` |
| F-044 | MINOR | No half-open probe gate on the circuit breaker | Rf-028 | `src/infra/ratelimit.rs:279-301` |
| F-045 | MINOR | Test coverage gaps: SSRF, CORS wiring, auth/plugins/CORS merge matrix | Rc-034 | `tests/proxy_http_tests.rs:20-26` |
| F-046 | MINOR | Compile-fail gate not applicable (no compile-time guarantee, no harness) | Rc-035 | `Cargo.toml` |
| F-047 | MINOR | `match_grpc` returns `candidates[0]` instead of the matched route | Rc-028 | `src/domain/routing.rs:116-134` |
| F-048 | MINOR | `allow_credentials` + wildcard origin accepted by the Rust validator (schema rejects it) | Rs-006 | `src/domain/cors.rs:24-36` |
| F-049 | MAJOR | Upstream request target sent in absolute form (`GET http://host/path`), not origin form | verification (live round trip) | `src/infra/proxy/client.rs:320-345` |

**Review-loop decisions**
- `REVIEW_GRANULARITY` = per-methodology (3 reviewers in parallel).
- Aggregate/dedupe result: 98 raw findings → 48 unique findings (F-001..F-048).
- Fix scope: **all findings**.
- F-049 was raised *after* the three-way review, by the live verification round
  trip (`release` binary + `config/e2e-local.yaml`): a `text/event-stream`
  upstream stopped parsing its own request path because the gateway handed it
  `GET http://host:port/sse HTTP/1.1`. Root cause: `hyper` writes absolute-form
  whenever the request `Uri` carries an authority. Fixed by reducing the target
  to origin form once the dial target has been read (`origin_form`,
  `src/infra/proxy/client.rs`), pinned by
  `tests/dataplane_tests.rs::the_request_target_is_sent_in_origin_form` against a
  raw socket.
