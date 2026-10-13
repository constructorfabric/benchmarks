# Implementation Plan: Mini Chat Gear and SDK

**Branch**: `001-mini-chat-gear` | **Date**: 2026-10-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-mini-chat-gear/spec.md`

## Summary

This plan implements the `mini-chat` ToolKit gear and its SDK crate exactly as specified by
`gears/mini-chat/docs`. The gear serves the REST and SSE chat API through api-gateway. It persists
state in SQLite or Postgres through Secure ORM, and enforces owner-only access through the AuthZ PEP.
Quota is reserved and settled in credits, with a premium→standard cascade. Calls to an
OpenAI-compatible provider go through in-process OAGW proxying, and the gear provisions its own
upstreams and routes. Asynchronous work runs on the toolkit outbox: usage, audit, attachment
cleanup, chat cleanup and thread summary. An orphan watchdog and an upload reaper run alongside. The
bundled static model-policy and audit plugins live in the gear crate. The SDK crate holds the
plugin contracts and the shared models. Automated unit tests and in-process integration tests cover
every acceptance-criteria item.

## Technical Context

**Language/Version**: Rust (edition and toolchain from `rust-toolchain.toml`, workspace lints)

**Primary Dependencies**: toolkit (gear macro, OperationBuilder, ClientHub), toolkit-db (Secure ORM,
outbox, odata), toolkit-canonical-errors, toolkit-odata, sea-orm / sea-orm-migration, axum (SSE),
authz-resolver-sdk (PolicyEnforcer), authn-resolver-sdk (S2S), oagw-sdk (`ServiceGatewayClientV1`),
types-registry-sdk (plugin discovery), multer (multipart), image (thumbnails), serde, time, uuid

**Storage**: SQLite (dev and test), Postgres; dual-dialect raw-SQL migrations; toolkit outbox tables

**Testing**: `cargo test`, using in-crate unit tests and `tests/` integration suites with in-memory
SQLite, a mock PDP and a fake OAGW provider

**Target Platform**: Linux server, linked into `apps/cf-gears-example-server` via feature `mini-chat`

**Project Type**: platform gear (library crates linked into a server binary)

**Performance Goals**: deltas are relayed as they arrive, with no buffering; in-gear TTFT overhead
p99 < 50 ms

**Constraints**: offline build (only crates in `Cargo.lock`); the server must build with
`mini-chat,static-authn,static-authz,single-tenant,static-credstore`; no provider identifiers are
exposed

**Scale/Scope**: about 20 endpoints, 9 tables, 5 outbox queues, 2 background workers, 2 bundled plugins

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Evidence |
|---|---|---|
| I. Contract-first | PASS | contracts/ point to DESIGN/ADR/api.json; the error table follows ADR-0004 |
| II. Gear architecture conformance | PASS | SDK crate plus gear crate with api/domain/infra layers; gear macro; ClientHub; OperationBuilder; canonical errors (research R1–R5) |
| III. Test-first | PASS | test tasks precede implementation in tasks.md; every AC item is mapped to tests (R8) |
| IV. Black-box verifiability | PASS | behaviour depends only on config; the smoke run uses the real server binary |
| V. Security, tenancy, simplicity | PASS | PEP plus `ensure_owner` on every query; no secrets in the gear; no new dependencies |

Post-design re-check: PASS. Proxying with the S2S context (R6) is a deliberate choice inside the
documented contract: identity still reaches the provider through `user` and `metadata`. It needs no
complexity justification.

## Project Structure

### Documentation (this feature)

```text
specs/001-mini-chat-gear/
├── plan.md  research.md  data-model.md  quickstart.md
├── contracts/ (rest-api.md, provider-wire.md, config.md)
└── tasks.md
```

### Source Code (repository root)

```text
gears/mini-chat/mini-chat-sdk/            # package cf-gears-mini-chat-sdk, lib mini_chat_sdk
└── src/
    ├── lib.rs
    ├── models.rs        # ModelCatalogEntry, EstimationBudgets, ModelGeneralConfig, KillSwitches,
    │                    # PolicySnapshot, UserLimits, TierLimits, ModelTier, UsageEvent, UsageTokens
    ├── audit.rs         # TurnAuditEvent, TurnMutationAuditEvent, AuditEvent enum
    ├── plugin_api.rs    # MiniChatModelPolicyPluginClientV1, MiniChatAuditPluginClientV1, errors
    └── gts.rs           # MiniChatModelPolicyPluginSpecV1, MiniChatAuditPluginSpecV1

gears/mini-chat/mini-chat/                # package cf-gears-mini-chat, lib mini_chat
├── src/
│   ├── lib.rs           # pub mod api, config, domain, infra, gear
│   ├── gear.rs          # #[toolkit::gear], init/start/stop, DatabaseCapability, RestApiCapability
│   ├── config.rs        # MiniChatConfig plus validation (Appendix B)
│   ├── api/rest/        # routes.rs, handlers/*.rs, dto.rs, sse.rs, error.rs (DomainError→CanonicalError)
│   ├── domain/
│   │   ├── error.rs     # DomainError taxonomy
│   │   ├── authz.rs     # resource types, actions, scope helpers (ensure_owner)
│   │   ├── credits.rs   # credits_micro, estimation, periods
│   │   ├── quota.rs     # cascade/preflight, reserve, settle, warnings, status
│   │   ├── context.rs   # ContextPlan assembly and truncation, guards
│   │   ├── sanitize.rs  # provider id / url / key scrubbing
│   │   ├── billing.rs   # billing outcome derivation
│   │   └── service/     # chats, messages, stream (send/replay/finalize), turns (mutations),
│   │                    # attachments, models, reactions, quota_status
│   └── infra/
│       ├── db/          # entities/*.rs, migrations/*.rs, repo helpers
│       ├── llm/         # provider resolver, openai_responses adapter, SSE decoder, files and vector stores
│       ├── oagw.rs      # provisioning plus reconcile
│       ├── outbox.rs    # queue names, enqueue helpers, handlers (usage, audit, cleanup x2, summary)
│       ├── workers.rs   # orphan watchdog, upload reaper, leader elector (no-op / k8s)
│       ├── policy.rs    # model-policy and audit plugin gateways (GtsPluginSelector)
│       ├── thumbnail.rs # image thumbnails (webp)
│       └── plugins/     # static_model_policy (gear), static_audit (gear)
└── tests/               # integration suites plus common harness (fake provider, mock PDP)
```

**Structure Decision**: two crates under `gears/mini-chat/`, following the credstore / oagw SDK split.
The workspace gains the member `gears/mini-chat/mini-chat-sdk` and a `[workspace.dependencies]`
entry `mini-chat-sdk`, which the gear depends on.

## Complexity Tracking

No constitution violations.
