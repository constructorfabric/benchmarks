## Context

`gears/mini-chat/mini-chat` is an empty stub (two empty plugin modules imported by `apps/cf-gears-example-server`). The contract lives in `gears/mini-chat/docs/` (PRD, DESIGN, ADR-0001..0010, acceptance-criteria). The gear is checked black-box: a hidden suite starts `cf-gears-example-server` (debug build, features `mini-chat,static-authn,static-authz,single-tenant,static-credstore`) with its own config using the DESIGN Appendix B keys, then drives HTTP/SSE, inspects the gear's SQLite database, and inspects the requests received by an OpenAI-compatible mock provider reached through OAGW.

Constraints: offline cargo (only crates in `Cargo.lock`), Rust toolchain pinned, platform conventions (ToolKit gear macro, OperationBuilder REST, canonical errors, Secure ORM, toolkit-db outbox, OAGW in-process proxy, AuthZ PEP, types-registry plugin discovery).

## Goals / Non-Goals

**Goals:**
- Serve the full P1 public API (DESIGN §3.3) with the exact bodies, SSE events, status codes and canonical Problem errors.
- Persist exactly the DESIGN §3.7 schema (table/column names, SQLite UUID-as-BLOB encoding via SeaORM) so DB-level assertions and seeded rows (e.g. `quota_usage`) work.
- Implement quota reserve/settle, finalization CAS, usage + audit outbox events, orphan watchdog, upload reaper, cleanup handlers and thread summaries.
- Send OpenAI Responses API requests through OAGW with the documented shape; provision OAGW upstreams/routes at start.
- Automated tests covering every acceptance-criteria item.

**Non-Goals:**
- MCP (ADR-0006), document summaries, chunk caps, image quotas, policy snapshot cache, hard purge, audit content (ADR-0007..0009).
- Anthropic secondary image copies (`attachments.secondary_*` stay `not_attempted`) and gear metrics (OTLP instruments).
- Full fidelity of the Chat Completions / vLLM / Anthropic adapters: the config accepts all four `kind`s; `openai_responses` is fully implemented and is the only adapter the black-box suite exercises (ADR-0005 confirms E2E uses `openai_responses` only). Other kinds reuse the request builder with documented tool dropping.
- Kubernetes Lease leader election (`k8s` feature): a no-op elector is used; CAS guards keep correctness.
- Passing `cargo clippy` with the workspace's pedantic deny-list (not part of the build gate).

## Decisions

1. **Crate layout (DDD-light, like oagw/credstore).**
   `mini_chat_sdk`: `models.rs` (catalog, kill switches, limits, policy snapshot, usage/audit events), `plugin_api.rs` (`MiniChatModelPolicyPluginClientV1`, `MiniChatAuditPluginClientV1`), `gts.rs` (plugin specs), `error.rs`.
   `mini_chat`: `gear.rs` (gear + lifecycle), `config.rs`, `api/rest/{routes,handlers,dto,sse,error}.rs`, `domain/{error,credits,estimation,sanitize,context,quota,authz,service/*}.rs`, `infra/{db/{entity,migrations,repo},llm/{openai_responses,sse_parser,files,vector_stores},oagw_provisioning,outbox,policy_gateway,audit_gateway,workers/{orphan_watchdog,upload_reaper,thumbnail},plugins/{static_model_policy,static_audit}}.rs`.
   Alternative (single flat module) rejected: too large to parallelize safely.

2. **Persistence via SeaORM entities + raw-statement migrations per backend.** Migrations create exactly the DESIGN tables, CHECKs and partial unique indexes (`UNIQUE(chat_id) WHERE state='running' AND deleted_at IS NULL`, `UNIQUE(chat_id, request_id)`, etc.). Request-path queries are scoped by tenant and owner (Secure ORM / explicit `tenant_id` + `user_id` predicates derived from the PEP `AccessScope`); child tables are reached through an owner-scoped chat lookup. CAS updates use `UPDATE ... WHERE state='running'` and check `rows_affected`.

3. **Outbox: toolkit-db shared outbox** (five queues: usage, attachment cleanup, chat cleanup, thread summary, audit), enqueued in the same DB transaction as the domain change; handlers return Ok/Retry/Reject. If the shared outbox cannot run on SQLite in-process, a gear-local fallback table `mini_chat_outbox` with the same semantics is used (decision recorded in tasks).

4. **Streaming pipeline.** Handler performs all preflight in a spawned setup task (so a client disconnect cannot leave a half-created turn), commits the reserve transaction, then spawns a provider task that reads the OAGW SSE stream, translates events, pushes them through a bounded `mpsc` (capacity `sse_channel_capacity`) to the axum SSE response, and finalizes via CAS. Cancellation: dropping the SSE body drops a guard that cancels a `CancellationToken`; the provider task aborts the upstream request and finalizes `cancelled`. Terminal events are emitted only after the finalization transaction commits.

5. **Quota service** implements DESIGN §5 literally: checked `credits_micro`, per-candidate reserve, cascade with kill switches and reasons, reserve + re-check inside the turn transaction, settlement on persisted `period_start` (derived from `started_at` for orphans), overshoot cap, exactly one usage event per reserve.

6. **Policy and audit via plugins resolved through types-registry** by `vendor`; the bundled static plugins register themselves as separate gears in the same crate (`static-mini-chat-model-policy-plugin`, `static-mini-chat-audit-plugin`).

7. **Errors.** One `DomainError` enum mapped to canonical errors (toolkit-canonical-errors) with the reasons/resource types of ADR-0004; SSE errors are `{code, message}` with sanitized messages.

8. **Operational choices fixed during implementation.**
   - No `ThrottlingSpec` on `create_chat`: the api-gateway fails startup when a referenced zone is missing from its config, and DESIGN does not require the zones; omitting the binding is always safe.
   - All provider calls (chat, summary, files, vector stores, knowledge search) use the gear S2S security context, so upstreams provisioned at start are visible regardless of the caller's tenant; the end user is carried in `user` / `metadata`.
   - The default upstream alias equals OAGW's own derivation (`host` on 80/443, `host:port` otherwise) because OAGW rejects any other alias for hostname endpoints and requires one for IP endpoints.
   - The outbox uses the default `toolkit_outbox_*` tables (the gear has its own database) and leased per-message handlers; usage events carry `usage: null` when the provider reported none.
   - OData `id` filter fields are declared as strings and converted to UUIDs in the mapper so DESIGN's quoted form `id eq '<uuid>'` works; `$orderby=title` orders on `COALESCE(title, '')`.
   - Knowledge search: function tool + agentic loop in the stream service, Azure vector-store search retriever over OAGW, enabled only by `knowledge_search.enabled`.

9. **Testing.** Rust unit tests (credits, estimation, sanitization, SSE parsing, cascade, truncation, billing outcome derivation, config validation) in separate `*_tests.rs` files, plus a Python black-box suite in `gears/mini-chat/e2e/` that builds the config, starts the real server binary and an aiohttp mock provider (`mock_provider.py`), and covers every acceptance-criteria item through HTTP/SSE, the SQLite DB and recorded provider requests.

## Risks / Trade-offs

- [Hidden suite seeds/reads DB rows with an encoding we don't match] → use SeaORM default types (UUID BLOB, `time` types) like other gears; test seeding `quota_usage` directly in our own suite.
- [Unknown config keys in the hidden config make init fail (strict deny_unknown_fields)] → accept every key listed in DESIGN Appendix B, including deprecated ones; worker sections accept unknown keys.
- [OAGW HTTP upstream to the mock requires `use_http` + `allow_http_upstream`] → honor `use_http`/`port` in provisioning; our test config sets `oagw.allow_http_upstream: true`.
- [Large surface implemented in limited time] → prioritize behaviours named by acceptance-criteria; secondary metrics are registered but minimal.
- [SQLite write contention between workers and requests] → short transactions, `busy_timeout` from config.

## Migration Plan

New gear; no data migration. Rollback = build without the `mini-chat` feature.

## Open Questions

- Exact toolkit-db outbox API on SQLite (resolved during implementation).
- Whether the hidden suite expects `store` in provider requests; we send `store: false` (the request controls `store` per DESIGN's `extra_body` list).
