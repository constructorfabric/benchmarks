# Mini Chat gear + SDK — Implementation Design Spec

Date: 2026-10-04
Status: Approved (brainstorming, architectural path)

## 1. Intent

Implement the `mini-chat` gear (`gears/mini-chat/mini-chat`, package `cf-gears-mini-chat`, lib
`mini_chat`) and its SDK crate (`gears/mini-chat/mini-chat-sdk`, package
`cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`). The gear is a multi-tenant AI chat with SSE
streaming, attachments (documents and images), quotas with tier downgrade, turn mutations, thread
summaries, outbox-driven cleanup/usage/audit, and background recovery workers.

**Source of truth** (in priority order):

1. `gears/mini-chat/docs/ADR/*` (accepted ADRs; override older DESIGN/PRD text they supersede)
2. `gears/mini-chat/docs/DESIGN.md`
3. `gears/mini-chat/docs/PRD.md`
4. `docs/api/api.json` (generated OpenAPI — exact DTO field names, required/optional, enums)
5. `gears/mini-chat/docs/acceptance-criteria.md` (checklist; every item implemented + tested)

This spec does not restate the 7k-line DESIGN. It fixes the architecture, the decisions on
ambiguities, and the traceability from every acceptance criterion to modules and tests.
Implementers MUST read the referenced DESIGN sections for exact codes and values.

**Success** = the server builds with
`cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`,
starts with `config/mini-chat.yaml` (and with a test config pointing providers at a mock
OpenAI-compatible server), and a black-box client observes the HTTP/SSE contract, the DB rows and
the provider requests that DESIGN specifies. Our own automated tests cover every acceptance item
and pass.

## 2. Constraints

- Offline cargo: only crates already in `Cargo.lock` (workspace deps or exact versions present).
- Workspace lints are strict (clippy pedantic deny). Code must pass `cargo clippy` for the two
  crates (`-D warnings` is not required for the run but warnings should be kept minimal; lints set
  to `deny` will fail the build and must be respected).
- Do not put tests in `testing/e2e/suites/mini_chat/` (reserved for the hidden suite).
- Existing server wiring imports `mini_chat`, `mini_chat::infra::plugins::static_audit`,
  `mini_chat::infra::plugins::static_model_policy` — these paths must keep existing and must
  register the bundled plugin gears (`static-mini-chat-audit-plugin`,
  `static-mini-chat-model-policy-plugin`).
- Persistence uses SeaORM via `toolkit-db` (SQLite and Postgres migrations). Column types:
  `Uuid` (stored as 16-byte BLOB on SQLite), `time::OffsetDateTime` for TIMESTAMPTZ,
  `time::Date` for `quota_usage.period_start`, JSON as text/JSONB. Table and column names exactly
  as DESIGN §3.7 (the hidden suite queries and seeds them, e.g. `quota_usage`).
- All provider traffic goes through OAGW (`ServiceGatewayClientV1::proxy_request`). The gear
  provisions its own upstreams/routes at start (ADR-0005).
- The outbox is the platform `toolkit-db` outbox (its own migrations/tables); five queues.

## 3. Scope decisions (answers to open questions)

| Topic | Decision |
|---|---|
| Provider adapters | `openai_responses` fully (streaming + non-streaming summary call, tools, citations, errors). Config accepts all four kinds; `openai_chat_completions`, `vllm_responses`, `anthropic_messages` are implemented after the core is complete (lower priority; minimal faithful mapping per DESIGN §3.2/ADR-0005). |
| Storage kinds | `openai` (`/v1/files`, `/v1/vector_stores`) and `azure` (`/openai/files?api-version=`, `/openai/vector_stores?api-version=`) both implemented. |
| Knowledge search | Config parsed/validated; feature implemented last (off by default). |
| Metrics | OpenTelemetry instruments registered for the main series where cheap; not a test target. |
| K8s leader election | `k8s` feature compiles a Lease elector only if trivial; default build uses the no-op elector (always leader). |
| MCP | Not implemented (ADR-0006). |
| Doc summaries, image daily quota, policy cache, hard purge, chat-deletion audit | Not implemented (ADR-0007/0008/0009). Columns exist, values null/0. |
| Replay `done` | Rebuilt from stored models; `downgrade_reason` omitted; no citations (ADR-0010). |
| SSE ping | Only between `stream_started` and the first `delta`/`tool` event (ADR-0010); axum keep-alive comment every 30 s. |
| Error envelope | Canonical `Problem` for REST, `{code,message}` for SSE (ADR-0004). |

## 4. Architecture

DDD-light layering per DESIGN §1.3.

```
mini-chat-sdk/src/
  lib.rs            re-exports
  models.rs         PolicySnapshot, ModelCatalogEntry (+ EstimationBudgets, ModelGeneralConfig,
                    ModelApiParams, ModelFeatures, ModelToolSupport, ModelSupportedEndpoints,
                    ModelPreference), ModelTier, KillSwitches, UserLimits, TierLimits,
                    PolicyVersionInfo
  usage.rs          UsageEvent, UsageTokens (outbox usage payload, DESIGN §5.6/A.3)
  audit.rs          TurnAuditEvent, TurnMutationAuditEvent, AuditEvent enum, ToolCalls, ...
  plugin_api.rs     MiniChatModelPolicyPluginClientV1, MiniChatAuditPluginClientV1 (+ errors:
                    MiniChatModelPolicyError, PublishError, AuditPluginError{Transient,Permanent,
                    PluginTimeout}, UserLicenseStatus)
  gts.rs            MiniChatModelPolicyPluginSpecV1, MiniChatAuditPluginSpecV1 GTS plugin specs

mini-chat/src/
  lib.rs            module tree, gear registration re-exports
  gear.rs           MiniChatGear: init/start/stop, REST registration, migrations
  config.rs         MiniChatConfig (+ sections, defaults, validation, ${VAR} expansion)
  api/rest/         routes.rs, dto.rs, handlers/{chats,messages,stream,turns,attachments,
                    models,reactions,quota}.rs, error.rs (DomainError -> Problem), sse.rs
  domain/
    error.rs        DomainError (one enum, mapped once to Problem and to SSE codes)
    authz.rs        PEP wrapper (PolicyEnforcer -> AccessScope + owner predicate)
    credits.rs      credits_micro_checked, ceil_div
    estimation.rs   text-token estimation, surcharges, reserve computation
    sanitize.rs     provider-id / URL / credential sanitizer
    billing.rs      billing outcome derivation (single shared fn)
    context.rs      ContextPlan assembly + truncation + summary trigger inputs
    services/       chat_service, message_service, model_service, reaction_service,
                    turn_service (status + mutations), stream_service (send/retry/edit
                    pipeline, provider task, relay), finalization_service, quota_service
                    (preflight cascade, reserve, settle, status, warnings),
                    attachment_service, thread_summary_service
  infra/
    db/entities/    chats, messages, chat_turns, attachments, message_attachments,
                    thread_summaries, chat_vector_stores, quota_usage, message_reactions
    db/migrations/  m0001_initial (sqlite + postgres variants) + outbox migrations
    db/repos/       one repository per aggregate, all scoped via AccessScope
    llm/            provider_resolver.rs, adapter trait + events, providers/openai_responses.rs
                    (request builder, SSE parser, error mapping), [other adapters],
                    storage/{openai,azure}.rs (files + vector stores), multipart.rs
    oagw_provisioning.rs  upstream/route registration + background reconcile
    outbox/         usage_handler, audit_handler, attachment_cleanup_handler,
                    chat_cleanup_handler, thread_summary_handler, payloads.rs
    workers/        leader.rs (no-op / k8s), orphan_watchdog.rs, upload_reaper.rs
    plugins/        static_model_policy (gear), static_audit (gear),
                    policy_gateway.rs, audit_gateway.rs (types-registry lazy resolution)
    thumbnail.rs    WebP thumbnail with decode limits
```

### 4.1 Request flows (normative references)

| Flow | DESIGN reference |
|---|---|
| Chat CRUD, list (OData, cursor, `updated_at desc, id`) | §3.3 Create/List/Get/Update/Delete Chat |
| Messages list (`created_at asc`), attachments + `my_reaction` enrichment | §3.3 List Messages, §3.7 message_attachments |
| Send message (validation → authz → chat load → idempotency/replay → parallel guard → snapshot boundary → preflight (kill switches, cascade, tool quotas) → input limit / image guards → context assembly → provider resolution → single reserve transaction → `stream_started` → provider task → finalization → terminal event) | §3.6 Send Message, §3.3 Streaming Contract, §4 Parallel Turn Policy, §5.4 |
| SSE contract & ordering, ping, close rules, error codes | §3.3 SSE Event Definitions/Ordering, Streaming error codes, ADR-0010 |
| Provider event translation (OpenAI Responses) | §3.3 Provider Event Translation |
| Finalization (CAS, settlement, usage+audit outbox, thread-summary trigger, terminal SSE gating) | §5.7, §5.8, §5.9, §3.6 Thread Summary trigger |
| Turn status / retry / edit / delete | §3.3 Turn Status, §3.9, §3.6 retry/edit variant |
| Attachments upload/get/delete, vector store creation protocol, indexing poll, background indexing, thumbnails | §3.3 Upload/Get Attachment, §3.6 File Upload, §3.7 chat_vector_stores, §4 Attachment Deletion |
| Quota (credits, estimation, cascade, reserve re-check, settlement, overshoot, warnings, status endpoint) | §3.2 quota service, §4 Two-Tier Rate Limiting, §5.3–5.5, §5.10 |
| Outbox handlers (usage, audit, attachment cleanup, chat cleanup, thread summary) | §3.2, §3.6 Cleanup on Chat Deletion, §3.6 Thread Summary, §5.6, B.9.2–B.9.4 |
| Orphan watchdog, upload reaper | §4 Turn Lifecycle/Orphan Watchdog, B.9.1, B.9.5 |
| OAGW provisioning | §3.2 OAGW provisioning, §3.5, ADR-0005 |
| Models API, reactions, quota status | §3.3 Models API, Message Reaction API, §3.2 Quota Status Endpoint |
| Authorization (PEP matrix, fail-closed) | §3.8 |
| Config keys/defaults/validation | Appendix B.1–B.9 |

### 4.2 Key cross-cutting decisions

1. **One `DomainError`** with variants carrying the machine-readable reason; one mapping to
   canonical `Problem` (ADR-0004 table) and one mapping to SSE `{code,message}`. Not-found variants
   carry the GTS resource type `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`.
2. **Authorization**: every chat-scoped operation evaluates the PDP with resource type
   `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~` and the action from §3.8, compiles
   constraints to `AccessScope`, and adds an owner predicate for the subject. Denied/compile
   failure → 403 `AUTHZ_DENIED`; PDP failure → 503 `Retry-After: 5`. Models use the model resource
   type with `require_constraints=false`; quota status uses the user-quota resource type.
3. **Transactions**: the send-path reserve transaction (quota increments + re-check, user message,
   `message_attachments` validation/insert, `chat_turns` insert, `chats.updated_at` bump) and the
   finalization transaction (CAS + assistant message + settlement + usage/audit outbox + optional
   thread-summary outbox) are single DB transactions.
4. **Streaming**: provider task → bounded mpsc (`streaming.sse_channel_capacity`) → SSE writer.
   The handler runs setup in a spawned task and awaits it (disconnect before open still finalizes
   correctly). Dropping the SSE body cancels a `CancellationToken`; a failed channel send is also
   treated as disconnect. Terminal events are sent only after the finalization commit.
5. **Determinism**: effective model, reserve, policy version and generation floor are persisted at
   preflight; settlement reads only persisted fields + the snapshot of `policy_version_applied`.
6. **Outbox payloads**: JSON; usage = SDK `UsageEvent`; audit = SDK audit event enum; cleanup and
   thread-summary payloads are gear-internal structs with fields listed in DESIGN.
7. **Provider ids never leave the gear**: sanitizer applied to every provider-originated message
   before it reaches SSE or logs visible to clients; citations map provider file ids to attachments.

## 5. Testing strategy

- **Unit tests** (in-crate `#[cfg(test)]` and `tests/`): credits, estimation, cascade truth tables,
  billing derivation table, sanitizer, SSE parser/translation, request builder, context truncation,
  config validation, error mapping, thumbnail limits, summary response parsing.
- **Repository/service tests** on in-memory/temporary SQLite with migrations applied, with fakes
  for policy plugin, PDP, OAGW gateway client and outbox where needed: chat CRUD + pagination,
  idempotency/replay, parallel guard, reserve/settle math on `quota_usage`, finalization CAS
  exactly-once, mutations, attachments lifecycle, cleanup handlers, orphan watchdog, upload reaper,
  thread summary CAS.
- **Black-box E2E suite** (own suite at `testing/e2e/suites/mini_chat_dev/`, Python/pytest, not the
  reserved `mini_chat` dir): starts the debug server with a generated config pointing providers at
  a Python mock OpenAI-compatible server (Responses SSE, files, vector stores), drives the HTTP/SSE
  API and inspects the SQLite DB and recorded provider requests. One test module per acceptance
  area.
- **Traceability**: `docs/superpowers/specs/mini-chat-acceptance-traceability.md` maps each
  acceptance item → implementing modules → test names.

## 6. Acceptance criteria → design mapping

| Acceptance area | Modules | Primary tests |
|---|---|---|
| Principles (isolation, context budget, no buffering, model immutable, quota before outbound) | authz, repos, context, stream_service, quota_service | unit + e2e isolation/budget/streaming |
| Chat CRUD, list, ordering | chat_service, chats handlers, chats repo | repo/service + e2e chats |
| Messages API | message_service, messages repo | service + e2e messages |
| Streaming send, preflight, persistence | stream_service, finalization_service | service + e2e stream |
| SSE contract | api/rest/sse.rs, stream_service relay | unit (ordering) + e2e sse |
| Idempotency & replay | stream_service | service + e2e replay |
| Parallel turns | stream_service, turns repo (unique index) | service + e2e |
| Turn mutations / lifecycle | turn_service, stream_service | service + e2e turns |
| Attachments | attachment_service, storage, thumbnail, upload reaper | service + e2e attachments |
| Models / reactions / quota status | model_service, reaction_service, quota_service | service + e2e |
| Quota enforcement, settlement | quota_service, finalization_service, credits, estimation | unit + service + e2e quota |
| Context assembly | context.rs, stream_service | unit + e2e provider request inspection |
| Error mapping & sanitization | api/rest/error.rs, sanitize.rs | unit + e2e |
| Web search | stream_service tools, quota_service, openai adapter | unit + e2e |
| Cleanup & recovery, thread summary | outbox handlers, thread_summary_service, workers | service + e2e |
| Authorization | authz.rs, every handler | unit + e2e cross-tenant/foreign user |
