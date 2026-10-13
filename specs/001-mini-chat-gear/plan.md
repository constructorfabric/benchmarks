# Implementation Plan: Mini Chat Gear (multi-tenant AI chat) and its SDK

**Branch**: `001-mini-chat-gear` | **Date**: 2026-10-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-mini-chat-gear/spec.md`

## Summary

Implement the `mini-chat` ToolKit gear (`gears/mini-chat/mini-chat`, lib `mini_chat`) and its SDK
crate (`gears/mini-chat/mini-chat-sdk`, lib `mini_chat_sdk`). The gear serves the REST/SSE API under
`/mini-chat/v1` (chats, messages, streaming turns, attachments, turn status and mutations,
reactions, models, quota status), persists state through Secure ORM over SQLite/PostgreSQL,
reaches the LLM/RAG provider only through the in-process OAGW client (OpenAI Responses wire format,
OpenAI/Azure Files and Vector Store APIs), enforces credit quotas with reserve → settle and a
CAS-guarded finalization, and runs its asynchronous work (usage, audit, attachment cleanup, chat
cleanup, thread summary) as toolkit-db outbox handlers plus two leader-only workers (orphan
watchdog, upload reaper). Two bundled plugin gears (static model policy, static audit) are
registered from the same crate and discovered through types-registry. See [research.md](./research.md)
for the platform API decisions.

## Technical Context

**Language/Version**: Rust 2024 edition, toolchain pinned by `rust-toolchain.toml` (1.98.1)

**Primary Dependencies**: `toolkit` (gear macro, REST OperationBuilder, OData extractor, SSE),
`toolkit-db` (Secure ORM, migrations, outbox), `toolkit-canonical-errors`, `toolkit-odata`,
`toolkit-security`, `toolkit-gts`/`gts`, `authz-resolver-sdk`, `authn-resolver-sdk`, `oagw-sdk`,
`types-registry-sdk`, `sea-orm` 2.0, `axum` 0.8, `utoipa`, `multer`, `image` (webp/png/jpeg/gif),
`serde`/`serde_json`, `tokio`, `futures`, `opentelemetry` — all already in `Cargo.lock`

**Storage**: SQLite (tests, example config) and PostgreSQL via toolkit-db; schema per DESIGN §3.7
plus the shared outbox tables

**Testing**: `cargo test` — unit tests (domain logic: credits, estimation, cascade, context
assembly, SSE parsing, sanitizer, config validation) and integration tests that boot the gear's
services against in-memory SQLite with a fake provider client and allow-all/deny/fail PDP stubs;
a scripted smoke run of the real server against a Python/Rust mock OpenAI-compatible provider

**Target Platform**: Linux server (in-process gear of `cf-gears-example-server`)

**Project Type**: Rust library crates (gear + SDK) composed into the example server binary

**Performance Goals**: in-gear TTFT overhead p99 < 50 ms; cancel-to-abort p99 < 200 ms

**Constraints**: offline build (lockfile only); bounded SSE channel (16–64, default 32); no
buffering; config sections reject unknown keys where DESIGN B.1 says so; provider ids never leave
the gear

**Scale/Scope**: 19 REST operations, 9 tables + outbox, 5 outbox queues, 2 leader workers,
2 bundled plugins

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Gate | Status |
|-----------|------|--------|
| I. Spec documents are the source of truth | Routes, DTOs, codes, schema and config keys traced to PRD/DESIGN/ADR/OpenAPI (contracts/, data-model.md) | PASS |
| II. Platform conventions first | Gear macro, Secure ORM, canonical errors, OData, outbox, OAGW, types-registry plugins (research.md) | PASS |
| III. Test-first | tasks.md orders tests before implementation; AC traceability table in spec | PASS |
| IV. Isolation, fail-closed | PolicyEnforcer on every operation, owner predicate added, 404/403/503 mapping | PASS |
| V. Streaming-first | Provider task → bounded mpsc → SSE writer; terminal event after commit | PASS |
| VI. Deterministic quota/billing | Reserve tx with re-check, CAS finalization + settle + outbox in one tx | PASS |
| VII. P1 scope | MCP, doc summary, purge, image quota, snapshot cache excluded | PASS |
| Build constraints | No new crates outside lockfile; SDK crate added to workspace | PASS |

Post-design re-check (after Phase 1): PASS — no violations; Complexity Tracking empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-mini-chat-gear/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── openapi-mini-chat.json      # mini-chat subset of docs/api/api.json
├── contracts/
│   ├── rest-api.md
│   ├── sse-events.md
│   ├── provider-wire.md
│   ├── config.md
│   └── plugins-sdk.md
└── tasks.md
```

### Source Code (repository root)

```text
gears/mini-chat/
├── mini-chat-sdk/                       # cf-gears-mini-chat-sdk / mini_chat_sdk
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── models.rs                    # catalog, kill switches, limits, policy snapshot
│       ├── usage.rs                     # UsageEvent, TokenUsage
│       ├── audit.rs                     # TurnAuditEvent, mutation audit events
│       ├── plugin_api.rs                # MiniChatModelPolicyPluginClientV1, MiniChatAuditPluginClientV1
│       ├── gts.rs                       # plugin spec schemas
│       └── error.rs
└── mini-chat/                           # cf-gears-mini-chat / mini_chat
    ├── Cargo.toml
    ├── src/
    │   ├── lib.rs
    │   ├── gear.rs                      # gear registration, init/start/stop, REST registration
    │   ├── config.rs                    # gear config + validation (Appendix B)
    │   ├── api/rest/                    # routes, handlers, dto, error mapping, sse writer
    │   ├── domain/                      # services: chat, message, stream, turn, attachment,
    │   │                                #   reaction, model, quota, context, finalization, authz
    │   ├── infra/
    │   │   ├── db/                      # entities, migrations, repositories
    │   │   ├── llm/                     # provider resolver, adapters, SSE parser, storage clients
    │   │   ├── oagw_provisioning.rs
    │   │   ├── outbox/                  # enqueuer + handlers (usage, audit, cleanup x2, summary)
    │   │   ├── workers/                 # orphan watchdog, upload reaper, leader elector
    │   │   ├── metrics.rs
    │   │   └── plugins/
    │   │       ├── static_audit/        # gear static-mini-chat-audit-plugin
    │   │       └── static_model_policy/ # gear static-mini-chat-model-policy-plugin
    │   └── util/                        # sanitizer, credits, estimation, thumbnails, mime
    └── tests/                           # integration tests (in-memory SQLite + fake provider)
```

**Structure Decision**: two crates under `gears/mini-chat/`, matching the platform's
gear + SDK split (credstore, oagw). The bundled plugins live inside the gear crate because the
example server imports them from `mini_chat::infra::plugins`.

## Complexity Tracking

No constitution violations.
