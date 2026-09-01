# oagw — accepted deviations

Recorded <2026-08-29> by the semantic review loop. Each entry names the design element, the deviation, the rationale, the consequence and the follow-up.

---

## `OagwConfig` token-cache keys are declared but not consumed by the plugin layer

**Design element**: [ADR-0008](docs/ADR/0008-oauth2-client-credentials-auth-plugin.md) — "Gear-Level Configuration (OagwConfig)" (`token_cache_ttl_secs`, `token_cache_capacity`).

**Deviation**: `OagwConfig` (`oagw/src/config.rs`) declares both keys with the ADR-0008 defaults (`TOKEN_CACHE_TTL_SECS = 300`, `TOKEN_CACHE_CAPACITY = 10 000`) and exposes them through `OagwConfig::token_cache_ttl_secs()` / `token_cache_capacity()`, but `OAuth2ClientCredAuthPlugin` (`oagw/src/infra/plugin/oauth2_cc_auth.rs`) still reads the build-time constants `DEFAULT_TOKEN_CACHE_TTL_SECS` / `DEFAULT_TOKEN_CACHE_CAPACITY`. `BuiltinPlugins::with_builtins(credstore)` / `with_builtins_optional(credstore)` take no configuration argument, so there is no path from the gear config into the plugin constructors.

**Rationale**: The config contract and its accessors are the stable part of the change; the plumbing would require threading a value through `DataPlaneServiceImpl` construction and changing the signature of `BuiltinPlugins::with_builtins{,_optional}` (ADR-0008 originally documented a `with_builtins(credstore, token_http_config, token_cache_config)` signature that does not exist).

**Consequence**: None on behaviour — the declared defaults and the constants are numerically identical (300 s / 10 000 entries). Operators cannot retune the cache without recompiling. The doc comment on `BuiltinPlugins::with_builtins` ("they are process constants because the gear configuration contract exposes no such keys") is now stale.

**Follow-up**: Change `BuiltinPlugins::with_builtins{,_optional}` to accept the two values (or a small `TokenCacheConfig`) and pass them into `OAuth2ClientCredAuthPlugin::with_cache`; then delete the stale doc comment in `oagw/src/infra/plugin/mod.rs`.

## Observability metrics are exported by OTLP push, not scraped

**Design element**: `DESIGN.md` §4.2 / `PRD.md` §6.1 "Observability" — metrics collection.

**Deviation**: The gear emits OpenTelemetry instruments (`oagw/src/infra/metrics.rs`, meter `cf-gears-oagw`) and relies on the platform meter provider's OTLP exporter. There is **no** `/metrics` HTTP endpoint and no Prometheus exporter; the docs previously claimed "Prometheus metrics at `/metrics` (admin-only)". The docs now state the push transport.

**Rationale**: Adding a Prometheus exporter would mean adding an exporter dependency and a serving surface the gear does not have. Correcting the documentation is the honest fix.

**Consequence**: A Prometheus scrape of the gear returns nothing. Series named as documented appear only at the collector that receives the OTLP stream.

**Follow-up**: If pull-based collection is required, register a Prometheus exporter in the gear and re-introduce the `/metrics` path as a documented (admin-only) endpoint. `DESIGN.md` §4.7 item 9 records this.

## Configuration is persisted in process memory, not in a database

**Design element**: `DESIGN.md` §1.2 ("SeaORM + `toolkit-db`"), §1.3, §3.4 ("`toolkit-db` — Database persistence (SeaORM, multi-backend)"), §3.6 ("Database Schemas & Tables") and [ADR-0006](docs/ADR/0006-state-management.md) (state ownership).

**Deviation**: The repositories in `oagw/src/infra/storage/mod.rs` are **in-memory only** (`dashmap` for the `(tenant_id, alias)` index, `parking_lot::RwLock` around a single `Store` holding the upstream / route / plugin vectors). No SeaORM entities, no migrations and no `toolkit-db` connection exist for this gear. ADR-0006's *CP State* discussion is implemented with these process-local structures rather than a database.

**Rationale**: `toolkit-db` is not published in this environment, and the gear's data model (three small tenant-scoped collections with hierarchical merge) is fully expressible in memory. The in-memory store is a deliberate, contained substitution behind the `UpstreamRepository` / `RouteRepository` / `PluginRepository` traits (`oagw/src/domain/repo.rs`), so the rest of the gear is storage-agnostic.

**Consequence**: Configuration does not survive a restart; every instance holds its own copy, so multi-replica deployments would diverge; there is no durable audit of configuration changes; §3.6's table/index/column inventory (PKs, unique keys, `oagw_plugin.gc_eligible_at`, binding tables `oagw_upstream_plugin` / `oagw_route_plugin`) describes a schema that is not materialized anywhere. The repository traits and the domain validation pipeline are unaffected, and `§3.6`'s *Common Queries* remain semantically accurate at the domain level.

**Follow-up**: Implement the three repository traits on SeaORM/`toolkit-db` with the §3.6 schema, add migrations, and swap the wiring in `src/gear.rs`. The in-memory implementation stays useful as the test store.

## Plugin garbage collection is dormant

**Design element**: `DESIGN.md` §3.1 (Plugin model) / §3.2 (Plugin Lifecycle Management) / §3.6 (Common Queries) — `oagw_plugin.gc_eligible_at`.

**Deviation**: `Plugin.gc_eligible_at` (`oagw/src/domain/model.rs`) exists as a field and is persisted, but nothing sets it to a value other than `None` and no background job consumes it. Deletion is purely explicit and refused with `409 PluginInUse` while referenced.

**Rationale**: A time-based sweeper is a background-scheduling concern outside this slice; the reference-guarded explicit delete delivers the same safety without it.

**Consequence**: A plugin that is unlinked is never removed automatically, so an abandoned tenant-defined plugin row lives until the tenant deletes it. `DESIGN.md` §3.2 and §3.6 now say this explicitly.

**Follow-up**: `DESIGN.md` §4.7 item 2 — a background sweeper that marks unlinked plugins `gc_eligible_at = now + ttl` and deletes them once the TTL elapses.

## Problem bodies omit `upstream_id` and `trace_id`

**Design element**: `DESIGN.md` §3.3 "Error Response Format" — extension fields.

**Deviation**: `OagwProblem` (`oagw/src/api/rest/error.rs`) emits `type`, `title`, `status`, `detail`, `instance`, `context`, `error_code`, `error_domain`, `retry_after_seconds`, `plugin_id`, `referenced_by`, `host` and `path`. `upstream_id` and `trace_id` are not emitted.

**Rationale**: The data plane resolves the selected upstream inside `DataPlaneServiceImpl` and returns only a `DomainError` to the transport layer, so the handler has no `upstream_id` to report. No trace identifier is propagated into `SecurityContext` or the request extensions, so a `trace_id` value cannot be obtained without inventing one. Fabricating field values in a documented contract is worse than omitting them.

**Consequence**: Clients cannot correlate a problem with the specific upstream instance or a distributed trace by reading the body alone.

**Follow-up**: Extend the data-plane error path to carry the resolved upstream identifier back to the transport layer, and adopt the platform's trace propagation once it reaches the gateway handlers. Both are transport-adjacent changes in `src/infra/`, which is outside the file scope of this change.

## HEAD and OPTIONS on the proxy paths are not declared as OpenAPI operations

**Design element**: `DESIGN.md` §3.3 "Proxy API" — methods served by `/oagw/v1/proxy/{alias}[/{path_suffix}]`.

**Deviation**: `HEAD` and `OPTIONS` are served by the proxy handlers but are not declared through `OperationBuilder::register`. `GET`/`POST`/`PUT`/`PATCH`/`DELETE` are declared.

**Rationale**: The toolkit `OpenApiRegistry` collapses every non-`GET`/`POST`/`PUT`/`PATCH`/`DELETE` method onto `HttpMethod::Get`, so declaring `HEAD` or `OPTIONS` would silently overwrite the documented `GET` operation of the same path item.

**Consequence**: An OpenAPI client generated from the document does not advertise `HEAD`/`OPTIONS`, although both are served.

**Follow-up**: Add `head`/`options` support to the toolkit's `HttpMethod` mapping, then declare them on both proxy paths. Documented in `oagw/src/api/rest/routes.rs` (module documentation) and `DESIGN.md` §3.2.

## Deferred (other agent owns the file)

These findings are **not** fixed by this change; the file each would require was explicitly out of scope for this pass (`src/gear.rs`, `src/domain/*`, `src/infra/*`, `tests/common/mod.rs`, `Cargo.toml`).

- **F-017 — OpenAPI operations are registered at the `/oagw/v1/...` paths, not the `/api/oagw/v1/...` contract paths.** The dual mount lives in `src/gear.rs` (`router.merge(sub).nest("/api", sub)`), which is owned by another agent; the registry is a single document, so declaring both path sets would require re-registering the operations under a second prefix at mount time. `DESIGN.md` §3.2 documents the current behaviour. Owner: the `gear.rs` / control-plane agent.

- **`BuiltinPlugins::with_builtins` doc comment** (`oagw/src/infra/plugin/mod.rs`) still states that the gear configuration contract exposes no token-cache keys. It now does (`OagwConfig::token_cache_ttl_secs` / `token_cache_capacity`), so the comment is stale until the plumbing entry above is done. Owner: the plugin/control-plane agent.

- **OAuth2 token-cache values are build-time constants** (`oagw/src/infra/plugin/oauth2_cc_auth.rs::DEFAULT_TOKEN_CACHE_TTL_SECS`, `DEFAULT_TOKEN_CACHE_CAPACITY`): the first entry above describes the signature change needed to consume the config keys. Owner: the plugin/control-plane agent.

## Review gates

The compile-fail review gate is **not applicable** to this gear: it exists to prove that an API surface forbids a misuse at compile time, and every OAGW contract here is enforced at runtime (RFC 9457 rendering, extractor rejection mapping, OpenAPI declaration) or by the serde/schema layer. There is no contract in `DESIGN.md` §3.3, ADR-0008 or ADR-0009 whose negation can be expressed as a Rust program that must fail to compile. The runtime equivalents are the integration tests in `oagw/tests/api_contract_tests.rs`.
