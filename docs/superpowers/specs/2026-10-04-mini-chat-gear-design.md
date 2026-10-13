# Mini Chat gear + SDK — Implementation Design (spec)

Date: 2026-10-04
Status: approved (benchmark run; the human partner role is played by the agent)
Source of truth: `gears/mini-chat/docs/` (`PRD.md`, `DESIGN.md`, `ADR/0001..0010`,
`acceptance-criteria.md`). This spec does **not** restate those documents; it pins the
implementation shape, resolves ambiguities, and maps every acceptance-criteria item to
code and tests. Where this spec and `gears/mini-chat/docs/` disagree, the gear docs win.

## 1. Intent and success criteria

**Outcome.** Restore the `mini-chat` gear (multi-tenant AI chat over SSE) and create its SDK
crate so that the example server built with
`--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`
serves the full P1 contract black-box: REST + SSE API, database records, and provider
requests sent through OAGW to an OpenAI-compatible provider.

**Who checks it.** A hidden acceptance suite: starts the debug server with its own config
(keys/defaults per DESIGN Appendix B), talks HTTP/SSE, inspects the gear's SQLite DB, and
inspects the requests received by an OpenAI-compatible mock (Responses API, Files API,
Vector Stores API).

**Success.**
1. Server builds with the features above (offline, lockfile-only crates).
2. Every item of `acceptance-criteria.md` is implemented and covered by our own automated
   tests (Rust unit/integration tests in the gear crate + a black-box Python smoke suite
   outside `testing/e2e/suites/mini_chat/`), and the tests pass.
3. Wire contract (routes, bodies, SSE events, status codes, Problem reasons, DB schema,
   config keys, provider request shape) follows DESIGN/PRD/ADRs.

**Constraints (from the task).** No new crates outside `Cargo.lock`; no network except the
model API; never kill processes by command-line match; do not put tests in
`testing/e2e/suites/mini_chat/`.

## 2. Approaches considered

| Option | Summary | Verdict |
|---|---|---|
| **A. Layered single gear + SDK crate (chosen)** | `cf-gears-mini-chat-sdk` (`mini_chat_sdk`) holds public models + plugin APIs; `cf-gears-mini-chat` (`mini_chat`) holds api/domain/infra layers, the in-process `llm_provider` library, outbox handlers, workers, and the two bundled plugin gears under `infra::plugins::{static_model_policy, static_audit}` (paths fixed by the example server imports). | Matches DESIGN §1.3, ADR-0001/0005, platform conventions. |
| B. Separate `llm_provider` crate/service | Extract provider adapters. | Rejected: ADR-0001 mandates an in-gear library. |
| C. Simplified synchronous pipeline (no outbox, no CAS) | Direct plugin calls from request path. | Rejected: DESIGN §5.6–5.9 make outbox + CAS + watchdog P1-mandatory, and the suite inspects outbox rows. |

## 3. Crate layout

### 3.1 `gears/mini-chat/mini-chat-sdk` (package `cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`)

Added to `[workspace].members` and `[workspace.dependencies]` (`mini-chat-sdk = { package =
"cf-gears-mini-chat-sdk", version = "0.4.5", path = "gears/mini-chat/mini-chat-sdk" }`);
`cf-gears-mini-chat` depends on it.

Contents:
- **Policy models** (serde): `PolicySnapshot { policy_version, model_catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches }`,
  `ModelCatalogEntry` (fields + defaults per DESIGN Appendix A.1 "Required fields"),
  `ModelTier { Premium, Standard }` (serde accepts `Premium`/`premium`, serializes lowercase for the API),
  `EstimationBudgets` (defaults: bytes_per_token_conservative 4, fixed_overhead_tokens 100,
  safety_margin_pct 10, image_token_budget 1000, tool_surcharge_tokens 500,
  web_search_surcharge_tokens 500, code_interpreter_surcharge_tokens 1000,
  minimal_generation_floor 50), `ModelGeneralConfig` (`type`, `available_from`,
  `max_file_size_mb`, `api_params`, `features`, `tool_support`, `supported_endpoints`),
  `ModelApiParams` (optional temperature/top_p/frequency_penalty/presence_penalty, `stop`,
  `extra_body`, `reasoning_effort`), `ModelToolSupport` (`web_search`, `file_search`,
  `image_generation`, `code_interpreter`, `mcp`), `ModelPreference { is_default, sort_order }`,
  `WebSearchContextSize { Low, Medium, High }`, `KillSwitches` (all six fields required on
  deserialize), `UserLimits { user_id, policy_version, standard: TierLimits, premium: TierLimits }`,
  `TierLimits { limit_daily_credits_micro, limit_monthly_credits_micro }`.
- **Usage event** `UsageEvent` exactly as DESIGN §3.2 / Appendix A.3 (field names, `usage` nullable,
  `user_id`/`turn_id` omitted for system tasks, `system_task_type` omitted for user turns).
- **Audit events** `TurnAuditEvent` (event_type `turn_completed|turn_failed`, identities, models,
  usage, latency, tool calls `{web_search_calls, file_search_calls}`, quota decision,
  empty `prompt/response/attachments/license/quota_scope`, `trace_id`),
  `TurnMutationAuditEvent` (`turn_retry|turn_edit|turn_delete` with `actor_user_id`,
  `chat_id`, `original_request_id`, `new_request_id` / `request_id`, `timestamp`), wrapped in an
  `AuditEvent` enum serialized with a tag.
- **Plugin APIs**: `MiniChatModelPolicyPluginClientV1` (`get_current_policy_version`,
  `get_policy_snapshot`, `get_user_limits`, `publish_usage`, `check_user_license` default
  `active: false`) and `MiniChatAuditPluginClientV1` (`emit(AuditEvent)`), GTS plugin spec
  types `MiniChatModelPolicyPluginSpecV1` / `MiniChatAuditPluginSpecV1`, and errors
  (`PolicyError`, `PublishError::{Transient, Permanent}`, `AuditPluginError::{Transient, Permanent, PluginTimeout}`).
- **Credit arithmetic** `credits_micro_checked` (DESIGN §5.3, checked, bounds 10M tokens,
  multipliers `1..=10_000_000_000`), shared by gear and plugin validation.

### 3.2 `gears/mini-chat/mini-chat` (package `cf-gears-mini-chat`, lib `mini_chat`)

```
src/
  lib.rs                 gear declaration (name "mini-chat"), re-exports
  gear.rs                init / start / stop / REST registration
  config.rs              full gear config (Appendix B.1–B.9), validation, ${VAR} expansion
  api/                   routes, handlers, DTOs, SSE writer, error -> Problem mapping, OpenAPI
  domain/
    error.rs             DomainError (one enum; mapped to Problem in api/)
    authz.rs             PEP: resource types, actions, fail-closed mapping
    chat_service.rs      chats CRUD + list
    message_service.rs   messages list (+attachments/reactions enrichment)
    reaction_service.rs
    model_service.rs     Models API, model resolution (with/without enabled filter)
    quota/               estimator, credits, cascade preflight, reserve, settlement, status, warnings
    context.rs           ContextPlan assembly + truncation + guards + summary preamble
    stream/              send pipeline, replay, provider task, relay, finalization, events
    turn_service.rs      turn status, retry/edit/delete
    attachment_service.rs upload/get/delete, limits, purposes, vector-store protocol, indexing
    thread_summary.rs    trigger evaluation + handler logic
    sanitize.rs          provider-id/url/credential scrubbing
  infra/
    db/                  entities, migrations (pg + sqlite), repositories
    llm/                 provider resolver, adapters (openai_responses, openai_chat_completions,
                         vllm_responses, anthropic_messages), SSE parser, error mapping
    storage/             file storage + vector store dispatch (openai, azure), anthropic files
    oagw_provisioning.rs upstreams/routes at start + reconcile loop
    outbox/              5 queue handlers (usage, attachment cleanup, chat cleanup, thread summary, audit)
    gateways/            model policy gateway, audit gateway (types-registry resolution)
    workers/             orphan watchdog, upload reaper, leader elector (no-op; k8s Lease behind `k8s`)
    metrics.rs           OpenTelemetry instruments (`mini_chat_*`)
    plugins/static_model_policy/   bundled plugin gear `static-mini-chat-model-policy-plugin`
    plugins/static_audit/          bundled plugin gear `static-mini-chat-audit-plugin`
tests/                   integration tests (sqlite + fake OAGW + in-proc PDP)
```

## 4. Behaviour decisions (pinning ambiguities)

All behaviour not listed here is implemented exactly as DESIGN/PRD/ADRs specify.

1. **Routes** under `url_prefix` (default `/mini-chat`): the 19 operations of
   `docs/api/api.json` (DESIGN §3.3 minus the P2 `:temporary` toggle), matching its schemas,
   operation ids, tags and status sets. All routes require auth + the base license feature
   `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`. `POST /v1/chats` references rate-limit
   zone `rl_mini_chat_chat` and in-flight zone `ifl_mini_chat_chat` (api.json declares them; every
   shipped config defines both zones) with `require_security_context: false`, `dry_run: true`
   (observe-only, per the `config/quickstart.yaml` description of the binding).
2. **Provider request (openai_responses)** body: `model` = `provider_model_id`, `instructions`
   = catalog `system_prompt` + appended guards (`\n\n` separated), `input` = ordered items:
   optional thread-summary item (`role: user`, preamble + `\n\n` + summary), recent messages
   (`role: user|assistant`), current user message. User content is an array of
   `{type: input_text}` + `{type: input_image, file_id}` items; assistant content is an array
   of `{type: output_text, text}`. `stream: true`, `store: false`, `max_output_tokens` =
   applied value, `max_tool_calls` = catalog value, `tools` (only when non-empty),
   `include: ["code_interpreter_call.outputs"]` only with code interpreter, `user` =
   `{tenant_hex}{user_hex}`, `metadata` `{tenant_id,user_id,chat_id,request_type,feature}`,
   api_params when set (`temperature`, `top_p`, `frequency_penalty`, `presence_penalty`,
   `stop` when non-empty, `reasoning: {effort}` when `reasoning_effort` set), `extra_body`
   merged except controlled keys (warning logged). Tool shapes per DESIGN §4
   (`file_search` with `vector_store_ids` + `max_num_results`; `web_search` with
   `search_context_size`; `code_interpreter` with `container: {type: auto, file_ids}`).
3. **SSE parsing** uses `event:` name or JSON `type`; handles `response.output_text.delta`,
   `response.output_text.annotation.added`, file/web search and code-interpreter events,
   `response.output_item.done` (code_interpreter logs; `function_call` items →
   `unexpected_tool_use` when knowledge search is off), `response.completed`,
   `response.incomplete`, `response.failed`, `error`. Citations are collected from
   annotation events and, if none were streamed, from `response.completed.response.output`.
4. **Thread summary request** (non-streaming): same adapter, `stream: false`,
   `metadata.request_type = summary`, system `user` id `11111111-6a88-4768-9dfc-6bcd5187d9ed`.
5. **Storage**: OpenAI prefix `/v1`, Azure prefix `/openai` + `?api-version={api_version}`.
   Files upload multipart `purpose=assistants`, filename `{chat_id}_{attachment_id}.{ext}`.
   Vector store create `POST {prefix}/vector_stores` (`{"name": "chat-{chat_id}"}`), add file
   `POST {prefix}/vector_stores/{vs}/files` (`{"file_id", "attributes": {"attachment_id"}}`),
   poll `GET {prefix}/vector_stores/{vs}/files/{file_id}`, deletes via `DELETE`.
6. **DB**: tables/columns/indexes per DESIGN §3.7 (names exact) for both SQLite and
   PostgreSQL; platform outbox tables via the platform outbox migrations; UUIDs and
   timestamps use the SeaORM defaults of the platform (SQLite: UUID as 16-byte BLOB,
   timestamps as text). `quota_usage.period_start` is a DATE.
7. **Knowledge search**: config parsed/validated; tool offered only when enabled and its
   parameters can be built (Azure retriever implemented). Default off.
8. **Leader election**: no-op elector by default; `k8s` feature compiles a Lease-based
   elector (`mini-chat-orphan-watchdog`, `mini-chat-upload-reaper`, 15 s / 2 s).
9. **Metrics**: all "Emitted" instruments of DESIGN §4 Metrics are created with the
   configured prefix and recorded at the documented points; "declared, deferred" ones are
   registered and never recorded.

## 4a. Platform integration decisions

Derived from the platform code (reference notes kept in `/tmp/refs/01..05` during the run):

1. **Gear declaration**: `#[toolkit::gear(name = "mini-chat", deps = [types_registry, authn_resolver,
   authz_resolver, oagw], capabilities = [db, rest, stateful], lifecycle(entry = "serve",
   stop_timeout = "30s", await_ready))]`. The `deps` list also forces linking of those gears
   (the example-server feature set used by the check does **not** enable its own `oagw`
   feature, so mini-chat must pull OAGW in). Bundled plugin gears live in their own modules
   (`infra::plugins::static_model_policy`, `infra::plugins::static_audit`) with
   `deps = [types_registry]`, register a `PluginV1<Spec>` instance in types-registry and a
   scoped client in ClientHub; the main gear resolves them lazily via
   `GtsPluginSelector` + `choose_plugin_instance` (never in `init`).
2. **Config**: `ctx.config_expanded_or_default::<MiniChatConfig>()`, `#[serde(default,
   deny_unknown_fields)]` on the sections DESIGN B.1 lists as strict; worker sections
   (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept
   unknown keys. `${VAR}` expansion via `#[derive(ExpandVars)]` on provider `host`,
   `auth_config`, tenant overrides, and `client_credentials`.
3. **Startup**: `init` parses/validates config, resolves ClientHub clients, builds services,
   registers no provider state that requires plugins. `serve` (lifecycle) performs the S2S
   client-credentials exchange (retrying while the authn plugin is not ready), OAGW
   provisioning (fatal on validation errors, deferred + background reconcile on
   `FailedPrecondition(auth.config.secret_ref)` / unavailable), starts the outbox pipeline
   (5 queues), the orphan watchdog and the upload reaper; on cancel it stops workers and the
   outbox. REST routes are registered from services built in `init`; requests that reach a
   provider before provisioning finished fail with `provider_error`.
4. **OAGW**: alias = configured `upstream_alias`, else for IP hosts the host, for hostnames the
   OAGW-derived alias (`host`, or `host:port` for a non-standard port) so OAGW validation
   passes; the resolver uses the same alias. Upstream auth `SharingMode::Inherit`. Chat route:
   `POST` on the path prefix of `api_path` before `{model}` (or the full path), `Append`
   suffix mode, query allowlist = query keys present in `api_path`. RAG routes per storage
   kind: `{prefix}/files` (`POST`,`DELETE`) and `{prefix}/vector_stores` (`GET`,`POST`,`DELETE`),
   Azure allowlisting `api-version`. Requests go through `proxy_request` with URI
   `/{alias}{path}`; SSE responses are consumed with `oagw_sdk::sse::ServerEventsStream`.
   Both failure channels are handled (`Err(CanonicalError)` and `Ok` with
   `ErrorSource::Gateway`).
5. **AuthZ**: `PolicyEnforcer` with resource types chat/model/user-quota (GTS ids per DESIGN
   §3.8); chat scope = PDP scope `.ensure_owner(subject_id)`; child tables are queried with
   `scope.tenant_only()` + `chat_id` from the owner-scoped chat query. `Denied`/`CompileFailed`
   → 403 `AUTHZ_DENIED`; `EvaluationFailed` → 503 `Retry-After: 5`.
6. **DB**: raw-SQL migrations per dialect (credstore pattern), UUID columns as BLOB on SQLite
   (sqlx stores 16-byte blobs), timestamps `time::OffsetDateTime` (TEXT on SQLite,
   TIMESTAMPTZ on PG), booleans INTEGER on SQLite. OData datetime filter values are normalised
   to the stored representation in the field mapper. Platform outbox tables via
   `toolkit_db::outbox::outbox_migrations()` (default `toolkit_outbox` prefix) appended to the
   gear migrations. Outbox: explicit partition = hash(key) % `outbox.num_partitions`,
   `batch_size(1)` so `attempts` is per message, enqueue inside the domain transaction and
   fire the `Wake` after commit, payload ≤ 64 KiB (chat-cleanup payload overflow → 400 per
   ADR-0004).
7. **REST**: `OperationBuilder` with full paths `/mini-chat/v1/...` (prefix from `url_prefix`),
   `.authenticated().require_license_features([&License])` (base feature), create-chat
   throttling zones `rl_mini_chat_chat` / `ifl_mini_chat_chat` (dry-run); request bodies via
   `toolkit::api::rest::extract::{Json, Path}` (canonical 400/415/422 reasons); errors are
   `CanonicalError` built from `#[resource_error(gts_id!("cf.core.mini_chat.<x>.v1~"))]`
   types; 429s get a manual `Retry-After` only where DESIGN requires one (none do); upload
   parses multipart by hand with `multer` to produce the documented reasons; SSE via
   `axum::response::sse::Sse` with a 30 s comment keep-alive and our own `ping` events;
   `Location` built from the request URI; OData via the `OData` extractor +
   `toolkit_db::odata::paginate_odata` with `LimitCfg { default: 20, max: 100 }`.

## 5. Testing strategy

- **Unit tests** (in-module or `tests/` per DESIGN Confirmation sections): credit math,
  estimator, cascade/downgrade matrix, warnings, billing-outcome derivation, context
  truncation, SSE parsing per adapter, sanitizer, MIME/purpose routing, config validation,
  error→Problem mapping (category/status/reason), audit/usage payload shapes.
- **Integration tests** (`gears/mini-chat/mini-chat/tests/`): real SQLite (temp file) +
  migrations + outbox, a fake `ServiceGatewayClientV1` that scripts provider responses
  (SSE streams, files, vector stores, errors, delays) and records requests, an in-process
  PDP stub, and the static policy plugin. Tests drive the REST router (axum `oneshot`) so
  status codes, Problem bodies and SSE event sequences are asserted end to end.
- **Black-box smoke suite** (`testing/mini_chat_blackbox/`, Python + aiohttp mock provider):
  runs against the real debug binary with a generated config, exercising the main flows.
- Every acceptance-criteria item maps to at least one named test (traceability table in the
  plan).

## 6. Acceptance-criteria coverage map (summary)

| Area | Implementation | Key tests |
|---|---|---|
| Principles & constraints | authz PEP + scoped repos; context budget (INPUT_TOO_LONG, CONTEXT_BUDGET_EXCEEDED); unbuffered relay; immutable `chats.model`; preflight before provider | isolation, budget, no-buffer (delta before provider end), model-lock, quota-before-call |
| Chat CRUD / list / ordering | chat_service + OData | crud, validation, filter/order/cursor errors, ordering by activity |
| Messages API | message_service | list + OData, DTO invariants, counts/order |
| Streaming | stream service | request_id correlation, preflight validation, persistence |
| SSE contract | relay + events | ordering, done/usage/warnings, error sanitization, ping |
| Idempotency & replay | replay path | side-effect-free replay, conflicts, replay-before-guard |
| Parallel turns | unique running index + checks | 409 turn_already_running, terminal unblocks |
| Turn mutations | turn_service | latest/terminal checks, new request_id, race, attachments copy |
| Turn lifecycle | finalization CAS | completed/failed/cancelled, partial/null content |
| Attachments | attachment_service, workers | lifecycle, limits, indexing (sync/background/timeout), reaper, cleanup |
| Models / Reactions / Quota status | services | visibility, reactions idempotency, status accuracy |
| Quota enforcement / settlement | quota service + finalization | reserve, downgrade, credits, exactly-once settlement + usage event |
| Context assembly | context.rs | deterministic truncation, guards/tools |
| Error mapping | api/error | Problem shapes, SSE codes, sanitization |
| Web search | tools + quota | tool inclusion, citations, accounting, daily/per-turn limits |
| Cleanup & recovery | outbox handlers, summary | chat cleanup, summary trigger/failure/invalidation |
| Authorization | PEP | 404 foreign, 403 deny, 503 PDP failure |
