# Code Review Findings Report — `oagw` gear

**Scope:** `gears/system/oagw/oagw` (implementation) and `gears/system/oagw/docs/` (FEATURE/DECOMPOSITION artifacts).
**Method:** 14 independent reviewer slices (7 code-checklist categories, 4 bug-finding layer slices, 3 cross-document consistency slices), each emitting `ReviewFindingContract` records.
**Raw findings:** 151. **After deduplication:** 128.

Finding IDs below are stable: `<namespace>-<NNN>` where namespace is `RVW` for the
merged report; the originating reviewer's ID is carried in `Source` so every record
stays traceable to the slice that emitted it.

---

## A. CRITICAL

### RVW-001 — Credential boundary is a stale denylist, not an allowlist
**Severity:** CRITICAL · **Source:** sec-1 · **Confidence:** high
**Disposition:** FIXED — `SECRET_KEYS` extended to 9 spellings (`client_secret_ref`, `api_key`, `password`, `password_ref`, `private_key` added; the `*_ref`-suffix rule dropped because `client_id_ref` is an identifier, not a secret); `looks_like_secret` extended to 10 prefixes plus JWT (`eyJ…`+2 dots) and 32+-hex shapes; the header name is now passed to `reject_header_value` explicitly so a name containing a dot is still matched; `set-cookie` and `www-authenticate` added to `CREDENTIAL_BEARING_HEADERS`. Covered by `a_literal_in_a_reference_shaped_oauth2_key_is_rejected` and `a_credential_bearing_header_name_containing_a_dot_is_still_matched` in `src/config_tests.rs`.
**Location:** `src/domain/credential.rs:117-135` (and `:76`, `:97-98`, `:143-148`, `:156-159`)

**Evidence:** `const SECRET_KEYS: [&str; 5] = ["api_key_ref", "password_ref", "secret_ref", "client_secret", "credential_ref"];` / `const PREFIXES: [&str; 5] = ["sk_", "sk-", "Bearer ", "basic ", "xox"];` / `let name = field.rsplit('.').next().unwrap_or(&field);`

**Root cause:** The credential boundary is a denylist of literal spellings, not an exhaustive allowlist, and the denylist is stale against the plugins it protects: `oauth2_client_cred_auth.rs:193` reads the credential keys `client_id_ref` / `client_secret_ref`, neither of which is in `SECRET_KEYS` (`client_secret_ref` != `client_secret`), and the free-form keys a custom `auth.config` may use (`token`, `password`, `api_key`, `private_key`) are absent too. The value-shape fallback `looks_like_secret` only catches 5 prefixes and is case-sensitive (`basic ` but not `Basic `), and no JWT/hex/`ghp_`/`AKIA` shapes are covered. On the header side the name is recovered as the last dot-segment of the field path, so a configured header name containing a dot is compared as `key` and never matched against `CREDENTIAL_BEARING_HEADERS`, which also omits `set-cookie`.

**Impact:** A literal client secret (`auth.config = {"client_secret_ref": "a1b2c3..."}`) is accepted at the boundary, persisted in the upstream record, and echoed back by the management read path (`UpstreamResponse.auth`, `dto.rs:236`), violating the documented invariant `inst-gf-cred-1/-2/-5`. The secret is then long-lived in control-plane storage and in any control-plane cache/audit copy.

**Fix:** Invert the check for `auth.config`: reject any string value under `auth.config.*` that is not `is_cred_reference(...)`, instead of guessing which keys are secret-bearing (keep the positive `SECRET_KEYS`/`*_ref` list as a fast path). Derive the header name from the field path with a separator that cannot appear in a header name, or pass the name explicitly to `reject_header_value`, and add `set-cookie` plus other auth spellings to `CREDENTIAL_BEARING_HEADERS`. Add a test for `client_secret_ref` / `client_id_ref` literals.

**Verification:** `POST` an upstream whose `auth.config` is `{"client_secret_ref":"hex-secret-value","client_id_ref":"x"}` — it is stored and returned verbatim by `GET /oagw/v1/upstreams/{id}`; the same payload with `client_secret` (the tested spelling) is rejected.

---

### RVW-002 — The proxy response body is buffered without any size cap
**Severity:** CRITICAL · **Sources:** perf-1, bfl12-3, sec-4 · **Confidence:** high
**Disposition:** FIXED — `stream::collect` now takes `max_bytes` and returns a `DownstreamError` the moment `buffer.len() + data.len()` would exceed it; `render` derives `max_bytes` from `DataPlaneLimits::max_body_size_bytes` (already clamped to `MAX_BODY_SIZE_CEILING` by `config.rs:134`) instead of relying on the shared client's `usize::MAX`. A 4 MB response against a 1 MB limit returns `502 gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1` (non-retriable, so failover does not retry it). Covered by `a_response_larger_than_the_body_limit_is_a_downstream_error`.
**Location:** `src/infra/proxy/service.rs:1470,1493-1495` with `src/infra/proxy/stream.rs:201-217`

**Evidence:** `let body = response.into_limited_body();` … `let bytes = stream::collect(body, context.trace_id.clone()).await` — `stream::collect` accumulates `buffer.extend_from_slice(data)` with no cap, and the limit comes from `HttpClientConfig::proxy()` (`toolkit-http/src/config.rs:903`): `max_body_size: usize::MAX, // no cap: stream large downloads untruncated`. `DataPlaneLimits::max_body_size_bytes` bounds only the inbound request (`proxy_handlers.rs:549`).

**Root cause:** the non-SSE response path buffers the entire upstream body in one `BytesMut` and inherits the shared client's `usize::MAX` response limit instead of the gear's body ceiling; no response-size limit exists anywhere on this path.

**Impact:** any proxied response of arbitrary size is fully resident in memory per request. A tenant-controlled or compromised upstream (or simply a large legitimate download) drives the data plane into unbounded RSS growth and OOM with only a handful of concurrent requests (CWE-770). The DoD `cpt-cf-oagw-dod-request-proxy-body-validation` states the limit rationale as "so no oversized body is ever read into memory"; the response leg has no such bound.

**Fix:** give the proxy response leg its own cap — either a distinct `HttpClientConfig` for the buffered leg with a bounded `max_body_size`, or enforce a dedicated `max_response_size_bytes` (defaulting to the `MAX_BODY_SIZE_CEILING`) inside `stream::collect`, mapping the breach to a `502 downstream.error.v1`.

**Verification:** point an upstream at an endpoint returning several GB and observe RSS growth with no 413/502 until OOM; after the fix the exchange fails at the configured limit.

---

### RVW-003 — `Principal::default()` on the plugin path collapses the OAuth2 token cache across tenants
**Severity:** CRITICAL · **Source:** bfl12-1 · **Confidence:** high
**Disposition:** FIXED — `PluginRuntime::run_request` takes a `Principal` and hands it to the auth phase verbatim; `DataPlaneServiceImpl::run_plugin_request` builds it from `ProxyContext::{tenant_id, principal_id}`, so the OAuth2 cache key is `{tenant}:{subject}:{method}:{config_hash}` with both identity components populated. `scopes` stays empty: the proxy pipeline performs no scope resolution (the platform AuthZ resolver sits outside the gear), and no shipped auth plugin reads `principal.scopes`. Covered by `the_auth_phase_receives_the_caller_identity` in `src/infra/plugin/executor_tests.rs`; the key-level separation is already pinned by `the_cache_key_separates_tenants_and_subjects`.
**Location:** `src/infra/plugin/executor.rs:441`

**Evidence:** `principal: crate::domain::plugin::Principal::default(),` — the only `Principal` construction in the crate, so `subject_id`/`tenant_id` are always `None` and `scopes` always empty.

**Root cause:** `PluginRuntime::run_request` builds the `AuthContext` from nothing; no caller identity, tenant or scope from `ProxyContext`/`Actor` is threaded into the plugin executor, while `inst-ps-key-2` requires the tenant component of the identity to reach the auth phase.

**Impact:** (1) `oauth2_client_cred_auth.rs:148-161` builds its token cache key as `format!("{tenant}:{subject}:{method}:{config_hash}")` — with both components always empty the key degenerates to `":<method>:<config_hash>"`, so two tenants whose OAuth2 configurations hash identically share one cached access token and tenant B's upstream request is authenticated with tenant A's client credential. (2) `security_context_for(ctx.principal.subject_id, ctx.principal.tenant_id, …)` yields `SecurityContext::anonymous()`, so `cred_store` is asked to decide accessibility of a `cred://` reference for the nil tenant instead of the requesting tenant.

**Fix:** construct the `Principal` in `run_request` from the proxy actor (`subject_id`, `tenant_id`, resolved scopes) and pass it through `PluginRuntime`/`run_request` instead of `Principal::default()`.

**Verification:** bind the same OAuth2 `config` to two different tenants: mint a token for tenant A, issue a proxied request for tenant B, and observe B's upstream receives A's cached token.

---

### RVW-004 — `route-management.md` states both "$top clamps" and "$top rejects"
**Severity:** CRITICAL · **Source:** cons2-1 · **Confidence:** high
**Location:** `docs/features/route-management.md:329` and `:472`

**Evidence:** `:329` — "Apply `$top` with a default of 50 and a maximum of 100, clamping a larger value instead of serving it - `inst-rm-lq-4`"; `:472` — "unit tests for `$top` clamping". The same doc contradicts itself at `:158`, `:463` and `:554` ("An unsupported, malformed, or out-of-range parameter **MUST** be rejected with a validation error naming the parameter"), and `upstream-management.md:429` states reject. The implementation (`src/domain/list_query.rs:516-518`) rejects, and `tests/route_list_query.rs:130` asserts rejection.

**Root cause:** the `inst-rm-lq-4` step and its Tests line were written against an earlier clamp semantics and not updated when §6 and the shared implementation moved to reject.

**Impact:** two MUST-level requirements inside one FEATURE doc are incompatible; an implementer following `inst-rm-lq-4` ships clamping and then fails their own acceptance criterion and their own referenced test.

**Fix:** change `:329` to "…rejecting a value above the maximum or below 1 rather than silently clamping it" (matching `upstream-management.md:429`) and `:472` to "unit tests for `$top` rejecting out-of-range values".

**Verification:** `grep -n "clamping" docs/features/route-management.md` returns nothing; `cargo test -p cf-gears-oagw list_query` passes.

---

### RVW-005 — `cors.md` and `request-proxy.md` disagree on the proxy-path stage order
**Severity:** CRITICAL · **Source:** cons1-1 · **Confidence:** high
**Location:** `docs/features/cors.md:57,120,123,124` vs `docs/features/request-proxy.md:140,148`

**Evidence:** `cors.md:57` states the canonical order as "**alias resolution → tenant resolution → route match → preflight detection (short-circuit, answers locally) → actual-request CORS check → plugin chain → upstream dispatch**" and declares it "stated once here and referenced from both flows of §2"; `request-proxy.md:140` says the preflight "is answered at handler level with a permissive `204` and never reaches alias resolution, route matching, or an upstream"; `request-proxy.md:148` (`inst-rp-dispatch-6`) repeats "no upstream resolution, no tenant context, and no plugin-chain execution".

**Root cause:** the proxy-path stage order is one canonical concept, but `cors.md` claims canonical ownership while the entry that owns the proxy path restates it without linking and with the opposite placement of the preflight-detection stage; no precedence is recorded.

**Impact:** an implementer cannot determine whether alias resolution, tenant resolution and route matching run before the preflight `204`. The two readings are observably different: under `cors.md`'s order an `OPTIONS` aimed at an unknown alias fails before the short-circuit, while `request-proxy.md`'s dispatch flow returns `204` before alias normalization. `cors.md`'s own requirement that the preflight echo the *effective* `allow_credentials` and the *matched* verdict is unimplementable under `request-proxy.md`'s order.

**Fix:** make `cpt-cf-oagw-feature-request-proxy` the canonical owner of the proxy-path stage order, replace `cors.md:57`'s "stated once here" with a pointer to it, and correct `request-proxy.md:140` and `inst-rp-dispatch-6` so the short-circuit sits after alias/tenant/route resolution.

---

## B. MAJOR

### RVW-006 — Plugin bindings are looked up under the caller's tenant instead of the owning level
**Disposition:** FIXED — `plugin_chain` now reads the selected upstream's bindings under `walk.levels[walk.selected].tenant_id` and the route's bindings under the tenant of the level whose `tenant_distance` the matcher recorded, and both repository failures are propagated instead of read as an empty binding set (the now-unused `actor` parameter was dropped). Covered by `an_ancestor_owned_selected_upstream_keeps_its_bindings` in `tests/plugin_chain_ordering.rs`.
**Location:** `src/infra/proxy/service.rs:1008-1018`

**Evidence:** `let upstream = self.upstreams.get(actor.tenant_id, selected.upstream.id).map(|stored| stored.plugin_bindings).unwrap_or_default();` / `let route_bindings = self.routes.get(actor.tenant_id, route.id)…unwrap_or_default();`

**Root cause:** the selected alias level and the matched route may be owned by an *ancestor* level (the alias walk selects the first level holding the alias, and `select_route` matches across all levels), but these two reads are keyed by the requesting tenant. `UpstreamRepository::get`/`RouteRepository::get` are strictly caller-tenant scoped and the not-found outcome is swallowed by `unwrap_or_default()`, unlike the ancestor loop four lines above, which correctly uses `level.tenant_id`.

**Impact:** for any request where the selected upstream or the matched route is inherited from an ancestor, that level's guard and transform bindings silently drop out of the composed chain — a configured guard such as `required_headers` stops being enforced for inherited upstreams, and the failure is invisible because `compose` never sees the bindings and no error is raised.

**Fix:** read the selected level's bindings with the level's own tenant, track the tenant of the level that produced `route` for the routes read, and propagate a repository failure instead of defaulting to an empty binding list.

---

### RVW-007 — The buffered body read is outside the `proxy_timeout_secs` budget
**Severity:** MAJOR · **Sources:** bfl12-2, perf-2, bfl3-4 · **Confidence:** high
**Disposition:** FIXED — `exchange` now passes the remaining `budget` into `render`, which wraps `stream::collect` in `tokio::time::timeout(budget, …)`, mapping expiry to `504 timeout.request.v1` with `guidance_secs = proxy_timeout_secs` (the SSE branch is untouched: `IDLE_WINDOW` still governs it). Covered by `a_stalled_response_body_is_bounded_by_the_proxy_timeout`, which holds the socket open past the budget and asserts a 504 within 10 s.
**Location:** `src/infra/proxy/service.rs:1337,1417-1418,1447,1493` with `src/infra/proxy/stream.rs:201-217`

**Evidence:** `let response = match tokio::time::timeout(budget, exchange).await { … }` … `self.render(response, context, effective, endpoint).await` where `render` ends in `stream::collect(body, …).await` for every non-SSE response. The client's own `request_timeout` is 24 h and `tower`'s `TimeoutLayer` bounds the head, not the body.

**Root cause:** the `proxy_timeout_secs` budget wraps only `builder.send()` (the response head); the buffered body read is awaited outside it, and `IDLE_WINDOW` exists only on the SSE branch.

**Impact:** an upstream that sends a response head and then stalls holds the client request, its task and its pooled connection open for up to 24 hours with no gateway-side bound, contradicting the documented contract "proxy_timeout_secs bounds connection establishment and the complete buffered exchange" and the DoD `…-request-proxy-timeout-no-retry`. Repeated requests exhaust connections and task memory.

**Fix:** bound the whole `exchange` (head plus `collect`) by the remaining budget, or install an idle window on the buffered read the way `stream::relay` does.

---

### RVW-008 — Failover re-issues a request whose body was already delivered
**Severity:** MAJOR · **Source:** bfl3-3 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:1339-1354`

**Evidence:** `match self.send_once(candidate, …) { Ok(response) => return Ok(response), Err(failure) => { if matches!(failure, ProxyFailure::Domain(DomainError::RequestTimeout { .. })) { return Err(failure); } last = Some(failure); } }` — and `render()` reads the full body inside the same `exchange` call.

**Root cause:** the failover gate distinguishes failures only by "is it a `RequestTimeout`". It never asks whether the request was already delivered, and never consults the `retriable` flag the error contract carries; since `render()` reads the full body inside `exchange`, a failure after the upstream received and acted on the body is classified as `StreamAborted`/`DownstreamError` and re-issued to the next endpoint.

**Impact:** non-idempotent requests (`POST`/`PATCH`/`DELETE` with a body) can be delivered twice to the upstream — duplicated order/charge/create side effects. The module contract "no re-issue of the client request" is violated on this path.

**Fix:** restrict failover to failures that occur strictly before the request was written (connect/`send()` errors), or gate on method idempotency; honour `DomainError::DownstreamError.retriable`.

---

### RVW-009 — Plugin phase budget restarts per phase instead of consuming the request budget
**Severity:** MAJOR · **Source:** bfl12-6 · **Confidence:** high
**Location:** `src/infra/plugin/executor.rs:315-317,424,445,485,507,557,580,607,645`

**Evidence:** `pub const fn budget(&self) -> Duration { Duration::from_secs(self.proxy_timeout_secs) }` — every phase wraps its call in `tokio::time::timeout(budget, …)`, with `let budget = self.budget();` recomputed fresh.

**Root cause:** `PluginRuntime` holds only `proxy_timeout_secs` and no request start instant, so each phase is granted the whole configured budget rather than what remains, contradicting `inst-ps-exec-15` ("Bound each built-in phase per invocation by the **remaining request budget**").

**Impact:** N phases in one request can hold the request for up to N × `proxy_timeout_secs`; a slow plugin in the response phase re-starts the full budget after the upstream call already consumed most of it.

**Fix:** carry the request start `Instant` in `PluginRuntime` and compute `budget = deadline.saturating_duration_since(Instant::now())` per phase.

---

### RVW-010 — The plugin-chain call-ins run before the auth phase, opposite to the DoD order
**Severity:** MAJOR · **Source:** bfl12-4 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:550,595,637-647`

**Evidence:** order in `execute`: `enforce_cors` (550) → `enforce_rate_limit` (595) → `plugin_chain(...)` / `run_plugin_request(...)` (637-647), where `run_plugin_request` runs `Phase::Auth` first.

**Root cause:** the call-ins were placed before the plugin chain composition instead of inside it; DoD `…-request-proxy-plugin-chain-points` fixes the order as "auth, then the rate-limit call-in, then guards, then transform(request), then the CORS call-in, then the upstream call".

**Impact:** unauthenticated requests consume rate-limit quota and can be answered with a `429` before the auth plugin has had a chance to authenticate or reject them; a `transform(request)` plugin cannot influence what the rate limiter or the CORS gate sees.

**Fix:** move `plugin_chain` composition + `run_plugin_request` (at least its auth and guard phases) ahead of `enforce_rate_limit` and `enforce_cors` in `execute`.

---

### RVW-011 — The plugin in-use scan and the removal are not one critical section for `auth` references
**Severity:** MAJOR · **Source:** bfl12-5 · **Confidence:** high
**Location:** `src/infra/storage/mod.rs:202-207` (with `plugin_management.rs:498-500`)

**Evidence:** `fn plugin_in_use(&self, id: Uuid) -> bool { … table.values().any(|row| row.plugin_uuid == Some(id)) }` — no comparison of the scalar `plugin_ref` and no inspection of `UpstreamRow::auth_plugin_ref`/`auth_plugin_uuid`. It is the only check inside the write lock; the auth-column scan lives in the service layer, outside the lock.

**Root cause:** `inst-ps-scan-2` requires "Scan the upstream auth plugin reference columns … so the check does not depend on scanning JSON" and `inst-ps-del-4` requires that scan "inside the same critical section as the removal". The implementation split the scan (service layer, unlocked) from the authoritative in-lock check (storage layer, omits the auth columns and `plugin_ref`).

**Impact:** an upstream `PUT` that writes `auth.auth_type = <this plugin>` between the service scan and the delete is invisible to `plugin_in_use`, so the delete succeeds and leaves `auth_plugin_ref` pointing at a removed plugin — every subsequent proxied request to that upstream fails with `PluginNotFound`.

**Fix:** move the full reference scan (chain bindings by `plugin_ref`/`plugin_uuid`, plus the upstream auth columns) into `Tables::plugin_in_use`, or expose a repository-level `delete_if_unreferenced` that performs both steps under one write lock.

---

### RVW-012 — `replace_upstream` on the control-plane path drops the validated alias
**Disposition:** FIXED — the `Result<String>` of `validate_alias_replacement` is now bound, written into the replacement before validation and re-validation, and used for the persisted `plugin_bindings`, mirroring `UpstreamManagementService::replace`.
**Location:** `src/domain/services/mod.rs:225`

**Evidence:** `crate::domain::alias::validate_alias_replacement(&stored.upstream, &upstream)?;` — the `Result<String>` alias is discarded, then the caller's `upstream.alias` is persisted. `UpstreamManagementService::replace` does the opposite and is correct (`management.rs:1018-1019`).

**Root cause:** the immutability matrix's verdict is used only as an accept/reject gate on this path; the stored-alias/derived-alias value is dropped.

**Impact:** a full replacement moves the `(tenant_id, alias)` routing key: the upstream becomes unreachable under its old alias and reachable under a caller-chosen one, and the cache invalidation bumps only the new alias key so the old entry stays stale.

**Fix:** bind the returned alias and write it, mirroring the management path.

---

### RVW-013 — Every non-derivable pool cannot be replaced, enabled or disabled without re-supplying the alias
**Severity:** MAJOR · **Source:** bfl5-2 · **Confidence:** high
**Location:** `src/domain/services/management.rs:780-787` (via `prepare`, reached from `replace` before `validate_alias_replacement`)

**Evidence:** `(true, None) => Err(ManagementError::validation("alias", "an explicit alias is required for an IP-based or non-derivable endpoint pool"))`.

**Root cause:** `reconcile_alias` implements the *create* rule and is run on the replace path before the alias-immutability matrix, so the documented branches "unchanged pool and no supplied alias → the stored alias is retained" and "non-derivable → non-derivable: the stored alias stands" are unreachable from every write path. `alias_tests.rs:169-186` pins exactly the behavior the service never reaches.

**Impact:** `PUT /oagw/v1/upstreams/{id}` is the documented enable/disable path, and the REST DTO turns an omitted `alias` into `""`. Every upstream whose pool is an IP host, a bare public suffix, or hostnames with no common registrable suffix cannot be replaced or toggled without re-supplying the alias; the client sees a misleading 400.

**Fix:** on the replace path, seed an empty supplied alias from the stored record before `prepare`/`reconcile_alias` so the immutability matrix, not the create-time derivation rule, decides.

---

### RVW-014 — The control-plane write path persists cross-tenant plugin bindings
**Disposition:** FIXED — `ControlPlaneServiceImpl::bindings_of` is now tenant- and existence-scoped: a UUID-backed `plugins.items[]` entry is resolved through the caller's plugin catalog before any binding row is stored, so a foreign or dangling reference is a `404` and writes nothing (built-in type identifiers pass through unchanged). Covered by `a_binding_to_a_plugin_outside_the_callers_tenant_is_rejected` in `tests/schema_contract.rs`; the two schema-contract fixtures that previously bound a bare random UUID now register the catalog row first. The global `plugin_in_use` scan (second half of the finding) is the store's documented cross-tenant uniqueness posture and is left as is.
**Location:** `src/domain/services/mod.rs:144-154` and `:190-208`/`:249-264`/`:284-298`; `src/infra/storage/mod.rs:202-207` and `:683-698`

**Evidence:** `Self::bindings_of(items)` builds `PluginBinding { position, plugin_ref, plugin_uuid: uuid_of(reference) }` with no existence or tenant check. Tenant B can bind tenant A's plugin uuid; the delete gate `plugin_in_use` then scans every tenant's binding rows and vetoes tenant A's delete.

**Root cause:** the control-plane write path persists binding rows straight from the request body (no tenant-scoped resolution), and the in-use guard of the plugin table is global rather than tenant-scoped.

**Impact:** tenant B can persist a reference to tenant A's plugin, which permanently blocks tenant A's `DELETE /plugins/{id}` with an unexplained 409 — cross-tenant write interference where the referenced record is not even readable by B.

**Fix:** resolve `plugins.items[]` through the tenant-scoped catalog boundary exactly as the management aggregates do, and scope `plugin_in_use` to the deleting tenant.

---

### RVW-015 — The accepted filter field set and the evaluated field set diverge
**Severity:** MAJOR · **Sources:** bfl4-2, qual-1 · **Confidence:** high
**Location:** `src/domain/list_query.rs:700-739` (vs `:29-41`, `:53-65`)

**Evidence:** `FIELDS`/`ROUTE_FIELDS` admit `"server", "tags", "auth", "headers", "rate_limit", "cors"` (and for routes `"match", "plugins"`), but `field_value`'s `QueryRecord::Upstream` arm handles only `alias|protocol|id|tenant_id|enabled` and its `Route` arm only `id|upstream_id|tenant_id|match_type|priority|enabled`, returning `None` otherwise; `matches_record` then does `if *comparison == Comparison::Equals { holds } else { !holds }`.

**Root cause:** the same field-name vocabulary is hand-maintained in four parallel tables (`*_FIELDS`, `FieldSet::kind_of`, `compare_*_field`, `field_value`) with no shared descriptor, so they drift independently; an unmatched field yields `None`, so the comparison degenerates instead of being rejected.

**Impact:** `$filter=tags eq 'prod'` returns 200 with an empty list for every record; `$filter=tags ne 'x'` (or `server`, `cors`, `rate_limit`, …) matches every record unconditionally; `startswith(tags,'x')`/`contains(...)` never match; `$orderby=rate_limit|cors|auth|headers` is accepted and does not order. Management list results are silently wrong with no error.

**Fix:** replace the four tables with one per-`FieldSet` descriptor (field name, kind, filter getter, ordering key) and derive `kind_of`, `compare_*_field` and `field_value` from it; reject at parse time any field with no filter/order implementation.

---

### RVW-016 — An owned Private CORS block behaves as first-wins instead of a union
**Severity:** MAJOR · **Source:** bfl4-3 · **Confidence:** high
**Location:** `src/domain/merge.rs:345-352`

**Evidence:** `Sharing::Private => { if cors.value.is_none() { cors.value = Some(candidate.clone()); cors.sharing = Some(share.cors); } }`

**Root cause:** the branch implements "first contributor wins", but the comment (and the union semantics of `cors::merge_fields` used by the `Inherit` arm directly above) state an owned private layer behaves like `inherit`, i.e. an add-only union. `CorsConfig.sharing` defaults to `Private`, and `route_layer` derives the route layer's `cors` sharing from that default.

**Impact:** when an upstream declares any `cors` block, a route's own `cors` block (default `sharing: private`) is silently discarded, so the effective CORS for that route is the upstream's — the route cannot add origins/methods and cannot be narrower than the upstream.

**Fix:** in the `Sharing::Private` arm, when `owned` and `cors.value` is `Some(previous)`, merge with `union_cors_origins(previous, candidate)`.

---

### RVW-017 — `CachedUpstreamRepository` stamps the generation after the read, so its race guard is inert
**Disposition:** FIXED — `CachedUpstreamRepository::get_by_alias` captures the generation before `inner.get_by_alias` and inserts only while it still equals the current generation, so a population whose read raced a write no longer inserts a pre-write value. Covered by `a_population_whose_read_raced_a_write_does_not_insert`, whose store bumps the key's generation mid-read and asserts the entry stays absent and the next read returns to the store.
**Location:** `src/infra/cp_cache.rs:564-569` (contract at `:26-32`)

**Evidence:** `match self.inner.get_by_alias(tenant_id, alias) { Ok(record) => { self.state.l1.insert(&rendered, record.clone(), self.generations.generation(&rendered)); Ok(record) }`

**Root cause:** the generation stamp is read at insert time, so the hit-path check `if generation == current` compares a value stamped with the then-current generation against the current generation — always equal unless a *later* write lands. The documented guard ("an insert is accepted only when that generation still equals the store's current generation … so a population that raced a flush cannot insert a pre-write value") does not exist in the code: the read and the stamp are not one critical section.

**Impact:** `CachedUpstreamRepository` is the single upstream read path for the control-plane service, all three management aggregates and the proxy data plane, so a pre-write record can be served as a cache hit indefinitely after any management write that raced it.

**Fix:** capture the generation before `get_by_alias`, then insert only if it still equals the current generation (mirroring `DpHotConfig::put`'s guard).

---

### RVW-018 — `DpHotConfig` samples the dependency generation after the store read
**Disposition:** FIXED — `level_record` now samples the dependency generations through `cache.observe(&[key])` *before* `store_record` and passes them to `put`, so the guard `put` runs is no longer self-comparing; the unused `generations()` accessor on the data-plane cache is gone from the hot path. The guard itself is pinned by `a_population_racing_a_write_is_refused` in `src/infra/dp_cache_tests.rs`; the ordering is not independently race-injectable from the integration harness and is asserted by construction.
**Location:** `src/infra/proxy/service.rs:764-771` (with `src/infra/dp_cache.rs:90-93,158-168`)

**Evidence:** `let Some(record) = self.store_record(tenant_id, alias)? else { return Ok(None); }; … cache.put(key, DpValue::Upstream(Arc::new(record.clone())), observed);` where `observed` is sampled after `store_record`.

**Root cause:** `Observed` is defined as "the generations the resolution observed **before** it read the store", and `put` re-checks those generations "so a population that raced a flush cannot insert a pre-write value". By sampling after the read the guard compares the current generation against itself and can never fail. `DpHotConfig::observe()` has no production caller.

**Impact:** a permanently stale upstream record in the Data Plane L1 cache (1,000 entries, no TTL). A deleted or `enabled=false` upstream keeps receiving proxied traffic; a changed endpoint host, CORS policy or rate limit is not applied until LRU eviction.

**Fix:** sample the dependency generation before the store read via `cache.observe(&[key])`, then `put` with that value.

---

### RVW-019 — Rate-limit counters are reset by LRU eviction under key churn
**Disposition:** PARTIALLY FIXED — `make_room` now scopes its first eviction pass to the resource of the key being inserted (a counter key is `{resource}|{identity}`), so a caller churning its own key surface can only reset a budget its own requests draw from, and live evictions are counted in a new `live_evictions()` accessor instead of being silent. The residual exposure is one resource's own `scope: ip` key space: callers sharing a route still compete for that route's counters, and no shared-store spill exists. Covered by `a_churning_resource_does_not_evict_another_resources_counters` in `src/infra/proxy/rate_limiter_tests.rs`. Cross-tenant displacement of counters on *other* resources is closed; the same-resource competition is a documented limitation, not a fix.
**Location:** `src/infra/proxy/rate_limiter.rs:226-246` (and `:156-176`)

**Evidence:** when the bounded registry (10,000 keys) is full, every cache miss evicts the globally least-recently-used counter; the next `acquire` for the evicted key re-creates it via `CounterEntry::fresh` at full capacity, so the eviction *is* a budget reset. `scope: ip` keys derive from `peer_addr` and are not bounded per tenant.

**Root cause:** nothing prevents an attacker from being the source of the churn: a caller presenting enough distinct keys keeps the registry at capacity so other tenants' counters are continuously evicted and their configured limits are never enforced. The eviction is unobservable.

**Impact:** rate-limit bypass by key churn; the eviction is silent.

**Fix:** make the eviction per-resource or per-tenant (never evict a counter whose owning tenant differs from the caller's), or spill to a shared store at capacity, or at minimum record the eviction and refuse admission (fail closed).

---

### RVW-020 — Rate-limiter eviction cost is O(n) per miss at capacity
**Disposition:** FIXED — eviction now drains the registry to a low-water mark (capacity − capacity/`EVICTION_BATCH_FRACTION`) in one pass, so the O(n) scan is paid once per batch rather than once per miss at capacity; the idle sweep still runs first and costs nothing when the registry is under its bound.
**Location:** `src/infra/proxy/rate_limiter.rs:226-246`

**Evidence:** `make_room` runs a full O(n) `sweep()` (10,000 per-key mutex acquisitions plus a `retain`) followed by a full O(n) LRU scan that evicts a single entry.

**Root cause:** once the registry sits at capacity with live keys, every new key pays the full scan, so the map stays at capacity and the work repeats.

**Impact:** under key churn, per-request cost becomes O(10k) lock acquisitions plus a global `DashMap` scan — CPU-bound amplification on the hot path.

**Fix:** evict down to a low-water mark in one pass, amortize the sweep, use an atomic per-entry last-used stamp, and maintain LRU order incrementally.

---

### RVW-021 — The L1 caches serialize every read on one global mutex with an O(capacity) victim scan
**Severity:** MAJOR · **Sources:** perf-7, bfl3-12 · **Confidence:** medium
**Location:** `src/infra/cp_cache.rs:227-244,262-308`; `src/infra/dp_cache.rs:210-221`

**Evidence:** `entries: Mutex<(HashMap<String, Entry<V>>, u64)>` with `insert` evicting via `guard.0.iter().min_by_key(|(_, entry)| entry.last_used)`; `ConfigGenerations { generations: Arc<Mutex<HashMap<String, u64>>> }` with `generation(&key)` locking per call.

**Root cause:** both the LRU map and the generation table are single global mutexes with no sharding, and the victim selection is an O(capacity) scan performed while holding the same lock every read needs.

**Impact:** the L1 cache that exists to keep the hot path cheap becomes a cross-core serialization point; at capacity each insert scans the whole map under the lock.

**Fix:** shard the L1, use an intrusive LRU list, and store generations per key in a sharded map or atomic-cell array.

---

### RVW-022 — Store reads are full-table scans with no secondary index
**Severity:** MAJOR · **Source:** perf-6 · **Confidence:** high
**Location:** `src/infra/storage/mod.rs:463-471,567-580`; `src/infra/proxy/service.rs:1001-1019,1146-1169`

**Evidence:** `get_by_alias`: `tables.upstream.values().find(|row| row.tenant_id == tenant_id && row.upstream.alias == alias)`; `list_for_upstream` scans, sorts and rehydrates every row; `bindings_of` scans the whole binding map per record.

**Root cause:** the store keeps no index for its documented unique/scoping keys — `(tenant_id, alias)`, `(tenant_id, upstream_id)`, `(parent_id)` — so every hot-path read is a full-table scan under a single shared `RwLock` that also gates every management write.

**Impact:** per-request CPU and allocation grow linearly with the *total* number of rows in the store, not just the tenant's.

**Fix:** maintain secondary indexes in `Tables` updated in `write_*`/`cascade_*`, and return `Arc<RouteRecord>`/borrowed candidates.

---

### RVW-023 — Failover/error handling and gauge wiring defects in the transport leg
**Severity:** MAJOR · **Source:** obs-4 · **Confidence:** medium
**Location:** `src/api/rest/proxy_handlers.rs:325-340` (API at `src/infra/metrics.rs:383-390`)

**Evidence:** `observability.metrics.enter_in_flight(alias);` … `let (response, dispatched) = dispatch(...).await;` … `observe_outcome(...); if let Some(alias) = &alias { observability.metrics.leave_in_flight(alias); }`

**Root cause:** the decrement is an ordinary statement after the await with no RAII guard, and for a relayed stream it runs when the response head is produced, not when the exchange ends.

**Impact:** any cancellation that unwinds the handler future before the decrement leaves `oagw_requests_in_flight` permanently elevated, while a long streaming exchange is counted as finished while its body is still streaming — the gauge drifts upward.

**Fix:** return a guard from `enter_in_flight` (RAII, holding `Arc<MetricsRegistry>` + host) and keep that guard alive inside the streamed body's terminal state.

---

### RVW-024 — `resolve_proxy_target` re-implements route selection instead of delegating to the matcher
**Severity:** MAJOR · **Sources:** eng-1, bfl5-6 · **Confidence:** high
**Location:** `src/domain/services/mod.rs:361-372`

**Evidence:** `fn route_matches(route: &Route, method: &str, path: &str) -> bool` — a second, hand-rolled matcher next to the canonical `crate::domain::route_matcher::select` the data plane uses. Separately, `services/mod.rs:343-350` ranks by `.find()` over a priority-sorted list rather than longest-prefix.

**Root cause:** the control-plane facade re-implements the path/method filter instead of delegating.

**Impact:** the two matchers disagree: `route_matches` ignores `PathSuffixMode`, does no longest-prefix/priority ranking, and cannot distinguish `NotFound` from `MethodNotAllowed`. `ControlPlaneService::resolve_proxy_target` — the ADR 0006 contract published on the client hub — can report a different matched route than the request the data plane actually proxies.

**Fix:** delete `route_matches` and have `resolve_proxy_target` call `route_matcher::select`, mapping the outcome onto `DomainError`.

---

### RVW-025 — The `ControlPlaneService` write surface duplicates the management aggregates
**Severity:** MAJOR · **Source:** eng-2 · **Confidence:** high
**Location:** `src/domain/services/mod.rs:190-298,304-325`

**Evidence:** the facade keeps its own implementation of the upstream/route/plugin write paths after entries 2.2/2.3/2.6 introduced the management aggregates for the same operations, so "write an upstream" exists twice with different rules: the facade skips alias derivation/reconciliation, plugin-binding resolution, `auth.config` schema validation, the ancestor-bind gate, the config-write hook, and tenant stamping.

**Impact:** two code owners for one invariant; every rule added to the management aggregates must be remembered in the facade or the hub-published contract silently accepts records the management surface rejects.

**Fix:** reduce `ControlPlaneServiceImpl` to the read/resolution surface the data plane and the hub need, and delegate its mutating methods to the management aggregates (or delete them).

---

### RVW-026 — The plugin-CRUD section of `ControlPlaneService` has no callers
**Severity:** MAJOR · **Source:** eng-3 · **Confidence:** high
**Location:** `src/domain/services/mod.rs:57-60,304-325`

**Evidence:** `create_plugin`/`list_plugins`/`delete_plugin` are referenced only by the trait and its impl; the plugin catalog's real surface is `PluginManagement` (per-base-type permissions, reference-guarded delete), which the facade's plugin methods bypass entirely.

**Impact:** speculative, untested surface on the contract other gears consume: it advertises plugin writes with none of the entry 2.6 rules, so a future client that binds to it silently bypasses authorization and the in-use guard.

**Fix:** remove the plugin-CRUD section from `ControlPlaneService` (and from `ControlPlaneServiceImpl`), keeping plugin management on `PluginManagement`.

---

### RVW-027 — Conflict classification is copy-pasted at nine call sites
**Severity:** MAJOR · **Source:** eng-3b · **Confidence:** high
**Location:** `src/domain/services/mod.rs:201-207,236-242,257-263,291-297,306-312`; `management.rs:950-956,1032-1038`; `route_management.rs:380-386,462-468`

**Evidence:** `.map_err(|error| { if error.is_conflict() { self.map_conflict(WriteConflict::RouteMatch) } else { error } })` repeated at nine call sites.

**Root cause:** the repository-conflict → domain-error mapping is copy-pasted at every write call site instead of living once behind the repository boundary; each site hard-codes which conflict the caller "means".

**Impact:** any correction to conflict classification must be replicated at nine sites; the already-reported mislabelling of every `create_route`/`replace_route` conflict as a route-match conflict is a direct symptom, and `create_plugin` maps a `BindingPositions`/`MissingParent` conflict to a UniqueKey alias conflict.

**Fix:** have the repositories return the already-classified `WriteConflict` and map it to `DomainError` in exactly one place, then delete the per-call-site closures.

---

### RVW-028 — The write-hook epilogue is implemented three times independently
**Severity:** MAJOR · **Source:** eng-4 · **Confidence:** medium
**Location:** `management.rs:504,551-591,602-625`; `route_management.rs:167,185-199,211-214,332-351`; `plugin_management.rs:128,145-152,339-369`

**Evidence:** `hook: RwLock<Arc<dyn ConfigWriteHook>>` + `set_config_write_hook` + `hook.on_*_written(notification).await.map_err(ManagementError::Domain)` repeated verbatim in all three aggregates, with three near-identical `ConfigWriteNotification` builders.

**Root cause:** the store-write → CP L1 invalidation → DP flush → audit → success epilogue is documented as one invariant but implemented as three independent copies.

**Impact:** the ordering guarantee and the notification shape can drift per aggregate (they already have: only `UpstreamManagementService` carries `upstream_alias`); a change to the epilogue must be re-implemented three times.

**Fix:** extract one shared `ConfigWriteEmitter` owning hook storage and the `upstream/route/plugin` dispatch.

---

### RVW-029 — Required ports are post-construction setters seeded with permissive no-op defaults
**Severity:** MAJOR · **Source:** eng-5 · **Confidence:** medium
**Location:** `management.rs:512-535,559-591`; `gear.rs:524-533`

**Evidence:** `struct NoPluginBindingResolver;` returning `Ok(bindings_of_references(references))` installed via `set_plugin_binding_resolver`.

**Root cause:** DIP inverted at construction: the two plugin-catalog ports and the write hook are not constructor dependencies but post-construction setters seeded with permissive no-op defaults.

**Impact:** there is a window, and no guard at all afterwards, in which `UpstreamManagementService` accepts writes that persist plugin bindings without any resolvability, ref/uuid-agreement or `auth.config` check. Forgetting one `set_*` call compiles, runs and silently weakens validation.

**Fix:** take the ports as `new()` parameters, or expose a builder whose `build()` fails if a required port is missing; delete `NoPluginConfigValidator`/`NoPluginBindingResolver`.

---

### RVW-030 — The binding-shape rule has two independent owners
**Severity:** MAJOR · **Source:** eng-6 · **Confidence:** high
**Location:** `src/domain/services/mod.rs:144-160`; `management.rs:1158-1170`

**Evidence:** `bindings_of` in the facade uses `plugin_uuid: uuid_of(reference)`; `bindings_of_references` in the management module uses `plugin_uuid: Uuid::parse_str(reference).ok()`.

**Root cause:** the documented storage rule "positions are contiguous from zero, `plugin_ref` always stored, `plugin_uuid` only when UUID-backed" has two independent owners in the same crate.

**Impact:** the two copies can diverge on the persisted binding shape, producing rows that only one write path formats.

**Fix:** keep one function and call it from `ControlPlaneServiceImpl`; delete the private `bindings_of`/`uuid_of` pair.

---

### RVW-031 — The shareable-block inventory is walked by hand in parallel with the merge engine
**Severity:** MAJOR · **Source:** eng-7 · **Confidence:** medium
**Location:** `management.rs:656-693`

**Evidence:** the four shareable blocks and their `SharingMode` are re-walked by hand in `ancestor_is_private`/`enforce_ancestor_blocks`, in parallel with `merge::upstream_base_layer` — and the two disagree on the default: merge treats an absent block as `SharingMode::Private`, the bind gate treats it as "not declared" and lets the bind through.

**Impact:** adding a shareable block requires synchronized edits in three places; a missed one leaves the merge engine enforcing an ancestor block the gate happily lets a descendant write.

**Fix:** extract one owner for the block/sharing inventory, e.g. `Upstream::sharing_blocks()`, and implement the gate over it.

---

### RVW-032 — Route ranking uses the raw path length, so a trailing-slash route wins over a higher priority
**Severity:** MAJOR · **Source:** bfl5-4 · **Confidence:** high
**Location:** `src/domain/route_matcher.rs:320` and `:347-352`

**Evidence:** `path_matched.push((candidate, route_path.len()))` ranks on the raw `match.http.path` length, while the write path normalizes a trailing slash as the same prefix.

**Root cause:** the ranking key is the untrimmed path length while matching and uniqueness both trim trailing slashes.

**Impact:** the documented ranking "longest matching path prefix, then route priority" is inverted for trailing-slash route paths: a lower-priority route is selected over the higher-priority one.

**Fix:** rank on `route_path.trim_end_matches('/').len()`.

---

### RVW-033 — The rate-limit counter resource is derived from the declared route block, not from the effective limit
**Severity:** MAJOR · **Source:** bfl5-5 · **Confidence:** medium
**Location:** `src/infra/proxy/service.rs:825-829`

**Evidence:** `let resource = if route.rate_limit.is_some() { RateLimitResource::Route { route_id } } else { RateLimitResource::Upstream { upstream_id } };` — while the limit enforced is the merged `rate_limit_view`.

**Root cause:** the counter key's resource component is derived from the declared route block instead of from the layer that contributed the effective limit.

**Impact:** an ancestor tenant's `sharing: enforce` upstream limit, which the code and tests describe as standing "absolutely", is enforced per route: with two routes declaring a `rate_limit` plus one that does not, the same effective limit is spread over three independent counters.

**Fix:** pick the resource from the effective configuration, attributing the counter to the route only when the route layer actually contributed the effective limit.

---

### RVW-034 — `ResolvedSecret`'s zero-on-drop is not guaranteed
**Severity:** MAJOR · **Source:** sec-2 · **Confidence:** medium
**Location:** `src/infra/plugin/credentials.rs:47-53`

**Evidence:** `impl Drop for ResolvedSecret { fn drop(&mut self) { for byte in &mut self.buffer { *byte = 0; } } }` — an ordinary write loop into a buffer deallocated immediately afterwards, with no volatile/synchronization primitive; the oagw crate does not depend on `zeroize`.

**Root cause:** LLVM's dead-store elimination is free to remove the stores; `credentials.rs:145` also copies the SDK's `SecretValue` into a plain `Vec<u8>`.

**Impact:** the documented guarantee ("The buffer is zeroed on drop, which is the guarantee the Known Residual Plaintext section of ADR 0008 records") is not enforced, so resolved credentials can persist in freed heap memory and leak into core dumps/swap.

**Fix:** depend on `zeroize` and hold `Zeroizing<Vec<u8>>` (or derive `ZeroizeOnDrop`).

---

### RVW-035 — Proxy write handlers return non-problem bodies for body-size and body-read failures
**Severity:** MAJOR · **Source:** err-2 · **Confidence:** high
**Location:** `src/api/rest/proxy_handlers.rs:550-563`

**Evidence:** `let body = match axum::body::to_bytes(request.into_body(), limit).await { Ok(bytes) => bytes, Err(_) => { return (ApiError::Domain(DomainError::PayloadTooLarge { … })) } }`

**Root cause:** the read-failure arm is a catch-all `Err(_)` that discards the error value: axum's `to_bytes` returns `LengthLimitError` for an over-limit body but the same `axum::Error` type for a connection reset, a stream reset, or a malformed chunked frame.

**Impact:** a client whose connection drops mid-upload receives `413` with a `detail` stating "the request body exceeds the configured body-size limit of N bytes" — a false statement. The real cause is swallowed with no `tracing` record.

**Fix:** downcast the error and map only the size case to `PayloadTooLarge`; classify every other read failure as a body-shape failure rendered as 400 `validation.error.v1` and `tracing::error!` the cause.

---

### RVW-036 — Upstream by-identifier handlers use the `Path<Uuid>` extractor, producing a plain-text rejection
**Severity:** MAJOR · **Source:** err-3 · **Confidence:** high
**Location:** `src/api/rest/upstream_handlers.rs:163` (also `:181`, `:200`)

**Evidence:** `axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,`

**Root cause:** axum's `FailedToDeserializePathParams` renders `Invalid URL: ...` as `text/plain; charset=utf-8` at 400, and neither `canonical_error_middleware` nor `complete_problem_context` touches non-problem responses.

**Impact:** `GET/PUT/DELETE /oagw/v1/upstreams/not-a-uuid` returns a gateway error that is not `application/problem+json`, carries no `X-OAGW-Error-Source: gateway`, no `trace_id` and no `instance` — violating the problem-details contract. The sibling surfaces deliberately avoid exactly this (`route_handlers.rs:100-106`, `plugin_handlers.rs:79-93`).

**Fix:** take `Path<String>` and route it through a shared `upstream_id_of(segment)` helper returning `DomainError::field_rejection("id", …)`.

---

### RVW-037 — Management write handlers read the body through the `Bytes` extractor
**Severity:** MAJOR · **Source:** err-4 · **Confidence:** high
**Location:** `upstream_handlers.rs:78` (also `route_handlers.rs:128,236`; `plugin_handlers.rs:106`)

**Evidence:** `body: axum::body::Bytes,`

**Root cause:** the `Bytes` extractor's rejection renders plain text at 413 (`LengthLimitError`) or 400 (`UnknownBodyError`) — the same class of failure the gear maps to problem+json on the proxy path.

**Impact:** a management create/replace whose body exceeds the effective limit returns `text/plain` "Failed to buffer the request body" with no `X-OAGW-Error-Source`, no `trace_id`, no GTS type and no `instance`.

**Fix:** replace the `Bytes` extractor with `Request` + `to_bytes(body, limit)` inside a shared `management_body_of` helper that maps `LengthLimitError` to `PayloadTooLarge`.

---

### RVW-038 — The raw serde error is interpolated verbatim into the rendered `detail`
**Severity:** MAJOR · **Source:** err-5 · **Confidence:** high
**Location:** `upstream_handlers.rs:49-54` (also `route_handlers.rs:56-61,80-85`; `plugin_handlers.rs:58-63`)

**Evidence:** `serde_json::from_slice::<UpstreamRequest>(bytes).map_err(|error| … field_rejection("body", &format!("failed to deserialize the JSON body into the target type: {error}")))`

**Root cause:** for a wrongly typed field serde builds the message with `invalid_type(Unexpected::Str(&s), exp)`, so the offending value from the request body is carried whole and unbounded into the rendered `detail`.

**Impact:** request body content reaches a rendered problem body, which the error contract forbids outright (`inst-eh-val-3`), and a caller can turn a body field into an echoed value of arbitrary size. Unlike `invalid_value` and `trace_id`, no bound is applied.

**Fix:** do not interpolate `{error}`. Map the serde failure to a fixed, field-oriented message, name the field only when recoverable, and keep the full serde text for `tracing::debug!`.

---

### RVW-039 — `GET /oagw/v1/upstreams/{gts-id}` is rejected although the FEATURE doc specifies it
**Severity:** MAJOR · **Source:** cons3-1 · **Confidence:** high
**Location:** `docs/features/upstream-management.md:214,476` vs `src/api/rest/upstream_handlers.rs:163,181,200`

**Evidence:** the doc states the path identifier is "the anonymous GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}`"; all three by-id handlers take `Path<uuid::Uuid>`. The route and plugin surfaces do implement the dual form.

**Impact:** a client following the doc and sending the GTS form is rejected by the strict UUID extractor instead of receiving the record.

**Fix:** either rewrite the doc to say the path identifier is the bare UUID, or extend the three handlers to resolve both forms.

---

### RVW-040 — Test-inventory DoD entries name 28 test paths that do not exist
**Disposition:** FIXED. The layer-boundary test the doc names now exists at the exact path `src/domain/layer_boundary_tests.rs`; the other 27 absent paths are repointed at the delivered files (`src/domain/route_matcher_tests.rs`, `src/domain/header_transform_tests.rs`, `src/domain/merge_tests.rs`, `src/infra/proxy/stream_lifecycle_tests.rs`, `src/infra/cp_cache_tests.rs`, `tests/proxy_dispatch.rs`, `tests/proxy_streaming.rs`, `tests/streaming_error_source.rs`, `tests/error_contract.rs`, `tests/cors_preflight.rs`, `tests/cors_enforcement.rs`, `tests/rate_limiting.rs`, `tests/plugin_chain_ordering.rs`, `tests/control_plane_resolution.rs`, `tests/upstream_enable_disable.rs`, `tests/upstream_validation.rs`, `tests/schema_contract.rs`, `tests/audit_log_shape.rs`). The two paths with no counterpart are rewritten as statements of the delivered posture instead: `tests/proxy_webtransport_session.rs` (no WebTransport relay — RVW-129) and `tests/proxy_latency.rs` (the phase histograms and the audited `duration_ms` are the delivered duration evidence; no p95 figure is claimed). A sweep over all nine FEATURE docs now resolves every test-file reference to a file in the tree.
**Severity:** MAJOR · **Sources:** cons2-2, cons3-2, cons3-8, cons3-9, cons3-10 · **Confidence:** high
**Location:** `docs/features/request-proxy.md:634-1009`; `error-handling.md:452,574,610`; `gear-foundation.md:531`; `observability-and-state.md:772,786,809,839`

**Evidence:** 28 distinct backticked test-file paths referenced across four FEATURE docs do not resolve to any file under `oagw/`; 57 reference instances in total. `request-proxy.md` names 19 absent `tests/proxy_*.rs` (only `proxy_dispatch.rs` and `proxy_streaming.rs` exist) and 5 absent or mis-located `src/infra/proxy/*_tests.rs` (`route_matcher_tests.rs` and `header_transform_tests.rs` actually live in `src/domain/`).

**Root cause:** the `Tests:` lines and the DoD test-inventory blocks name a planned test file layout that was never created; the behavior was implemented under different file names and, for the route matcher and header transform, in the `domain` layer rather than `infra/proxy`. The docs were not reconciled with the delivered tree.

**Impact:** broken dependency edge in the verification chain — every one of these is a normative "Tests:" assertion whose named evidence does not exist, so the DoD reads as unmet.

**Fix:** repoint every reference at the delivered locations (`tests/proxy_dispatch.rs`, `tests/proxy_streaming.rs`, `tests/streaming_error_source.rs`, `src/domain/route_matcher_tests.rs`, `src/domain/header_transform_tests.rs`, `src/infra/cp_cache_tests.rs`, …) and delete or rewrite the entries that name files with no counterpart.

---

### RVW-041 — `DECOMPOSITION.md` asserts the nine FEATURE artifacts "do not exist yet"
**Severity:** MAJOR · **Sources:** cons2-3, cons1-15, cons3-5 · **Confidence:** high
**Location:** `docs/DECOMPOSITION.md:27` (and the unticked status boxes at `:22,66,153,225,292,381,442,521,584,643`)

**Evidence:** "The `features/<slug>.md` documents linked from the entry headings are the FEATURE artifacts to be authored from these candidates; they do not exist yet." All nine exist and are non-empty, each carrying a ticked `featstatus-*-implemented` box.

**Root cause:** the sentence and the status markers were written before the nine FEATURE docs were authored and were never revised.

**Impact:** the decomposition states a false material fact about the repository state and reports every entry as not started, so any status aggregation over the two documents contradicts itself.

**Fix:** delete the clause and tick the decomposition status boxes (or replace them with links to each feature doc's `featstatus` marker).

---

### RVW-042 — The less-than-1ms rate-check ceiling is asserted by no benchmark
**Severity:** MAJOR · **Source:** cons3-3 · **Confidence:** high
**Location:** `docs/features/rate-limiting.md:495,506,599`

**Evidence:** the DoD obligates "a Criterion-based benchmark test that measures the check alone"; `grep -rn criterion oagw/` returns only prose, no `criterion` entry in `oagw/Cargo.toml`, and no `oagw/benches/` directory. The workspace does provide `criterion = { version = "0.8" }` and two other crates use it.

**Impact:** a ticked DoD and a named test obligation that cannot be satisfied; the ceiling the ADR records is asserted nowhere.

**Fix:** add `criterion.workspace = true` with a `[[bench]]` target measuring one `RateLimiterRegistry` acquire with the injected clock, or restate the DoD as a recorded deviation.

---

### RVW-043 — The FEATURE doc denies the metrics permission the code mints
**Disposition:** FIXED. `features/observability-and-state.md` now names the minted identifier `gts.cf.core.oagw.proxy.v1~:metrics`, states its root-only tenant mode, and records the DESIGN §3.2 delta; the three spellings are reconciled to that identifier in `src/api/rest/metrics_routes.rs` and `src/infra/authorization.rs`.
**Severity:** MAJOR · **Source:** cons3-4 · **Confidence:** high
**Location:** `docs/features/observability-and-state.md:595` vs `src/infra/authorization.rs:136`

**Evidence:** the doc says "this entry mints no OAGW permission identifier of its own because DESIGN §3.2 defines none for the metric surface", while the code defines `pub const PERM_METRICS: &str = "gts.cf.core.oagw.proxy.v1~:metrics";` used by `MetricsGate::authorize` with `TenantMode::RootOnly`, and `metrics_routes.rs:10,50` describes the decision as `oagw:proxy:metrics`.

**Impact:** an operator reading the FEATURE doc concludes the metric gate is evaluated without a permission identifier and cannot write a policy for it, and the three spellings prevent a single policy rule from matching.

**Fix:** rewrite `:595` to name the minted identifier and record the DESIGN §3.2 delta as a deviation; reconcile `metrics_routes.rs` to the same single spelling.

---

### RVW-044 — Four FEATURE docs declare themselves implemented with §6 entirely unticked
**Severity:** MAJOR · **Source:** cons3-6 · **Confidence:** high
**Location:** `gear-foundation.md:46` (§6 at 568-582, 0/14); `plugin-system.md:54` (0/17); `route-management.md:40` (0/16); `upstream-management.md:49` (0/29)

**Evidence:** each carries `- [x] p1 - cpt-cf-oagw-featstatus-*-implemented` with every §6 acceptance-criterion box unticked, versus siblings that tick their whole §6.

**Impact:** four of nine entries read as delivered but unverified, so the §6 lists cannot be used as a completion signal.

**Fix:** verify and tick the §6 items of the four entries, or state in each doc's §6 preamble that the criteria are owned by a separate verification pass.

---

### RVW-045 — Health never reports unhealthy for the four state components it names
**Severity:** MAJOR · **Source:** obs-1 · **Confidence:** high
**Location:** `src/infra/health.rs:220-223,250-259,146-169`

**Evidence:** `pub fn unhealthy(&self)` has no production caller, and `StateComponent` is referenced by no code outside `health.rs`. `gear.rs` only calls `health.initializing()` and `health.ready()`.

**Root cause:** the four state components the contract names are never probed and nothing in the gear drives the holder to `Unhealthy`, so the transition `ready → unhealthy` has no trigger.

**Impact:** `inst-os-health-5`/`-6` cannot fire: a failed CP L1 cache, DP cache, metrics registry or audit emitter keeps reporting `ready`, so the platform keeps routing proxy traffic to a gear whose owned state is unavailable.

**Fix:** carry per-component state in `HealthStateHolder`, derive `GearHealth::check` from the four components, and drive `unhealthy()`/`ready()` from the component-availability points.

---

### RVW-046 — Every `config_change` audit record carries `request_id: null`
**Severity:** MAJOR · **Source:** obs-2 · **Confidence:** high
**Location:** `src/infra/observability.rs:250-258` (with `management.rs:249-279`)

**Evidence:** `self.audit.emit(&AuditRecord::config_change(None, &notification.tenant_id.to_string(), …))`

**Root cause:** `ConfigWriteNotification`/`WriteNotification` carry no correlation identifier, so the hook can never fill `request_id`, although `inst-os-algo-audit-2b` requires it populated.

**Impact:** a management write cannot be joined to the request that caused it — the correlation-identifier DoD is unmet for the whole class.

**Fix:** add `request_id: Option<String>` to the notifications, thread it through `notify_written`, and pass it to `AuditRecord::config_change`.

---

### RVW-047 — Audit write failures are silently discarded
**Severity:** MAJOR · **Source:** obs-3 · **Confidence:** high
**Location:** `src/infra/audit.rs:386-392`

**Evidence:** `Destination::Stdout => { let stdout = std::io::stdout(); let mut handle = stdout.lock(); let _ = writeln!(handle, "{line}"); let _ = handle.flush(); }`

**Root cause:** both results are discarded and nothing records the failure — no counter, no metric, no log line — while `AuditSink` is the only emitter of the audit stream.

**Impact:** if stdout is full, blocked or redirected away, every audit record is silently lost, including the 100%-emission `proxy_request` class; a compliance-audit gap would be undetectable.

**Fix:** track failures on `AuditSink` (an `AtomicU64` of dropped records) and emit a rate-limited `tracing::error!` on the first failure.

---

### RVW-048 — The proxy data path emits no tracing spans or records at all
**Severity:** MAJOR · **Source:** obs-5 · **Confidence:** high
**Location:** `src/infra/observability.rs:83-137` and the whole data path

**Evidence:** `grep -rn "tracing::" infra/proxy/ infra/observability.rs infra/metrics.rs infra/health.rs api/rest/proxy_handlers.rs` returns no matches; the only `tracing` calls in the crate are three `info!` in `gear.rs`, one `warn!`, one `error!` and three `debug!` elsewhere.

**Root cause:** the entry treats the 14-field audit record as its only structured signal, and the human-readable DEBUG stream the contract designates is never produced on the proxy path.

**Impact:** a request failing with `error_type=upstream.connection_timeout` leaves no human-readable diagnostic anywhere, and the five timed pipeline phases cannot be followed as spans in a distributed trace even though the timing already exists.

**Fix:** open a `tracing` span per proxied exchange keyed by `request_id`/`host`/`http.route`/method with child spans for the five phases, and emit a `debug!` record carrying the human-readable failure message where `error_type` is set.

---

### RVW-049 — Requirement-reference checkbox states disagree across the PRD, DECOMPOSITION and the FEATURE docs
**Severity:** MAJOR · **Sources:** cons1-3, cons1-4, cons3-12 · **Confidence:** high
**Location:** `gear-foundation.md:66`; `request-proxy.md:92`; `error-handling.md:62`

**Evidence:** `gear-foundation.md:66` ticks `cpt-cf-oagw-nfr-credential-isolation` while `PRD.md:499` and `DECOMPOSITION.md:96` leave it unticked; `request-proxy.md:92` ticks `cpt-cf-oagw-nfr-input-validation` while `PRD.md:509` and `DECOMPOSITION.md:336` leave it unticked; `error-handling.md:62` writes `cpt-cf-oagw-fr-error-codes` unticked while the decomposition marks the same id as inherited-done.

**Root cause:** DECOMPOSITION.md defines the convention ("a ticked requirement reference means the PRD definition is already marked done upstream"), but three FEATURE docs apply it inconsistently.

**Impact:** the requirement-reference ticks no longer carry a single meaning, so an inherited-versus-delivered reading of any entry's requirement list is unreliable.

**Fix:** align each FEATURE-doc reference with the PRD state; do not edit the PRD (a supplied upstream document).

---

### RVW-050 — The shared §1 template is applied four different ways across the nine FEATURE docs
**Severity:** MAJOR · **Source:** cons1-5 · **Confidence:** high
**Location:** `cors.md:94`, `upstream-management.md:93`, `gear-foundation.md:90`, `plugin-system.md:115,119`, `error-handling.md:87`, `route-management.md:84`, `observability-and-state.md:104`, `rate-limiting.md:70`, `request-proxy.md:99`

**Evidence:** three docs use `### 1.5 Scope Exclusions`; `plugin-system.md` uses `### 1.5 Domain Applicability` plus `### 1.6 Graded Deviations`; five docs use an unnumbered bold paragraph; `rate-limiting.md` and `request-proxy.md` inline the statement inside §1.2.

**Root cause:** the shared §1 template was not applied uniformly; the same structural slot was placed at four locations and three heading levels.

**Impact:** the section inventory required to be identical across the nine FEATURE docs is not; tooling that walks `### 1.5` misses five docs entirely.

**Fix:** promote the exclusion/applicability statement to one canonical numbered subsection in all nine docs.

---

### RVW-051 — The FEATURE back-reference priority markers contradict DECOMPOSITION
**Severity:** MAJOR · **Source:** cons1-6 · **Confidence:** high
**Location:** `gear-foundation.md:49`, `upstream-management.md:52`, `route-management.md:43`, `request-proxy.md:69`, `plugin-system.md:57`

**Evidence:** five docs stamp `p2` on the `cpt-cf-oagw-feature-*` reference line while `DECOMPOSITION.md` marks the same entries `p1`; the `featstatus` line additionally splits 8×`p1`/1×`p2` with no stated rule.

**Impact:** five entries declared `p1` (HIGH) in the canonical decomposition are back-referenced as `p2` by their own FEATURE docs.

**Fix:** set `p1` on the five reference lines and pick one rule for the `featstatus` marker.

---

### RVW-052 — The connection gauge is keyed by the request alias instead of the endpoint host
**Disposition:** PARTIALLY FIXED. (a) `send_once` now keys the guard and the pool counter on the resolved `endpoint.host` rather than the client-controlled alias, and the entry is removed from the `in_flight` map when its count reaches zero, so the map follows the hosts actually in use. The assertion in `tests/metrics_endpoint.rs` that keyed `oagw_upstream_connections{host}` on the alias encoded the defective labeling and has been rewritten to key on the endpoint host and to assert that the alias carries no series. (b) holding the guard for the whole SSE/WS relay lifetime is NOT FIXED: it needs a `'static` guard threaded through `ProxyResponse`, which changes the signature of five public methods across the relay path; the residual is that a streaming session reports zero active connections for its duration after the head is returned.

**Severity:** MAJOR · **Sources:** bfl3-8, bfl4-7, perf-3 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:1370-1378,1392` (with `:306-325`)

**Evidence:** `let _connection = ConnectionGuard::take(self, &context.alias);` then `report_pool(&self, host: &str, delta)` uses `self.pool.move_by(host, delta)` with `idle = self.pool_max_per_host - active`.

**Root cause:** three defects on the same gauge: (a) the guard/counter key is the addressed *alias* while the connection is opened to `endpoint.host:port`; (b) the guard is dropped when `send_once` returns the response head, so for an SSE relay the gauge reports 0 active for the whole session; (c) the `in_flight` map grows monotonically, is keyed by the un-normalized client-controlled alias string, and no code path ever removes an entry.

**Impact:** `oagw_pool_*` signals misreport load exactly where it matters most (long streaming sessions), and unbounded heap growth over the process lifetime amplified by attacker-controlled key length.

**Fix:** key the gauge on the resolved endpoint host (bounded cardinality), hold the guard for the relay lifetime, remove the entry when the count reaches zero, and bound the map.

---

### RVW-053 — The header default disagrees with the DTO and the FEATURE doc
**Severity:** MAJOR · **Source:** bfl12-8 · **Confidence:** high
**Location:** `src/domain/headers.rs:240`

**Evidence:** `let passthrough = request.and_then(|request| request.passthrough).unwrap_or(HeaderPassthrough::All);` while `dto.rs:215-220` declares `#[default] None` and `dto.rs:234-235` documents the field as "Default `none`", and `request-proxy.md` `inst-rp-al-header-5` requires "`none` forwards no inbound header".

**Root cause:** the pipeline substitutes `All` for the absent field instead of using the DTO's own default; `headers.rs:19-22` records the deviation as intentional.

**Impact:** with no `headers` configuration at all, every surviving inbound header — including `authorization` and `cookie` — is forwarded to the selected upstream, and the `allowlist` contract cannot be reached without an explicit `passthrough` value.

**Fix:** either change the fallback to `HeaderPassthrough::default()` or amend the DTO and the DoD to declare `all` the absent-form default; the two declarations must agree.

---

## C. MINOR

### RVW-054 — A non-UTF-8 header value is silently coerced to the empty string
**Severity:** MINOR · **Sources:** err-6, bfl12-10 · **Confidence:** medium/high
**Location:** `src/api/rest/proxy_handlers.rs:454-458`; `src/infra/proxy/service.rs:1464`

**Evidence:** `value.to_str().unwrap_or("").to_owned()`

**Root cause:** a non-UTF-8 header value is silently coerced instead of being rejected or preserved, and nothing is logged, on both the inbound request and the upstream response legs.

**Impact:** the exchange is silently mutated; the same silent rewrite also feeds `is_preflight`, `declared_length` and the trace-id fallback, and decisions taken on that header (CORS origin matching, allowlist matching) diverge from what was actually sent.

**Fix:** reject the request with `field_rejection(name, "the header value is not valid UTF-8")` or preserve the raw bytes, and log the substitution once.

---

### RVW-055 — Response headers that fail header-name/value parsing are dropped silently
**Severity:** MINOR · **Source:** err-7 · **Confidence:** medium
**Location:** `src/api/rest/proxy_handlers.rs:193-201,624-633`

**Evidence:** `if let (Ok(name), Ok(value)) = (HeaderName::try_from(...), HeaderValue::from_str(value)) { headers.append(name, value); }`

**Root cause:** headers that fail parsing are dropped with no `else` branch and no log, on both the passthrough and the error-branch paths.

**Impact:** an upstream response whose value falls outside visible ASCII is silently removed, so passthrough is not the unmodified forward the DoD promises and the omission is undiagnosable.

**Fix:** on a parse failure, `tracing::warn!` the header name and either percent-encode the value or record a counter.

---

### RVW-056 — An out-of-range upstream status is silently rewritten to 500
**Severity:** MINOR · **Source:** err-8 · **Confidence:** medium
**Location:** `src/api/rest/proxy_handlers.rs:211-212`

**Evidence:** `let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);`

**Root cause:** an out-of-range upstream status is silently rewritten to 500 with no log and no problem body.

**Impact:** a malformed status reaching the render boundary is reported to the client as a 500 that carries the upstream's headers and body under a forged status, with no attribution and no operator signal.

**Fix:** on `Err`, log and render `DomainError::ProtocolError`/`DownstreamError` through `ProblemDetails`.

---

### RVW-057 — Handler-raised proxy errors omit the `path` occurrence member
**Severity:** MINOR · **Source:** err-9 · **Confidence:** high
**Location:** `src/api/rest/proxy_handlers.rs:554-558` (with `error.rs:1011`, `:520-522`)

**Evidence:** `ApiError::into_response` always passes `&ErrorOccurrence::default()`, so no request-scoped value reaches the body; `ErrorOccurrence::at_path` has no caller in the crate.

**Root cause:** the completing layer fills `instance` and `trace_id` only, never `path`.

**Impact:** the 413 and the two 400 validation bodies rendered by the proxy handler omit the `path` member the contract requires, so the same failure renders with `path` when the data plane raises it and without it when the handler does.

**Fix:** build the site occurrence in `dispatch` and pass it to `ProblemDetails::from_error` instead of `ErrorOccurrence::default()`.

---

### RVW-058 — Internal authorization-failure text is published on the public error surface
**Severity:** MINOR · **Source:** err-10 · **Confidence:** medium
**Location:** `src/api/rest/error.rs:1003-1009`

**Evidence:** `Self::Authorization(AuthorizeError::Unavailable { detail }) => { stamp_source(CanonicalError::internal(format!("the authorization decision could not be obtained: {detail}")).create().into_response()) }`

**Root cause:** the PDP/enforcer error text is handed straight to the caller as the 500 body, with no `tracing` record at the rendering site and no truncation or redaction.

**Impact:** internal authorization-infrastructure failure detail is published on the public error surface and the internal error is not logged where it is rendered.

**Fix:** log the enforcer error with `tracing::error!` at the render site and render a fixed client detail.

---

### RVW-059 — The collect-failure branch leaves a stale `content-length` over an empty body
**Disposition:** FIXED — the collect-failure branch now drops `content-length`, `content-type` and `transfer-encoding` before installing the empty body, so the framing headers can no longer advertise bytes the response cannot deliver.
**Location:** `src/api/rest/error.rs:1187-1191`

**Evidence:** `let Ok(bytes) = http_body_util::BodyExt::collect(body).await … else { *response = Response::from_parts(parts, Body::empty()); return; };`

**Root cause:** the branch returns an empty body while leaving `parts.headers` untouched.

**Impact:** if reached, the client reads a truncated/empty response against the original length header — protocol corruption introduced by the error path itself.

**Fix:** strip or rewrite `content-length` before installing `Body::empty()`, and log the cause.

---

### RVW-060 — The last-resort problem body hardcodes `status: 500`
**Disposition:** FIXED — `FALLBACK_BODY` is replaced by `fallback_body(status)`, which renders the `status` member from the status code the response actually carries, so serialization failure can no longer emit a disagreeing pair.
**Location:** `src/api/rest/error.rs:867-873,906`

**Evidence:** `const FALLBACK_BODY: &str = r#"{"type":…,"status":500,…}"#;` with `let status = StatusCode::from_u16(self.status).unwrap_or(…)`.

**Root cause:** the fallback body is a hand-written literal that hardcodes `"status":500` and restates the canonical internal type instead of deriving both from the mapped error.

**Impact:** if serialization ever fails, the emitted body's `status` member disagrees with the HTTP status of the response.

**Fix:** build the fallback from the mapped values or emit a fixed 500 response whose status member and HTTP status are both 500.

---

### RVW-061 — The idle timer is armed at construction rather than at the first poll
**Severity:** MINOR · **Source:** bfl3-10 · **Confidence:** high
**Location:** `src/infra/proxy/stream.rs:97,136-144`

**Evidence:** `idle_timer: Box::pin(tokio::time::sleep_until(TokioInstant::now() + idle)),` with the in-poll comment "It is armed at the first poll and re-armed by every frame that arrives."

**Root cause:** the timer is armed at `relay()` construction time, so every nanosecond between `relay()` and the first poll is charged to the client's idle budget; the trailer branch also polls without re-arming.

**Impact:** a response head handed to a client that is slow to start reading can be immediately idle-aborted, reporting `IdleTimeout` for a session that never got a chance to open.

**Fix:** construct the `Sleep` lazily on the first poll and re-arm on non-data frames.

---

### RVW-062 — The relay records no terminal lifecycle event when the client drops the body
**Severity:** MINOR · **Source:** bfl3-9 · **Confidence:** high (code path) / medium (impact)
**Location:** `src/infra/proxy/stream.rs:128-177`

**Evidence:** `Poll::Ready(None) => { this.finished = true; this.lifecycle.record(StreamEvent::Close); … }` — no `impl Drop for RelayStream`, and no `Aborted`/`Close` record on the drop path; `ProxyResponse.lifecycle` is never read by production code.

**Root cause:** the relay records terminal state only inside `poll_next`; the most common way a proxy stream ends — the downstream client disconnects and axum drops the response body — runs no code on this type.

**Impact:** client-aborted SSE sessions are indistinguishable from sessions that never opened in the outcome record.

**Fix:** implement `Drop for RelayStream` recording `Aborted` when `!finished`, and wire the lifecycle into the observation the entry consumes.

---

### RVW-063 — Counters of a deleted resource survive in the registry
**Severity:** MINOR · **Source:** bfl3-6 · **Confidence:** high
**Location:** `src/infra/proxy/rate_limiter.rs:212-221`

**Evidence:** `pub fn release_resource(&self, resource: &RateLimitResource) -> usize` — no production caller.

**Root cause:** the seam is documented as "invoked by the Data Plane flush", but `DpHotConfig::flush`/`ObservabilityHook::observe_write` never call it, and neither does the upstream/route delete path.

**Impact:** counters of a deleted upstream or route survive until the 900 s idle sweep; the documented post-delete counter release never executes.

**Fix:** call `release_resource` from the delete and rate-limit-update write hook, or drop the unused method and its contract.

---

### RVW-064 — Storage `replace` bumps only the new alias key, and cascades skip route keys
**Severity:** MINOR · **Sources:** bfl3-7, qual-12 · **Confidence:** medium
**Location:** `src/infra/storage/mod.rs:513-518` (replace), `:297-310` (cascade), `:619-635` (route delete, for contrast)

**Evidence:** `tables.write_upstream(tenant_id, &record); tables.bump(&CacheKey::Upstream { alias: record.upstream.alias.clone() }.as_string());` — and `cascade_upstream` removes every child route without any `bump_route_keys`.

**Root cause:** `Tables` documents "every accepted write bumps the generation of every documented key the record affects", but `replace` bumps only the record's *new* alias and the cascade bumps no route keys.

**Impact:** a cache entry built against a pre-write key can be re-inserted and served after the write for any caller that renames an upstream or reads the route family through the DP cache.

**Fix:** in `replace`, bump the stored alias's key as well; in `delete`/`cascade_upstream`, collect the cascaded routes and call `bump_route_keys` for each.

---

### RVW-065 — A retried gear init leaves the health holder stuck in `Initializing`
**Severity:** MINOR · **Source:** bfl3-11 · **Confidence:** medium
**Location:** `src/gear.rs:350-352,626`

**Evidence:** `let health = Arc::new(HealthStateHolder::default()); health.initializing(); let _ = self.health.set(Arc::clone(&health));` … later `health.ready();`

**Root cause:** the holder is the only component published before the fallible steps and its `set` failure is deliberately discarded; on a partial-failure-then-retry path the retry builds a new holder while `self.health` still holds the first one in `Initializing`.

**Impact:** the gear reports `initializing` forever even though every state component is wired and `init` returned `Ok`, so `/readyz` never admits traffic.

**Fix:** reuse the published holder on retry (read it back when `set` fails).

---

### RVW-066 — A data-less frame run reschedules the same task immediately
**Severity:** MINOR · **Source:** perf-9 · **Confidence:** medium
**Location:** `src/infra/proxy/stream.rs:159-164`

**Evidence:** `None => { cx.waker().wake_by_ref(); Poll::Pending }`

**Root cause:** a trailer frame reschedules the same task immediately without blocking or bounding the number of consecutive data-less frames.

**Impact:** a hostile upstream that emits a continuous stream of data-less frames pins a tokio worker in a busy loop.

**Fix:** poll the body in a bounded loop inside `poll_next` and cap consecutive data-less frames before surfacing an abort.

---

### RVW-067 — Cache hits deep-clone the whole record the cache handed back as an `Arc`
**Severity:** MINOR · **Source:** perf-8 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:761-762`; `src/infra/dp_cache.rs:72-81`

**Evidence:** `if let Some(record) = cache.get_upstream(tenant_id, alias) { return Ok((*record).clone()); }`

**Root cause:** the cache hands back an `Arc` precisely so the record need not be copied, but the consumer immediately deep-clones the whole record to fit an owned `AliasLevel`.

**Impact:** every hot-path cache hit at every tenant level performs an allocation-and-copy proportional to the resolved configuration's size.

**Fix:** make `AliasLevel.record: Option<Arc<UpstreamRecord>>` so a hit is a refcount bump.

---

### RVW-068 — Per-request clones and identity re-rendering on the hot path
**Severity:** MINOR · **Source:** perf-10 · **Confidence:** medium
**Location:** `src/infra/proxy/service.rs:652-653,830-837`

**Evidence:** `let mut proxied = context.clone(); proxied.query = query;` and four to five avoidable `String` allocations per rate-limited request.

**Root cause:** structural clones stand in for the one field that changed and identity values are re-rendered to `String` several times per request.

**Impact:** per-request allocations proportional to the inbound header count, measurable at high RPS.

**Fix:** pass the query into `forward` separately instead of cloning `ProxyContext`; format the counter key once from borrowed pieces.

---

### RVW-069 — Three storage helper functions are dead weight with a second `match_type` derivation
**Severity:** MINOR · **Source:** qual-5 · **Confidence:** high
**Location:** `src/infra/storage/mod.rs:712-739`

**Evidence:** `pub fn match_rows(...)`, `pub fn method_rows(...)`, `pub const fn derived_match_type(...)` — no test or caller references them anywhere in the workspace.

**Impact:** three public functions in the storage namespace are dead weight, and `match_type` has two independent derivations.

**Fix:** delete them, or move the assertion the schema-contract test needs into that test.

---

### RVW-070 — `executable_strategy` encodes graded deviation 8 but nothing executes it
**Severity:** MINOR · **Source:** qual-6 · **Confidence:** high
**Location:** `src/domain/rate_limit.rs:683-688`; `dto.rs:415-417`; `merge.rs:517-520`; `list_query.rs:743-746`

**Evidence:** `pub const fn executable_strategy(strategy: RateStrategy) -> RateStrategy` whose only references are their own sibling unit tests.

**Root cause:** production-declared public API whose only references are tests; `CounterSpec::of` never consults it.

**Impact:** `rate_limit_tests.rs:453-455` asserts a mapping nothing executes, and `RateLimitConfig::refill_per_second` duplicates `CounterSpec::refill_per_second`.

**Fix:** call `executable_strategy` where the counter spec is built and delete the duplicate conversion helpers.

---

### RVW-071 — A placeholder validator that accepts everything is exported from the validation module
**Severity:** MINOR · **Sources:** qual-4, qual-7 · **Confidence:** high
**Location:** `src/domain/validation.rs:467-471` (orphaned doc at `:262-270`)

**Evidence:** `pub const fn validate_sharing_mode(field: &str, mode: SharingMode) -> Result<(), DomainError> { let _ = field; let _ = mode; Ok(()) }`

**Root cause:** a placeholder left in place of the real check; the orphaned doc block belonging to it was left behind ~200 lines above, concatenating onto `CORS_METHODS`.

**Impact:** the boundary-validation module publicly exports a validator named for a check it does not perform, and rustdoc presents `CORS_METHODS` as "Validate the sharing modes a record may declare".

**Fix:** delete `validate_sharing_mode` and the orphaned doc block.

---

### RVW-072 — The re-export doc names `DEFAULT_ENDPOINT_PORT` but re-exports `EndpointScheme`
**Severity:** MINOR · **Source:** qual-8 · **Confidence:** high
**Location:** `src/domain/validation.rs:874-876`

**Evidence:** `/// The default endpoint port, re-exported for callers that materialize an endpoint without an explicit port.` / `pub use crate::domain::dto::EndpointScheme;`

**Root cause:** the doc sentence describes one item while the item actually re-exported is another.

**Impact:** a caller looking for the documented port default imports the scheme enum and discovers the mismatch only at compile time.

**Fix:** re-export `DEFAULT_ENDPOINT_PORT` under that doc or rewrite the comment.

---

### RVW-073 — A value is bound only to be immediately discarded
**Severity:** MINOR · **Source:** qual-9 · **Confidence:** high
**Location:** `src/domain/validation.rs:184-197`

**Evidence:** `if let Some(other) = normalized.iter().find(|e| e.scheme != first.scheme) { let _ = other; return Err(…); }` (repeated for `port`)

**Root cause:** a leftover from converting a `find`-based check; the reader must verify `other` is genuinely unused.

**Impact:** the uniformity rule reads as though the offending endpoint were used for a better message and then deliberately thrown away.

**Fix:** `if normalized.iter().any(|e| e.scheme != first.scheme)` or use the bound value to name the offending index.

---

### RVW-074 — The canonical origin is computed twice and recovered with `expect`
**Severity:** MINOR · **Source:** qual-10 · **Confidence:** high
**Location:** `src/domain/cors.rs:204-213`

**Evidence:** `if canonical_origin(entry).is_some_and(|canonical| canonical == requested) { return OriginVerdict::Exact(canonical_origin(entry).expect("just serialized")); }`

**Root cause:** the canonical form is computed twice and the second result is recovered with an `expect` whose justification lives only in the reader's head.

**Impact:** every configured origin entry is parsed twice on the CORS path, and the branch carries a panic instead of a denial.

**Fix:** `if let Some(canonical) = canonical_origin(entry) { if canonical == requested { return OriginVerdict::Exact(canonical); } }`

---

### RVW-075 — The tenant-scoped fetch-then-check idiom is hand-copied across four sites
**Severity:** MINOR · **Source:** qual-11 · **Confidence:** high
**Location:** `src/infra/storage/mod.rs:497-520,522-540,598-617,619-635`

**Evidence:** `let exists = tables.upstream.get(&id).filter(|row| row.tenant_id == tenant_id).is_some(); if !exists { return Err(Tables::not_found("upstream")); } let alias = tables.upstream.get(&id).map(|row| row.upstream.alias.clone());`

**Root cause:** the idiom is hand-copied across `replace`/`delete` for both aggregates, and the delete paths re-fetch the row a second time after the existence check.

**Impact:** any change to the not-found rule must be re-applied in four places.

**Fix:** add one private helper per aggregate returning `(alias, row)` and use it from get/replace/delete.

---

### RVW-076 — The upgrade exemption short-circuits the whole strip set
**Severity:** MINOR · **Source:** sec-5 · **Confidence:** medium
**Location:** `src/domain/headers.rs:333-342`

**Evidence:** `let mut forwarded: Vec<(String, String)> = upstream.iter().filter(|(name, _)| upgrade || !is_stripped_from_response(name)).cloned().collect();`

**Root cause:** the upgrade exemption short-circuits the whole strip set, so a `101` head relays every upstream header — including the framing and hop-by-hop names the non-upgrade path removes.

**Impact:** upstream-controlled framing headers reach the client's handshake through `render`, so a malicious upstream can corrupt the `101` head framing or smuggle hop-by-hop state.

**Fix:** strip the framing set unconditionally and exempt only the handshake pair.

---

### RVW-077 — Configured `set`/`add` rules can reintroduce reserved hop-by-hop headers
**Severity:** MINOR · **Source:** sec-6 · **Confidence:** medium
**Location:** `src/domain/headers.rs:268-288` (with `validation.rs:593-627`)

**Evidence:** the configured `set`/`add` rules run after the strip stage, and `validate_headers` checks only the RFC 7230 grammar, length and CR/LF/NUL, never a reserved-name exclusion.

**Root cause:** the "the eight hop-by-hop headers, `host`, `content-length` and every `x-forwarded-*` never reach the upstream" invariant is enforced only in the passthrough stage.

**Impact:** a tenant `headers.request` rule can reintroduce `host`, `content-length` or `transfer-encoding` into the outbound set — request-confusion / smuggling surface.

**Fix:** re-apply `is_stripped_from_request` after the set/add/remove stage, and reject reserved names in `validate_headers` on the write path.

---

### RVW-078 — A type-mistaken guard configuration silently disables presence enforcement
**Severity:** MINOR · **Source:** sec-7 · **Confidence:** high
**Location:** `src/infra/plugin/required_headers_guard.rs:98-108`

**Evidence:** `match value { serde_json::Value::String(text) => parse_comma_list(text), serde_json::Value::Array(items) => …, _ => Vec::new() }`

**Root cause:** any `required_request_headers`/`required_response_headers` value that is neither a string nor an array of strings yields an empty name list, and `decide` treats an empty list as "unconfigured" and allows.

**Impact:** a type-mistaken guard configuration silently disables a presence-enforcement control; ADR 0009 documents fail-open only for absent and blank values.

**Fix:** reject a present key of the wrong JSON type at the configuration boundary, or have `decide` return a reject/validation error for a present-but-unparsable key.

---

### RVW-079 — Endpoint hosts admit internal address classes
**Severity:** MINOR · **Source:** sec-7b · **Confidence:** medium
**Location:** `src/domain/validation.rs:87-94`

**Evidence:** `if host_is_ip(trimmed) { return Ok(trimmed.to_ascii_lowercase()); }` — the host validator accepts any IP literal — loopback, RFC 1918, link-local `169.254.169.254`, ULA, IPv6 `::1` — with no internal-address class excluded.

**Root cause:** neither `select_endpoint` nor `upstream_url` restricts the dialed authority beyond that.

**Impact:** a tenant can point an endpoint pool at the gateway's own loopback/management plane, the `credstore` service, or a cloud metadata service, and the data plane dials it from the gateway's network position while forwarding the caller's headers.

**Fix:** add a configuration-gated denylist (loopback, link-local, unique-local, and the instance's own addresses) resolved at connection time, or document explicitly that endpoint hosts are operator- rather than tenant-chosen. Note: the graded configuration sets `ssrf_policy.enabled: false`, so this is a documented-posture deviation rather than a live defect.

---

### RVW-080 — `canonical_origin` drops path, query, fragment and userinfo
**Severity:** MINOR · **Source:** bfl4-5 · **Confidence:** medium
**Location:** `src/domain/cors.rs:142-159`

**Evidence:** `canonical_origin` serializes only scheme/host/port and ignores path/query/fragment/userinfo, while the write-side check requires `path == "/"` and empty credentials for a *configured* entry.

**Root cause:** the two sides of the comparison are not held to the same shape.

**Impact:** a request `Origin` of `https://user:pass@app.example.com/x` canonicalizes to `https://app.example.com` and is granted an allow-origin echo of the bare origin.

**Fix:** return `None` from `canonical_origin` when path/query/fragment/credentials are present.

---

### RVW-081 — A client-controlled query parameter name is echoed unbounded into the error detail
**Disposition:** FIXED by omission — the rejection no longer interpolates the client-controlled name at all, which is what the module's own contract requires ("the only request value ever echoed is `X-OAGW-Target-Host`, never into `detail`"); the message names the position instead. No test pinned the old spelling.
**Location:** `src/domain/route_matcher.rs:254-260`

**Evidence:** `return Err(DomainError::field_rejection("query", &format!("query parameter \`{name}\` is not in the allowlist")));`

**Root cause:** the client-controlled name is interpolated unbounded into the error detail, while the module contract states the only request value ever echoed is `X-OAGW-Target-Host` through `bound_invalid_value`.

**Impact:** a request with a multi-kilobyte unknown query name is reflected verbatim into the rendered 400 body.

**Fix:** pass the name through `bound_invalid_value` (and strip control characters) before formatting, or omit it.

---

### RVW-082 — `upgrade::relay` populates `upstream_id` with the endpoint host
**Severity:** MINOR · **Source:** bfl4-8 · **Confidence:** high
**Location:** `src/infra/proxy/upgrade.rs:176-185`

**Evidence:** `DomainError::DownstreamError { upstream_id: Some(endpoint.host.clone()), host: Some(endpoint.host.clone()), … }`

**Root cause:** the `upstream_id` field of the downstream-error shape is populated with the endpoint host instead of the resolved upstream identifier; `relay()` never receives the upstream id.

**Impact:** a refused WebSocket session is attributed to the wrong field.

**Fix:** thread `upstream_id: &str` into `upgrade::relay` and use it here.

---

### RVW-083 — Alias derivation assumes a uniform endpoint pool without enforcing it
**Severity:** MINOR · **Source:** bfl4-9 · **Confidence:** medium
**Location:** `src/domain/alias.rs:174-178`

**Evidence:** `let pool_port = endpoints[0].port; let scheme = endpoints[0].scheme;` with the comment "The pool is uniform in port (validated at write time)".

**Root cause:** the derivation reads only the first endpoint's scheme/port on the assumption of pool uniformity, but nothing in this function enforces it; the enforcement lives in a separate `validate_server` step other call paths can bypass.

**Impact:** for a non-uniform pool the derived/classified alias is computed from one endpoint's port and scheme, so `derive_alias`, `alias_shape` and the alias-immutability matrix can all decide from the wrong port.

**Fix:** call `require_uniform_pool(server)` at the top of `try_derive_alias`.

---

### RVW-084 — Ancestor scalar layers hardcode `Inherit`, bypassing the permission gate
**Severity:** MINOR · **Source:** bfl4-10 · **Confidence:** low
**Location:** `src/domain/merge.rs:443-464` via `src/infra/proxy/service.rs:1266-1273`

**Evidence:** `sharing: crate::domain::merge::LayerSharing { scalars: Sharing::Inherit, … }` in `upstream_base_layer`, reused by `config_layers` for every shadowed ancestor.

**Root cause:** `Upstream` carries no sharing declaration for the scalar group, so `upstream_base_layer` hardcodes `Inherit`; because ancestor layers sit at the end of the ordered layer list, `inst-gf-merge-12` then gives them unconditional priority with no permission gate.

**Impact:** with alias shadowing, a shadowed ancestor upstream's `headers` block replaces the selected upstream's header rules, applied with no `enforce` declaration and no permission gate.

**Fix:** in `config_layers`, set `layer.sharing.scalars = Sharing::Private` for ancestor layers.

---

### RVW-085 — Preflight CORS consults the store and the security context against the DoD
**Severity:** MINOR · **Source:** bfl12-7 · **Confidence:** medium
**Location:** `src/infra/proxy/service.rs:941-944` (from `proxy_handlers.rs:495-501`)

**Evidence:** `let cors = alias.zip(tenant).and_then(|(alias, tenant)| self.upstreams.get_by_alias(tenant, alias).ok()).and_then(|record| record.upstream.cors)`

**Root cause:** the preflight call-in consults the upstream repository and the security context, while the DoD requires answering it "performing no upstream resolution, no tenant-context lookup, and no plugin-chain execution". The lookup is also un-normalized and chain-unaware.

**Impact:** for a preflight whose alias resolves only through the tenant chain or is spelled non-canonically, the `204` is emitted without the credential-bearing decoration, so browsers refuse the credentialed cross-origin request the actual-request leg would serve.

**Fix:** answer the preflight purely from the request headers as the DoD specifies, or resolve through the same normalized, chain-aware resolver the actual request uses.

---

### RVW-086 — `Transfer-Encoding: chunked, gzip` is admitted
**Disposition:** FIXED — the inner `any` became `all`, so every token of every `Transfer-Encoding` value must be `chunked`; `chunked, gzip` is now rejected. Covered by `a_transfer_encoding_naming_chunked_and_another_coding_is_rejected`.
**Location:** `src/infra/proxy/body_validation.rs:104-108`

**Evidence:** `let chunked = values.iter().all(|value| { value.split(',').any(|token| token.trim().eq_ignore_ascii_case("chunked")) });`

**Root cause:** the outer `all` combined with the inner `any` accepts a list that contains `chunked` *plus* another coding, while the function's own contract says it errors "when `Transfer-Encoding` names anything but `chunked`".

**Impact:** a `chunked, gzip` request is admitted; the transfer coding is stripped as hop-by-hop and the decoded-but-still-compressed bytes are re-framed with a computed length and forwarded with no `Content-Encoding`.

**Fix:** require every token of every value to be `chunked`.

---

### RVW-087 — The plugin-fallback hook invents the operation, principal and status of a `config_change` record
**Severity:** MINOR · **Source:** obs-9 · **Confidence:** medium
**Location:** `src/infra/observability.rs:275-289`

**Evidence:** `on_config_written` forwards to `on_upstream_written(WriteNotification { event: "upstream.replace", principal_id: Uuid::nil(), status: 200, outcome: "accepted" })`

**Root cause:** the pre-2.9 fallback entry point receives no operation, principal or status, so the hook invents all three.

**Impact:** any caller reaching the fallback emits a record that names the wrong operation, a nil principal and a fabricated `status: 200`.

**Fix:** remove the override so the fallback cannot manufacture an accepted-write record.

---

### RVW-088 — Audit field truncation and control-character deletion are unmarked
**Severity:** MINOR · **Source:** obs-10 · **Confidence:** medium
**Location:** `src/infra/audit.rs:451-454,459-475`

**Evidence:** `fn bounded_path(path: &str) -> String { … .chars().take(512).collect() }` and `control if (control as u32) < 0x20 => {}`

**Root cause:** the mitigations alter field values in place, with no marker that a value was truncated or a character dropped.

**Impact:** an audit line can carry a `path` that never existed verbatim, so a forensic join with the problem body's `trace_id` can silently fail.

**Fix:** keep the bound but append a deterministic truncation marker, and emit unmapped control characters as `\u00XX` escapes.

---

### RVW-089 — The in-flight gauge feeds the OpenTelemetry instrument a ±1 delta instead of the concurrency
**Severity:** MINOR · **Source:** obs-7 · **Confidence:** medium
**Location:** `src/infra/metrics.rs:314-325,383-390`

**Evidence:** `enter_in_flight(..., 1.0)` / `leave_in_flight(..., -1.0)` and `gauge.record(value, &attributes)`.

**Root cause:** `oagw_requests_in_flight` is declared a gauge but the accumulator path is additive, so the OTel gauge is fed the ±1 delta instead of the accumulated concurrency, and `add` casts `value as u64`.

**Impact:** the semantic-convention surface exports `oagw_requests_in_flight` as 1/-1 samples rather than the current concurrency.

**Fix:** record the current accumulated value on the gauge and reserve `add` for counter families.

---

### RVW-090 — The OpenTelemetry histogram inherits SDK default bucket boundaries
**Severity:** MINOR · **Source:** obs-8 · **Confidence:** medium
**Location:** `src/infra/metrics.rs:296-306`

**Evidence:** `meter.f64_histogram(family).with_description(…).with_unit("s".to_owned()).build()` — the declared bucket set is applied only to the local accumulator.

**Root cause:** the OTel histogram is built without an explicit boundary view.

**Impact:** once a meter provider is installed, `oagw_request_duration_seconds` exports different bucket boundaries than `/metrics`.

**Fix:** attach a view that sets `DURATION_BUCKETS` as the boundaries for that instrument.

---

### RVW-091 — The health state machine omits the two documented transitions
**Severity:** MINOR · **Source:** obs-6 · **Confidence:** medium
**Location:** `src/infra/health.rs:85-93,250-258`

**Evidence:** `admits` omits `initializing → uninitialized` and `ready → uninitialized`; `HealthState::Uninitialized | Initializing => HealthcheckResult::degraded("observability and state surface is still wiring")`.

**Root cause:** the machine omits the two documented transitions, and `gear.rs` deliberately leaves the holder at `initializing` when a later init step fails.

**Impact:** a failed or discarded wiring is indistinguishable from wiring in progress.

**Fix:** admit the two transitions, or record a distinct failure flag so `check()` returns unhealthy with a code such as `oagw_init_failed`.

---

### RVW-092 — Five gear accessors and the plugin-CRUD facade have no callers
**Severity:** MINOR · **Sources:** eng-11, test-13 · **Confidence:** high
**Location:** `src/gear.rs:252-256,271-284,304-315`

**Evidence:** `management()`, `plugin_management()`, `plugin_runtime()`, `audit()`, `observability()` have zero callers.

**Impact:** speculative public surface on the crate's exported `OagwGear`; it lets any future consumer reach past the domain seams and the authorization gates straight to the plugin runtime, the audit sink or the observability recorders.

**Fix:** demote the five unused accessors to `pub(crate)` (or delete them) and add accessors when a consumer appears.

---

### RVW-093 — The composition root re-instantiates stateless adapters and repeats the publication guard 13 times
**Severity:** MINOR · **Source:** eng-10 · **Confidence:** high
**Location:** `src/gear.rs:454-457,467-470,478-481,520-526,531-533,576-614`

**Evidence:** four separate `AuthzManagementAuthorizer::new` calls, three `CatalogBindingResolver::new` calls, and `map_err(|_| anyhow::anyhow!("{} module already initialized", …))` 13 times.

**Root cause:** the composition root re-instantiates stateless port adapters per consumer and repeats the same publication guard as a copy-paste block.

**Impact:** any per-instance state or policy added to those adapters is silently not shared; the publication sequence cannot be extended without copying the `map_err` line again.

**Fix:** bind each adapter once and clone the `Arc`; extract a `publish` helper for the 13 `set` calls.

---

### RVW-094 — The three sibling aggregates import each other's port definitions
**Severity:** MINOR · **Source:** eng-12 · **Confidence:** medium
**Location:** `management.rs:45-46`; `route_management.rs:53-57`; `plugin_management.rs:47-50`

**Evidence:** `management` imports ports that live inside `route_management` and `plugin_management`, and both of those import from `management`.

**Root cause:** the plugin-catalog ports have no neutral home.

**Impact:** the aggregates are not independent units as their module docs claim; a rename forces changes across the cycle.

**Fix:** move `PluginBindingResolver`, `PluginConfigValidator`, `Actor`, `ManagementError`, `ConfigWriteHook`/`Notification` into a shared seam module.

---

### RVW-095 — One concrete type satisfies two unrelated role interfaces, instantiated once per role
**Severity:** MINOR · **Source:** eng-13 · **Confidence:** medium
**Location:** `src/infra/plugin/mod.rs:57-117,129-187`; `gear.rs:524-533`

**Evidence:** `impl PluginConfigValidator for CatalogBindingResolver` and `impl PluginBindingResolver for CatalogBindingResolver`, constructed twice as two independent instances.

**Root cause:** one concrete infra type is made to satisfy two unrelated role interfaces with two different rejection helpers and two different not-found conventions.

**Impact:** the two contracts are coupled in one type.

**Fix:** split into `CatalogBindingResolver` and `CatalogConfigSchema`, sharing one private `resolvable(tenant_id, reference)` helper.

---

### RVW-096 — Permission-string → resource-id derivation is copy-pasted with a silent degrade
**Severity:** MINOR · **Source:** eng-8 · **Confidence:** high
**Location:** `management.rs:746-752`; `route_management.rs:225-231`

**Evidence:** `let resource = format!("{UPSTREAM_BASE_TYPE}:", permission.rsplit(':').next().unwrap_or(""));` — the segment extraction silently degrades to an empty resource id.

**Root cause:** the derivation is copy-pasted between the two aggregates with only the base-type constant differing, and a permission constant without `:` produces `...v1~:`.

**Impact:** a change to the resource-id scheme must be edited in every aggregate; a malformed constant is evaluated against a resource no policy names, with no error.

**Fix:** move the derivation into `domain/gts_helpers` as `resource_id_for(base_type, permission)` that rejects a permission with no action segment.

---

### RVW-097 — Client-visible conflict detail strings are inlined at each construction site
**Severity:** MINOR · **Source:** eng-9 · **Confidence:** high
**Location:** `services/mod.rs:121-131`; `management.rs:162-178`; `route_management.rs:284-288`

**Evidence:** `"another enabled route of this upstream already matches this method, path prefix and priority"` appears verbatim at two sites; `"plugin binding positions must be contiguous from zero"` at two sites.

**Root cause:** error detail strings are inlined at each construction site instead of being named constants.

**Impact:** a wording change silently produces two different conflict details depending on which aggregate served the request.

**Fix:** define the detail texts once — `map_conflict(WriteConflict::RouteMatch)` should call `ManagementError::route_match_conflict()`, and hoist the positions text into a `const`.

---

### RVW-098 — The list-query pipeline erases the record type and downcasts back with `unreachable!`
**Severity:** MINOR · **Source:** qual-2 · **Confidence:** high
**Location:** `src/domain/list_query.rs:288-316`

**Evidence:** `apply` and `apply_routes` downcast with `unreachable!("an upstream query yields upstream records")`, while `apply_plugins` handles the identical situation with a silent `filter_map`.

**Root cause:** `apply_records` erases the record type into `Vec<QueryRecord>` and the callers downcast it back.

**Impact:** any future change that lets a foreign variant reach the mapped vec converts a latent type mismatch into a request-path panic.

**Fix:** make the pipeline generic over a small `QueryRecord` trait.

---

### RVW-099 — `merge` holds 8 accumulators, 9 per-field blocks and 5 levels of nesting
**Severity:** MINOR · **Source:** qual-3 · **Confidence:** medium
**Location:** `src/domain/merge.rs:232-415`

**Evidence:** every one of the nine mergeable fields re-implements the same three-part gate inline (visibility → enforce → permission/base/none).

**Root cause:** the ~35 decision points accumulate in one body rather than behind a per-field abstraction.

**Impact:** a change to the gating order must be re-derived in each block, and a reviewer cannot confirm that `cors` and `plugins` treat `enforce` identically without diffing two 25-line blocks.

**Fix:** extract one helper per field family taking the accumulated slot plus the layer context.

---

### RVW-100 — No test drives a pool of two or more live endpoints
**Severity:** MINOR · **Source:** test-1 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:1327-1368`; `tests/proxy_streaming.rs:75-109`

**Evidence:** `forward_buffered` builds `pool = once(endpoint).chain(failover)`; every proxy integration fixture seeds a one-endpoint pool, so both failover branches and the data-plane wiring of `EndpointSelector` are unreached by tests.

**Impact:** a regression that retries a timed-out request against a sibling endpoint, or that fails to fail over on a connection refusal, or that pins the cursor to index 0, would pass the whole suite.

**Fix:** add an integration test seeding a two-endpoint pool (first on a closed port, second live) asserting the response is served from the second; add a sibling with two never-answering listeners asserting the accept count is 1.

---

### RVW-101 — The connection gauge is observed only at its rest state
**Severity:** MINOR · **Source:** test-2 · **Confidence:** high
**Location:** `tests/metrics_endpoint.rs:207-215`

**Evidence:** `assert_eq!(active, Some(0.0), …)` and `assert_eq!(idle, max, …)` — no assertion anywhere that `active` ever reached 1 while the exchange was in flight. `PoolUsage::move_by` saturates at 0, so an implementation whose take-side increment is missing yields exactly the asserted pair.

**Impact:** removing the take-side increment, or breaking `report_pool`'s `idle = max - active` derivation, passes the suite.

**Fix:** add a `PoolUsage` unit test covering `+1`, `+1 then -1`, and `-1` from 0, and an observer test asserting `active=1` was seen in flight.

---

### RVW-102 — The root-route branch of the prefix matcher has no test
**Severity:** MINOR · **Source:** test-3 · **Confidence:** high
**Location:** `src/domain/route_matcher.rs:96-99`; `route_matcher_tests.rs:41-53`

**Evidence:** `if prefix.is_empty() { return request_path == "/" || request_path.is_empty(); }` — no test seeds a route with `"path": "/"`.

**Impact:** the root route, the one prefix that matches the proxy surface itself, is the least-tested match rule.

**Fix:** add `path_matches` cases for a root route and one integration test seeding a `"/"` route.

---

### RVW-103 — The `max_body_size_bytes == 0` branch and the `SsrfPolicy` rejection path are untested
**Severity:** MINOR · **Source:** test-4 · **Confidence:** high
**Location:** `src/config.rs:127-133`; `config_tests.rs:54-60`

**Evidence:** `non_positive_integers_are_rejected` iterates `["proxy_timeout_secs", "token_cache_ttl_secs", "token_cache_capacity"]` and omits `max_body_size_bytes`; no test passes a non-`"disabled"` `ssrf_policy` value.

**Impact:** a `max_body_size_bytes` of 0 would be accepted at init and then make every proxied request 413.

**Fix:** add `"max_body_size_bytes"` to the key list and a case asserting a strict `ssrf_policy` is rejected.

---

### RVW-104 — The WebSocket lifecycle and the `connection: upgrade` header are asserted nowhere
**Severity:** MINOR · **Source:** test-5 · **Confidence:** high
**Location:** `src/infra/proxy/upgrade.rs:138-160`; `tests/proxy_streaming.rs:151-194`; `upgrade_tests.rs:42-100`

**Evidence:** `relay` records `Refused`/`Open`/`Close`/`Aborted` on four exit paths; `grep -rn 'StreamEvent\|lifecycle' tests/` returns only a doc-comment mention.

**Impact:** a regression that records nothing, or records `Open` for a refused session, or drops `Connection` from the 101 head, passes the suite.

**Fix:** assert the lifecycle events after awaiting the client upgrade future, and assert the 101 head carries `connection: upgrade`.

---

### RVW-105 — The `HierarchyPermissions` seam is unwired and untestable
**Severity:** MINOR · **Source:** test-6 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:252-268,465-473`

**Evidence:** `pub trait HierarchyPermissions` with its only implementation `NoHierarchyPermissions` returning `OverridePermissions::NONE`; neither is referenced by `gear.rs`, `test_support.rs`, or any file under `tests/`.

**Impact:** distinct from the NONE-merge defect: even after that defect is fixed there is no test that a caller holding override permissions sees a different effective configuration on the proxy path.

**Fix:** add a data-plane test that installs a `HierarchyPermissions` returning `ALL`, or delete the unused builder and trait.

---

### RVW-106 — The config/fixture block is copy-pasted 15 times across the integration suite
**Severity:** MINOR · **Source:** test-7 · **Confidence:** high
**Location:** 15 copies across `tests/*.rs`, plus 11 drifting `upstream_body` definitions and two incompatible `seed_upstream` helpers

**Evidence:** `fn proxy_config() -> Option<serde_json::Value> { … }` appears 15 times; `seed_upstream` in `route_crud.rs:29` (async/3-arg) shadows `oagw::test_support::seed_upstream` (sync/2-arg) which the same file imports.

**Root cause:** `test_support.rs` is already the shared fixture module but the config block and the seeded-surface helper were never hoisted into it.

**Impact:** a change to the documented default body limit or timeout requires 15 coordinated edits; the shadowed name is a reading trap.

**Fix:** add `proxy_config()` and `seeded_proxy_surface(...)` to `test_support.rs` and delete the 15 local copies.

---

### RVW-107 — The two gate test modules re-derive identical plumbing
**Severity:** MINOR · **Source:** test-8 · **Confidence:** high
**Location:** `src/infra/proxy/cors_gate_tests.rs:25-49`; `rate_limit_gate_tests.rs:27-55`

**Evidence:** byte-identical `AllowAll`, `NoAncestors` and `limits()` in both files.

**Impact:** a change to `DataPlaneLimits`'s field set must be edited in both files and will drift.

**Fix:** move them into `test_support.rs` or a shared `gate_fixture` module.

---

### RVW-108 — The cache-invalidation test reaches into private cache internals
**Severity:** MINOR · **Source:** test-9 · **Confidence:** high
**Location:** `tests/cache_invalidation.rs:50-52,77,83,104,111`

**Evidence:** `state.l1.contains(&key)` four times plus `assert_eq!(CP_L1_CAPACITY, 10_000, …)`.

**Root cause:** the test reconstructs the private key layout and the L1 map's membership rather than observing the read-after-write outcome.

**Impact:** renaming a `CacheKey` variant field breaks the test with no behavior change.

**Fix:** keep the behavior test and drop the internals assertions.

---

### RVW-109 — The 405 `allow` header accumulation across levels has no test
**Severity:** MINOR · **Source:** test-10 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:1155-1172`

**Evidence:** `RouteMatchOutcome::MethodNotAllowed { allowed } => { not_allowed.get_or_insert(allowed); }` — every `allow` header assertion comes from a single-level setup.

**Impact:** if the walk order were reversed, or `get_or_insert` replaced by `insert`, the `allow` header a descendant caller receives would name the ancestor's methods.

**Fix:** add a test seeding path-matching routes on two levels with disjoint method sets.

---

### RVW-110 — `$orderby` for the plugin and route field sets is unexercised
**Severity:** MINOR · **Source:** test-11 · **Confidence:** high
**Location:** `src/domain/list_query.rs:418-428,453-466`; `tests/plugin_management_api.rs:92-132`; `tests/route_list_query.rs:43-65`

**Evidence:** `compare_plugin_field` and `compare_route_field` are named by no test file; `plugin_management_api.rs` issues no `$orderby` at all.

**Impact:** a wrong field name silently falls into the `_ => Ordering::Equal` arm, so `$orderby=name` on plugins returns insertion order with a 200 response.

**Fix:** add `$orderby=name`/`plugin_type` and a two-record tiebreak assertion.

---

### RVW-111 — The fail-closed posture for a runtime-less data plane is unreachable from tests
**Severity:** MINOR · **Source:** test-12 · **Confidence:** high
**Location:** `src/infra/proxy/service.rs:1020-1034`

**Evidence:** every integration test reaches the data plane through `OagwGear::init`, which always calls `with_plugin_runtime`, so the branch is unreachable from `tests/`.

**Impact:** the fail-closed guarantee for a data plane without a plugin runtime is asserted nowhere; deleting the `Err` arm would pass the suite.

**Fix:** add an inline test that builds `DataPlaneServiceImpl::new(...)` without `with_plugin_runtime` and asserts `PluginNotFound`.

---

### RVW-112 — The suffix-disabled trailing-slash boundary is unpinned
**Severity:** MINOR · **Source:** test-14 · **Confidence:** medium
**Location:** `src/domain/route_matcher.rs:122-130,196-200`; `tests/proxy_dispatch.rs:260-285`

**Evidence:** `suffix_of` returns `request_path[prefix.len()..].trim_start_matches('/')`, so for a suffix-disabled route the request `/v1/orders/` yields an empty suffix and is let through while `/v1/orders/x` is rejected; no test pins the trailing-slash-only request.

**Impact:** the accepted/rejected boundary for a suffix-disabled route is undocumented by the suite.

**Fix:** add `suffix_of` and `select` cases plus one integration case.

---

### RVW-113 — Five FEATURE-doc path references are ambiguous about their base
**Severity:** MINOR · **Source:** cons1-9 · **Confidence:** medium
**Location:** `route-management.md:122,187,495,504,557`; `upstream-management.md:691`

**Evidence:** "validate … against the shapes of `docs/schemas/route.v1.schema.json`" — the files are at `gears/system/oagw/docs/schemas/`; `/app/docs/schemas/` does not exist.

**Impact:** the six references are the authority for payload validation and the path does not resolve from the repository root the sibling references use.

**Fix:** spell the path monorepo-root-relative.

---

### RVW-114 — The observability entry uses a third slug with no aliasing statement
**Severity:** MINOR · **Source:** cons1-7 · **Confidence:** high
**Location:** `docs/features/observability-and-state.md:1,58,61`

**Evidence:** the entry is the only one of nine whose concept-id slug (`observability-and-operability`) does not match its document slug (`observability-and-state`), and it additionally uses a third slug in its status id.

**Impact:** 43 references cannot be traced to the document by slug.

**Fix:** rename the id, or add a one-line aliasing note at both DECOMPOSITION.md:643 and the feature doc.

---

### RVW-115 — The optional-path-suffix brackets are dropped in one acceptance criterion
**Severity:** MINOR · **Source:** cons1-8 · **Confidence:** high
**Location:** `docs/features/request-proxy.md:1013`

**Evidence:** "- [x] A proxy request to `/oagw/v1/proxy/{alias}/{path_suffix}` …" against the canonical spelling used 49 times elsewhere.

**Impact:** the criterion reads as asserting a route that requires a path suffix.

**Fix:** change to `/oagw/v1/proxy/{alias}[/{path_suffix}]`.

---

### RVW-116 — Two docs drop the blank separator before `## 1. Feature Context`
**Severity:** MINOR · **Source:** cons1-13 · **Confidence:** high
**Location:** `plugin-system.md:57-58`; `request-proxy.md:69-70`

**Evidence:** the list item is immediately followed by the heading with no blank line.

**Impact:** the nine documents' structural inventory differs at the same position.

**Fix:** insert a blank line.

---

### RVW-117 — `cors.md` restates three graded deviations without the label
**Severity:** MINOR · **Source:** cons1-14 · **Confidence:** high
**Location:** `docs/features/cors.md:519,525,545,562`

**Evidence:** `cors.md` contains zero occurrences of "deviation" or "graded", yet it restates the substance of deviations 1, 3 and 4 without the label; the other eight docs cite the numbers.

**Impact:** a reader tracing `cors.md`'s route-tree, test-placement or e2e statements back to the override register finds no citation, and the DECOMPOSITION promise cannot be verified mechanically for entry 2.8.

**Fix:** add the deviation labels to the existing sentences.

---

### RVW-118 — The noun list and the entry-number list are positionally misaligned
**Severity:** MINOR · **Source:** cons1-2 · **Confidence:** high
**Location:** `docs/features/gear-foundation.md:362`

**Evidence:** "handler bodies for upstreams, routes, plugins, and proxy requests belong to entries 2.2, 2.3, 2.4, and 2.6" — read in parallel, the text assigns the plugin handler to 2.4 (Request Proxy) and the proxy handler to 2.6 (Plugin System).

**Impact:** misattributes ownership of the proxy handler and the plugin management handlers on the REST-registration DoD.

**Fix:** reorder to "upstreams, routes, proxy requests, and plugins".

---

### RVW-119 — `this entry` and `this feature` alternate as the subject of normative sentences
**Severity:** MINOR · **Source:** cons1-11 · **Confidence:** medium
**Location:** all nine FEATURE docs (29/14, 6/10, 5/9, 5/15, 2/23, 2/17, 1/20)

**Evidence:** the same document titles itself "# Feature: …" and says "this feature supplies the behavior behind those steps" while elsewhere writing "The entry MUST NOT implement any endpoint business logic".

**Impact:** a reader mapping "entry" to the decomposition candidate and "feature" to the artifact cannot tell what a sentence scopes to.

**Fix:** pick one self-reference for the FEATURE docs, reserving "entry N.M" for cross-references.

---

### RVW-120 — The ADR/Schemas reference block varies in label and presence
**Severity:** MINOR · **Source:** cons1-10 · **Confidence:** high
**Location:** `cors.md:90`, `error-handling.md:84`, `rate-limiting.md:91`, `plugin-system.md:101`, `observability-and-state.md:103`

**Evidence:** plural "**ADRs**:" carrying exactly one ADR in `cors.md`; singular for one ADR in `error-handling.md` and `rate-limiting.md`; four docs have no ADR line even though their prose cites ADRs; three docs have no Schemas line.

**Root cause:** the §1.4 reference block uses two label spellings for the same element and includes/excludes the ADR and Schemas lines per doc without a stated rule.

**Impact:** a reader collecting "which ADRs does this entry realize" misses the citations in four docs.

**Fix:** standardize on "- **ADRs**:" with one entry per ADR and add the missing lines.

---

### RVW-121 — `plugin-system.md` omits `$orderby` from the list-parameter enumeration
**Severity:** MINOR · **Source:** cons3-11 · **Confidence:** high
**Location:** `docs/features/plugin-system.md:165,722`

**Evidence:** `GET /oagw/v1/plugins` is documented with `$filter`, `$select`, `$top`, `$skip` while the shared `parse_plugin` path supports `$orderby` and DESIGN §3.3 lists it.

**Impact:** a client cannot discover that `$orderby=name` is legal, and the four list endpoints are documented with two different parameter sets for one shared parser.

**Fix:** add `$orderby` to both parameter enumerations.

---

### RVW-122 — The route doc conflates the minted identifier form with the accepted path form
**Severity:** MINOR · **Source:** cons3-13 · **Confidence:** medium
**Location:** `docs/features/route-management.md:109,364,543`

**Evidence:** "Identifiers **MUST** be server-generated in the anonymous GTS form `gts.cf.core.oagw.route.v1~{uuid}`" versus `route_handlers.rs:99-106` ("A bare UUID is the form the create response reports; the anonymous GTS resource identifier is accepted as well").

**Impact:** a client expecting the create response to carry the GTS-form identifier mis-parses `id`.

**Fix:** restate as "a bare UUID is minted and reported, and the by-identifier paths additionally accept the GTS form".

---

### RVW-123 — A few steps are declarative rather than imperative
**Severity:** MINOR · **Source:** cons3-14 · **Confidence:** medium
**Location:** `upstream-management.md:349,350`; `gear-foundation.md:122,149`; `error-handling.md:117`

**Evidence:** "3. [x] - `p1` - Verdict non-derivable when …" against sibling steps that begin with a capitalized imperative verb.

**Impact:** the nominal steps do not name an action the implementer or the test can perform.

**Fix:** recast as imperative clauses.

---

### RVW-124 — The malformed `[x]p2`` items do not parse as task-list checkboxes
**Severity:** MINOR · **Source:** cons1-12 · **Confidence:** high
**Location:** `docs/DECOMPOSITION.md:93,94,181,609`

**Evidence:** `  - [x]p2` - `cpt-cf-oagw-fr-config-layering`` — the space between the checkbox and the priority token and the " - " separator were dropped.

**Impact:** tick-state parsing of the requirement-coverage lists fails for four ids.

**Fix:** rewrite the four lines as `  - [x] `p2` - `cpt-cf-oagw-fr-…``.

---

### RVW-125 — The response-side `LimitedBody` inherits `usize::MAX` from the shared proxy client
**Severity:** MINOR · **Source:** qual-12/bfl3-7 (adjacent) — retained separately because the cap and the invalidation are distinct mechanisms
**Location:** `src/infra/proxy/service.rs:1470`

**Evidence:** `let body = response.into_limited_body();` with the client built from `HttpClientConfig::proxy()`.

**Root cause:** the response limit is not a gear-owned value.

**Fix:** see RVW-002.

---

### RVW-125b — The `enforce` declaration is recorded only when the value is adopted
**Severity:** MINOR · **Source:** bfl4-4 · **Confience:** medium
**Location:** `src/domain/merge.rs:290-305,312-324,361-383`

**Evidence:** `auth.sharing` is written only on adoption; same shape for `rate_limit.sharing` and `plugins.sharing`.

**Root cause:** the enforce/freeze state of a slot is derived from the sharing mode recorded at the last *adopted* contribution, so a layer that declares `Sharing::Enforce` but whose value is not adopted leaves the slot unlocked. Only the `scalars` slot records `Enforce` unconditionally.

**Impact:** in a three-layer chain [ancestor A (inherit) → ancestor B (enforce) → requester-owned C], a B `auth`/`rate_limit`/`plugins` `enforce` declaration is lost.

**Fix:** record the layer's declared sharing on the slot even when the value is not adopted.

---

### RVW-126 — The preflight answer is answered at handler level before alias resolution (code side)
**Severity:** MINOR · **Source:** bfl12-7/cons1-1 (code side) — see RVW-005 and RVW-085

---

### RVW-129 — No WebTransport relay code path exists behind the `wt` scheme value
**Severity:** MINOR · **Source:** fix-pass sweep (post-review)
**Location:** `oagw/src/infra/proxy/upgrade.rs` (WebSocket only)

**Evidence:** `features/request-proxy.md` §2 “WebTransport Session” and §5 `inst-rp-wt-1..4` state
that a session to an endpoint whose `scheme` is `wt` is established and relayed in both directions.
The crate admits `wt` at the configuration boundary (`EndpointScheme::wt`) and in the endpoint
allowlist, but the only session-establishment path is the WebSocket upgrade flow; nothing dispatches
on `wt`.

**Impact:** the two §6 acceptance boxes that assert a relayed `wt` session (`request-proxy.md`
`cpt-cf-oagw-dod-request-proxy-webtransport-session` and the `wt` clause of the idle-window box) are
left unticked rather than claimed, and a request addressed to a `wt` endpoint fails rather than
relaying.

**Disposition:** NOT FIXED this run. An HTTP/3 WebTransport transport is outside the reach of this
run; the gap is recorded here instead of being papered over in the acceptance list.

---

### RVW-130 — Counter release on a management-plane delete is not wired to the Control Plane
**Severity:** MINOR · **Source:** fix-pass sweep (post-review)
**Location:** `oagw/src/domain/services/mod.rs` (`delete_upstream`, `delete_route`);
`oagw/src/infra/proxy/rate_limiter.rs:222` (`release_resource`)

**Evidence:** `features/rate-limiting.md` §6 requires that “deleting an upstream or a route releases
its counters”. `RateLimiterRegistry::release_resource` implements exactly that boundary and is
unit-tested, but the Control Plane delete operations have no reference to the Data Plane registry,
so no delete path invokes it.

**Impact:** counters for a deleted resource are retained until the idle eviction of
`cpt-cf-oagw-dod-rate-limiting-per-instance-state` removes them; a recreated resource with the same
identifier inherits the old balance.

**Disposition:** NOT FIXED this run. Closing it needs a Control-Plane → Data-Plane notification
channel that crosses the domain/infra boundary the DESIGN fixes; the idle eviction bounds the
exposure. The corresponding §6 box stays unticked.

### RVW-131 — Cross-reference task/priority contract is unmet by prose mentions
**Severity:** MINOR · **Source:** fix-pass sweep (post-review)
**Location:** all nine `docs/features/*.md` (10 residual `ref-missing-task` / `ref-missing-priority`
validator errors each)

**Evidence:** the gear kit's reference contract (`constraints.toml`,
`[artifacts.DECOMPOSITION.identifiers.feature.references.FEATURE]`) sets `task = true` and
`priority = true`, and the scanner treats every inline backticked mention of a
`cpt-cf-oagw-feature-*` identifier as a reference. Each FEATURE doc therefore carries 5–7
validator errors for identifiers mentioned in prose, on top of the one traceability list item per
doc that is now ticked.

**Disposition:** PARTIALLY FIXED, residual recorded. The traceable reference each doc owes — the
`<!-- reference to DECOMPOSITION entry -->` list item — is present and ticked in all nine docs, and
the per-entry coverage lists in `DECOMPOSITION.md` name the same edges. Converting the remaining
~240 prose mentions into task-list items would mean rewriting every cross-reference sentence in the
nine documents into checkbox form, which is not a readable FEATURE idiom; the residual is recorded
here rather than papered over by stripping the backticks (which would hide the references from the
scanner without changing the prose).

---

### RVW-132 — `DECOMPOSITION.md` cannot be both truthful and validator-clean against the supplied PRD
**Severity:** MINOR · **Source:** fix-pass sweep (post-review)
**Location:** `docs/DECOMPOSITION.md` — the nine ticked entry status boxes and their
`Requirements Covered` lists

**Evidence:** ticking an entry's status box requires every nested task-tracked item beneath it to be
ticked (`parent-checked-nested-unchecked`), and ticking a nested requirement reference requires the
requirement's *definition* in the PRD to be ticked (`ref-done-def-not-done`). The supplied
`PRD.md` defines its requirements unticked (for example `cpt-cf-oagw-fr-upstream-mgmt` at line 196
and `cpt-cf-oagw-nfr-multi-tenancy` at line 539), and the run is forbidden to edit it.

**Impact:** nine `parent-checked-nested-unchecked` errors are irreducible in `DECOMPOSITION.md`:
the alternative states are either falsely reporting the nine entries as not implemented (the
RVW-041 defect) or editing the supplied PRD. Nine self-explaining structural errors are preferred
over a false implementation status.

**Disposition:** NOT FIXED — structurally impossible this run; recorded as an upstream-constrained
residual alongside the ADR 0009 defect in section D.

---

## D. Upstream defect (not fixable this run)

`cpt-cf-oagw-adr-required-headers-guard-plugin` (ADR 0009) is referenced from ADR 0009 but is not
present in `docs/DESIGN.md`. Fixing it requires editing a supplied upstream document, which this run
is forbidden to do. Recorded here so the residual `ref-missing-from-kind` validator error is
explained rather than unexplained.

---

## D2. Findings the live-server CI pass surfaced

The two findings below are invisible to the in-crate suite: the crate's
`FakeTypesRegistry` records what the gear submits without classifying it, so a
document the real GTS ingest refuses reaches the field. Both were exposed the
first time the release server was started against the real `types-registry`
gear, and both are now guarded by tests that run the same ingest the registry
runs (`tests/base_type_schema_validation.rs`,
`tests/catalog_instance_validation.rs`).

### RVW-133 — A Type Schema `$id` must carry the `gts://` URI form, not the bare canonical identifier
**Severity:** MAJOR · **Source:** live-server CI pass (post-test)
**Location:** `oagw/src/infra/type_provisioning.rs` (`base_type_schema`)

**Evidence:** the six base-type schema documents were submitted with
`"$id": "gts.cf.core.oagw.upstream.v1~"`. The GTS ingest refuses it: a schema
entity's `$id` must be the `gts://` URI form, and a `$id` starting with the
bare `gts.` canonical prefix leaves the entity with no resolvable GTS id
("Unable to detect GTS ID in schema entity"). The registry surfaces that as
`invalid_argument: Request validation failed`, gear initialization aborts, and
the server never binds.

**Disposition:** FIXED. `$id` is now `format!("{GTS_ID_URI_PREFIX}{type_id}")`.
The four assertions that encoded the bare form — `tests/type_provisioning.rs`,
`src/infra/type_provisioning_tests.rs` (two), `src/gear_tests.rs` — are
repointed at the URI form, and `tests/base_type_schema_validation.rs` ingests
all six documents through `gts::GtsOps` and refuses the bare form as a
regression.

### RVW-134 — A GTS instance document must be an instance, not a schema document
**Severity:** MAJOR · **Source:** live-server CI pass (post-test)
**Location:** `oagw/src/domain/type_catalog.rs` (`catalog_instance_documents`)

**Evidence:** the six catalog-only plugin identifiers were submitted through
`register_instances` as JSON Schema documents — a non-empty `$schema` keyword,
`type`, `properties`, `required` — carrying the instance identifier in `$id`.
The GTS model classifies an entity as a Type Schema by the presence of a
non-empty `$schema`, so the document was read as a schema keyed by an
identifier that carries a segment past the `~` and no `gts://` URI form, and
was refused with the same `invalid_argument: Request validation failed`
envelope. Gear initialization aborted at entry 2.6, after the base types.

**Disposition:** FIXED. The documents are now instance documents: the
identifier in the `id` field (the well-known-instance form), no `$schema`, and
the catalog metadata as instance fields — which the base type schema accepts
(`required: ["id"]`, `additionalProperties: true`). Verified on the live
server: initialization logs `oagw plugin catalog provisioned`, and the
registry validates each of the six against its base type. Both new regression
tests in `tests/catalog_instance_validation.rs` assert the instance registers
by its own identifier and that a schema-shaped variant is refused.

### RVW-135 — The OAGW problem body is not deserializable by the platform's canonical error middleware
**Severity:** MINOR · **Source:** live-server CI pass (post-test)
**Location:** `oagw/src/api/rest/error.rs` (`ProblemDetails`) against
`libs/toolkit/src/api/canonical_error_layer.rs` (`Problem.context`)

**Evidence:** the OAGW error contract puts every extension member at the top
level of the JSON object and states that `Problem` "cannot carry the OAGW
types" (`error.rs` module documentation, DESIGN §3.3). The platform middleware
wrapped around every response re-parses any `application/problem+json` body
into `toolkit_canonical_errors::Problem`, whose `context` member is required on
deserialization. Every error body this gear renders therefore logs
`canonical error middleware: failed to deserialize problem+json body error=
missing field 'context'` — observed live on a `400` from `POST /oagw/v1/routes`
with a malformed `upstream_id`.

**Impact:** logging and trace enrichment only. The middleware returns the
original body untouched on the failure branch, so the client still receives the
contracted body with `Content-Type: application/problem+json` and
`X-OAGW-Error-Source: gateway` — verified live. The cost is one spurious ERROR
line per OAGW error response, plus the loss of the middleware's own
classification line for that response.

**Disposition:** NOT FIXED this run. Nesting the OAGW extension members under
`context` would contradict the error contract the FEATURE and DESIGN §3.3
specify; giving `Problem.context` a `#[serde(default)]` is an edit to a
supplied platform crate. Recorded here so the log noise is explained rather
than unexplained.

---

## E. Reviewer-slice manifest

| Slice | Findings | Merged into |
|---|---|---|
| ENG (checklist: coupling/dead-surface) | 15 | RVW-024, -025, -026, -027, -028, -029, -030, -031, -092, -093, -094, -095, -096, -097 |
| PERF | 10 | RVW-002, -007, -020, -021, -022, -052, -066, -067, -068 |
| QUAL | 12 | RVW-015, -069, -070, -071, -072, -073, -074, -075, -098, -099, -064 |
| TEST | 14 | RVW-100..-112 |
| OBS | 10 | RVW-023, -045, -046, -047, -048, -087, -088, -089, -090, -091 |
| SEC | 8 | RVW-001, -034, -076, -077, -078, -079 |
| ERR | 12 | RVW-035..-038, -043, -054..-060 |
| Bug-finding L1-L2 | 10 | RVW-003, -007, -002, -010, -011, -009, -085, -053, -086, -054 |
| Bug-finding L3 | 12 | RVW-018, -017, -008, -007, -019, -063, -064, -052, -062, -061, -065, -021 |
| Bug-finding L4 | 10 | RVW-006, -015, -016, -125b, -080, -081, -052, -082, -083, -084 |
| Bug-finding L5 | 6 | RVW-012, -013, -014, -032, -033, -024 |
| Consistency INV/DEP/TERM | 15 | RVW-005, -118, -049, -050, -051, -114, -115, -113, -120, -119, -124, -116, -117 |
| Consistency CLAIM/LINK | 3 | RVW-004, -040, -041 |
| Consistency STALE/STYLE | 14 | RVW-039, -040, -042, -043, -041, -044, -113, -040, -040, -121, -049, -122, -123 |
