# Implementation Plan: Mini Chat Gear

**Branch**: `001-mini-chat-gear` | **Date**: 2026-10-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-mini-chat-gear/spec.md`

## Summary

Implement the `mini-chat` ToolKit gear (crate `cf-gears-mini-chat`) and its SDK
(`cf-gears-mini-chat-sdk`) so the example server serves the 19 operations of `docs/api/api.json`
under `/mini-chat`, streams OpenAI-Responses-compatible provider output as the stable SSE contract,
persists chats/messages/turns/attachments in SQLite/Postgres through Secure ORM, enforces the
credit quota cascade with CAS-guarded settlement and outbox events, and runs the background
workers (orphan watchdog, upload reaper, outbox handlers for usage, audit, attachment/chat cleanup and
thread summaries). Two bundled plugin gears (static model policy, static audit) live in
`mini_chat::infra::plugins`. Approach: DDD-light layering (api → domain → infra) following the
credstore/oagw gears; provider calls go through the in-process OAGW client; tests at three levels
(unit, in-process router tests with fakes, black-box Python suite against the real binary with a mock
provider).

## Technical Context

**Language/Version**: Rust 2024 edition, toolchain pinned by `rust-toolchain.toml` (rust-version 1.95)

**Primary Dependencies**: toolkit (gear macro, OperationBuilder, ClientHub, canonical error layer,
OData), toolkit-db (Secure ORM, DBProvider transactions, outbox), toolkit-security (SecurityContext,
AccessScope), toolkit-canonical-errors, authz-resolver-sdk (PolicyEnforcer), authn-resolver-sdk (S2S
client credentials), oagw-sdk (ServiceGatewayClientV1: proxy + upstream/route CRUD),
types-registry-sdk (plugin instances), axum 0.8 (SSE, multipart via multer), sea-orm 2 /
sea-orm-migration, image 0.25 (thumbnails, lossless WebP), serde/serde_json, tokio, uuid, time.

**Storage**: SQLite (tests, dev, acceptance) and PostgreSQL via SeaORM; tables of DESIGN §3.7 plus the
shared toolkit outbox tables (migrations included in the gear migration list).

**Testing**: `cargo test` (unit tests in `*_tests.rs`, in-process router integration tests in
`src/**/..._tests.rs` using in-memory SQLite + fake OAGW gateway + fake policy plugin + mock
enforcer); Python `pytest` black-box suite in `gears/mini-chat/mini-chat/tests/blackbox/` driving the
debug binary with an aiohttp mock OpenAI provider.

**Target Platform**: Linux server (single binary `cf-gears-example-server`).

**Project Type**: Platform service component (gear) + SDK library.

**Performance Goals**: relay each provider SSE event immediately (no buffering); bounded channel of
`streaming.sse_channel_capacity` (default 32).

**Constraints**: offline build, no new crates outside `Cargo.lock`; fail-closed authorization;
provider identifiers never exposed; build gate command must succeed.

**Scale/Scope**: P1 scope of DESIGN; ~19 REST operations, 9 tables, 5 outbox queues, 2 background
scanners, 2 plugin gears.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Gate | Status |
|---|---|---|
| I. Contract Fidelity | DTOs copied from `docs/api/api.json`; errors from ADR-0004 table; schema from DESIGN §3.7; config from Appendix B | PASS |
| II. Platform Conventions | gear macro, OperationBuilder, toolkit extractors, Secure ORM, toolkit outbox, OAGW client, types-registry plugins; no new crates (eventsource parsing hand-written) | PASS |
| III. Test-First | tasks list test tasks before implementation per story; traceability table for every acceptance item; no tests in `testing/e2e/suites/mini_chat/` | PASS |
| IV. Isolation / Fail-Closed | every handler evaluates the PDP and scopes queries by tenant + owner; 404 masking; 503 on PDP failure; sanitizer for provider text | PASS |
| V. Streaming & Billing | bounded mpsc relay; CAS finalization transaction = state + settlement + outbox; replay path has no access to settlement | PASS |
| VI. P1 Scope | MCP, doc summaries, hard purge, policy cache, image daily quota not implemented; non-OpenAI adapters implemented minimally per ADR-0005 (request shaping + event mapping) | PASS |

Post-design re-check (after Phase 1): PASS — no violations; Complexity Tracking empty.

## Project Structure

### Documentation (this feature)

```text
specs/001-mini-chat-gear/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # rest-api.md, sse-and-provider.md
├── traceability.md      # acceptance item → tests (written during implement)
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gears/mini-chat/
├── mini-chat-sdk/                      # package cf-gears-mini-chat-sdk, lib mini_chat_sdk
│   └── src/{lib,models,events,error,plugin_api,gts}.rs (+ models_tests.rs)
└── mini-chat/                          # package cf-gears-mini-chat, lib mini_chat
    ├── src/
    │   ├── lib.rs                      # pub mod api, domain, infra, config, gear
    │   ├── gear.rs                     # #[toolkit::gear(name="mini-chat", deps=[types_registry,
    │   │                               #   authn_resolver, authz_resolver, oagw], capabilities=[db,rest,stateful])]
    │   ├── config.rs                   # MiniChatConfig (Appendix B) + validation
    │   ├── api/rest/                   # routes.rs, handlers/*.rs, dto.rs, sse.rs, error mapping
    │   ├── domain/
    │   │   ├── error.rs                # DomainError → CanonicalError mapping (ADR-0004)
    │   │   ├── authz.rs                # PEP helpers (PolicyEnforcer → AccessScope + owner predicate)
    │   │   ├── credits.rs              # credits_micro, ceil_div, overflow checks
    │   │   ├── estimate.rs             # token estimation (text, surcharges)
    │   │   ├── quota.rs                # cascade, preflight decision, reserve/settle SQL, status
    │   │   ├── context.rs              # ContextPlan assembly + truncation
    │   │   ├── sanitize.rs             # provider id / url / credential sanitizer
    │   │   ├── billing.rs              # billing outcome derivation
    │   │   ├── chat_service.rs         # chats CRUD, messages list, reactions, models, turns read
    │   │   ├── stream_service.rs       # send / replay / retry / edit pipeline, provider task, finalization
    │   │   ├── turn_service.rs         # mutation preview/commit, delete
    │   │   ├── attachment_service.rs   # upload, get, delete, indexing, thumbnails
    │   │   └── workers/                # orphan watchdog, upload reaper, outbox handlers
    │   └── infra/
    │       ├── db/                     # entities, migrations, repositories (raw SQL where needed)
    │       ├── llm/                    # provider resolver, adapters, SSE parser, file/vector-store clients
    │       ├── oagw_provision.rs       # upstream/route registration + reconcile loop
    │       ├── policy.rs, audit.rs     # plugin gateways (types-registry selection)
    │       ├── outbox.rs               # enqueuer + queue names
    │       └── plugins/{static_model_policy,static_audit}/   # bundled plugin gears
    └── tests/blackbox/                 # pytest suite + aiohttp mock provider
```

**Structure Decision**: two crates under `gears/mini-chat/` (SDK + gear), mirroring
`gears/credstore/{credstore-sdk,credstore}`; bundled plugins stay inside the gear crate because the
example server imports `mini_chat::infra::plugins::{static_audit, static_model_policy}`.

## Complexity Tracking

No constitution violations.
