# Feature Specification: Mini Chat Gear (multi-tenant AI chat)

**Feature Branch**: `001-mini-chat-gear`

**Created**: 2026-10-03

**Status**: Draft

**Input**: User description: "Implement the `mini-chat` gear (multi-tenant AI chat) and its SDK crate in the gears-rust workspace. The contract is gears/mini-chat/docs (PRD.md, DESIGN.md, accepted ADRs 0001-0010, acceptance-criteria.md); it wins over any other workspace file. Every item of acceptance-criteria.md MUST be implemented AND covered by automated tests."

**Normative source**: `gears/mini-chat/docs/PRD.md`, `gears/mini-chat/docs/DESIGN.md`, `gears/mini-chat/docs/ADR/0001..0010`, `gears/mini-chat/docs/acceptance-criteria.md`. Where this spec summarizes a rule, the cited document holds the exact codes, values and payloads; on any disagreement the gear documents win.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Converse with an AI assistant in a private chat (Priority: P1)

An authenticated user creates a chat (optionally choosing a model and a title), sends a message and watches the assistant answer stream in real time. The conversation is persisted, listed with the most recent activity first, and can be resumed later. Nobody else, not even another user of the same tenant, can see it.

**Why this priority**: This is the core product value; every other capability builds on chats, turns and streaming.

**Independent Test**: Create a chat, stream a message against a mock OpenAI-compatible provider, verify the SSE event sequence, the persisted messages and the chat list ordering.

**Acceptance Scenarios**:

1. **Given** an authenticated user, **When** they create a chat without a model, **Then** the default model is resolved (first enabled `is_default`, else first enabled), the response is 201 with a `Location` header and `message_count = 0`.
2. **Given** a chat, **When** the user sends a message, **Then** the stream starts with `stream_started` (request_id, message_id, is_new_turn=true), relays `delta` events as they arrive, and ends with exactly one `done` carrying usage, `effective_model`, `selected_model` and `quota_decision`.
3. **Given** two chats, **When** the user sends a message in the older one, **Then** that chat is listed first.
4. **Given** another user's chat, **When** the user reads, updates, deletes or streams into it, **Then** the response is 404 with the chat resource type.
5. **Given** a completed turn, **When** the same `request_id` is sent again, **Then** the stored answer is replayed (`is_new_turn=false`, one delta, done) with no provider call and no quota or outbox change.

---

### User Story 2 - Stay within cost limits (Priority: P1)

Operators bound per-user spend with credit quotas per tier and period. A premium chat whose premium quota is exhausted is transparently downgraded to the standard tier; when all tiers are exhausted the request is rejected before any provider call. The user can check remaining quota at any time.

**Why this priority**: Unbounded LLM cost is unacceptable for a multi-tenant product; quota must be checked before every outbound call.

**Independent Test**: Seed `quota_usage` rows, stream messages and assert downgrade metadata in `done`, 429 responses with the quota scope, settlement of reserves and the quota status report.

**Acceptance Scenarios**:

1. **Given** an exhausted premium bucket, **When** a premium chat sends a message, **Then** `done` reports `quota_decision=downgrade`, `downgrade_from`, `downgrade_reason=premium_quota_exhausted` and the provider receives the standard model.
2. **Given** exhausted `total` buckets, **When** a message is sent, **Then** 429 `resource_exhausted` with `violations[0].subject = tokens` is returned and no SSE stream is opened.
3. **Given** a completed turn, **When** it is settled, **Then** reserves are released, actual credits (per-component ceil) are added to spent credits for the right buckets and exactly one usage outbox event is enqueued.
4. **Given** usage, **When** `GET /v1/quota/status` is called, **Then** tiers `premium` and `total` with daily and monthly periods are reported with warning/exhausted flags consistent with the stored usage.

---

### User Story 3 - Correct the last exchange (Priority: P2)

The user can retry, edit or delete only the latest finished turn. Retry and edit go through the full send pipeline with a new server-generated request id; a rejection leaves the previous answer intact.

**Why this priority**: Frequent recovery action; strict tail-only semantics keep the conversation linear.

**Independent Test**: Run retry/edit/delete against latest and non-latest turns, running turns, other users' turns, and concurrent mutations.

**Acceptance Scenarios**:

1. **Given** a non-latest turn, **When** it is retried, **Then** 409 `aborted` with `NOT_LATEST_TURN`.
2. **Given** a running turn, **When** it is mutated, **Then** 400 `failed_precondition` with `turn_state`/`STATE`.
3. **Given** the latest completed turn, **When** it is edited, **Then** a new turn with a new request id streams and the old turn returns 404 on the turn status API; original attachments are carried forward.

---

### User Story 4 - Ground answers in documents and images (Priority: P2)

The user uploads documents (indexed in a per-chat vector store) and images (thumbnails stored locally), references them in messages, and gets answers that use `file_search`, image input or code interpreter. Limits and kill switches are enforced; failures and abandoned uploads are recovered.

**Why this priority**: Document-aware answers are a primary differentiator but depend on the chat core.

**Independent Test**: Upload files to a mock Files/Vector Stores API, check statuses, limits, the provider request tools, and deletion/cleanup outbox rows.

**Acceptance Scenarios**:

1. **Given** a chat, **When** a PDF is uploaded and indexing completes, **Then** 201 with `status: ready`, and later turns send `file_search` with the chat vector store.
2. **Given** an image upload, **When** it succeeds, **Then** `status: ready` with a WebP thumbnail; sending it via `attachment_ids` adds an `input_image` item.
3. **Given** an attachment referenced by a message, **When** it is deleted, **Then** 409 `already_exists` with `resource_name = attachment_locked`.

---

### User Story 5 - Web search, reactions, models (Priority: P3)

Users opt in to web search per message (with citations, per-turn and daily limits, kill switch), react like/dislike to assistant messages, and browse the enabled model catalog.

**Why this priority**: Valuable additions on top of the core flows.

**Independent Test**: Stream with web search against a mock that emits web search events and annotations; set/remove reactions; list models.

**Acceptance Scenarios**:

1. **Given** `web_search.enabled=true`, **When** the provider performs more searches than the per-turn limit, **Then** the stream ends with `error{code: web_search_calls_exceeded}`.
2. **Given** a user message, **When** a reaction is set on it, **Then** 400 `failed_precondition` with `reaction_target`.
3. **Given** a disabled catalog model, **When** models are listed, **Then** it is absent and `GET /v1/models/{id}` returns 404.

---

### User Story 6 - Reliable background processing (Priority: P3)

Deleted chats have provider resources cleaned up, long chats are summarized, abandoned turns and uploads are recovered, and usage/audit events are published exactly once per turn.

**Why this priority**: Operational integrity; invisible to the user but required for billing and data hygiene.

**Independent Test**: Assert outbox rows and handler effects (cleanup provider calls, summary rows, orphan finalization) against the database and the mock provider.

**Acceptance Scenarios**:

1. **Given** a chat with attachments, **When** it is deleted, **Then** 204, sub-resources return 404, and the provider files and vector store are deleted asynchronously.
2. **Given** a running turn with stale progress, **When** the watchdog runs, **Then** it is finalized `failed`/`orphan_timeout` with an estimated settlement and one usage event.

### Edge Cases

- Client disconnects mid-stream: no SSE error is sent; the turn becomes `cancelled`, partial text is persisted if non-empty, and quota is settled with the estimated formula.
- Provider 429, provider 5xx, provider SSE `error`, `response.failed`, `response.incomplete`: mapped to `rate_limited` / `provider_error` / completed per DESIGN "Provider Event Translation"; messages sanitized of provider identifiers.
- Same `request_id` reused for running/failed/cancelled/deleted turns → 409 `request_id_conflict`; replay check precedes the parallel-turn check.
- Message estimate above `max_input_tokens` → 400 `INPUT_TOO_LONG`; mandatory context above budget → 400 `CONTEXT_BUDGET_EXCEEDED`; history truncated by whole turns.
- Malformed OData filter/cursor/limit=0 → 400 with resource type `gts.cf.core.odata.query.v1~`; limit > 100 clamped.
- Duplicate, foreign, not-ready or too many `attachment_ids` → 400 `invalid_attachment` before any provider call; images on a non-vision effective model → 400 `VISION_NOT_SUPPORTED`.
- Upload size/type/limit violations (`FILE_TOO_LARGE`, `UNSUPPORTED_CONTENT_TYPE`, 429 `document_limit`/`storage_limit`), indexing failure (503 + row `failed`/`indexing_failed`), indexing timeout (201 `uploaded`, background completion).
- Concurrent mutations of the same turn: exactly one wins, the other gets 409 `GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`.

## Requirements *(mandatory)*

### Functional Requirements

Principles & constraints

- **FR-001**: System MUST scope every resource (chat, message, turn, attachment, reaction, quota) to the owning tenant and user; foreign or soft-deleted resources MUST return 404 with the matching `gts.cf.core.mini_chat.*.v1~` resource type; PDP deny MUST be 403 `AUTHZ_DENIED`, PDP failure 503 with `Retry-After: 5`.
- **FR-002**: System MUST enforce the context window budget for the input message (`INPUT_TOO_LONG`) and for the assembled request (`CONTEXT_BUDGET_EXCEEDED`), computed for the effective model after downgrade.
- **FR-003**: System MUST relay provider streaming events to the client as they arrive through a bounded channel, never buffering the full answer.
- **FR-004**: The chat's model MUST be immutable after creation; only quota/kill-switch/disabled-model downgrades change the per-turn effective model.
- **FR-005**: Quota preflight (cascade, daily tool quotas) and reserve MUST complete before any outbound provider call.

Chat CRUD & messages

- **FR-006**: System MUST provide create/get/list/update(title only)/delete chat with model validation (`INVALID_MODEL`) and title validation (trim, 1–255, `INVALID_TITLE`); `title` omitted when absent; delete is a soft delete (204, second delete 404).
- **FR-007**: List endpoints MUST support OData `$filter`/`$orderby` (chats: `updated_at`, `id`, `title`; messages: `created_at`, `id`, `role`), cursor pagination (default 20, max 100 clamped), accepted-and-ignored `$select`, and reject malformed input with 400.
- **FR-008**: `chats.updated_at` MUST be bumped on create, rename, delete and in the same transaction as every sent message, retry and edit; default chat order `updated_at desc, id`.
- **FR-009**: Each message MUST expose `id`, `request_id` (non-null), `role`, `content`, `attachments` (always an array, non-deleted only), `my_reaction` (always present, nullable), and optional `model`, `input_tokens`, `output_tokens` (omitted when absent/0); `message_count` and chronological order MUST be correct across turns.

Streaming

- **FR-010**: `POST /v1/chats/{id}/messages:stream` MUST stream SSE correlated by a client or server `request_id` (any UUID version).
- **FR-011**: Preflight validation (content, attachments, image count, kill switches, vision, limits, quota) MUST run before any provider call and return JSON Problem errors, not SSE.
- **FR-012**: On completion the assistant message, usage tokens, effective model and turn state MUST be persisted and settled in one finalization transaction with usage and audit outbox events.
- **FR-013**: The SSE contract MUST follow `stream_started ping* (delta|tool)* citations? (done|error)` with payloads exactly as DESIGN §3.3; `done` exposes usage and quota/downgrade data and `quota_warnings` without internal identifiers; `error` is terminal with a sanitized `{code, message}`.

Idempotency, parallel turns, mutations, lifecycle

- **FR-014**: Replays of completed turns MUST be side-effect free; reuse of a request id for non-completed or deleted turns MUST give 409 `request_id_conflict`; the replay check MUST precede the parallel-turn guard.
- **FR-015**: At most one running turn per chat (409 `turn_already_running`); a new turn is accepted once the previous one is terminal.
- **FR-016**: Retry/edit/delete MUST act only on the latest terminal turn, run full preflight before replacing the turn, use a new server request id, resolve concurrent mutations deterministically, copy non-deleted attachments, and preserve `web_search_enabled`.
- **FR-017**: Turn state machine `running → completed|failed|cancelled` MUST be CAS-guarded; Turn Status API maps states to `running|done|error|cancelled`; cancelled turns persist non-empty partial content; failed turns store `error_code`.

Attachments

- **FR-018**: Upload/get/delete of documents and images MUST validate size (per kind and per model), MIME type, per-chat document count and storage limits, kill switches, and the chat model; documents are indexed synchronously up to 25 s, then in the background up to 10 minutes.
- **FR-019**: Attachments MUST be exposed to provider tools: ready documents → `file_search` with the chat vector store; ready XLSX → `code_interpreter` with file ids; images in `attachment_ids` → `input_image`.
- **FR-020**: Deletion MUST soft-delete and enqueue provider cleanup via the outbox (idempotent re-delete 204, 409 `attachment_locked` when referenced); the upload reaper MUST fail abandoned uploads (`upload_abandoned`).

Models, reactions, quota

- **FR-021**: Models API MUST list/get only enabled catalog entries with fields `model_id`, `display_name`, `tier`, `multiplier_display`, optional `description`, `multimodal_capabilities`, `context_window`.
- **FR-022**: Reactions MUST be set/replaced/removed idempotently on assistant messages only (`reaction_target`/`STATE` otherwise; `INVALID_REACTION` for bad values).
- **FR-023**: Quota status MUST report `premium` and `total` tiers per daily/monthly period consistent with stored usage, omitting periods with limit ≤ 0.
- **FR-024**: Reserve-before-execute, premium→standard downgrade (kill switches, disabled models), credits computed per model with per-component ceiling, and overshoot tolerance MUST follow DESIGN §5.
- **FR-025**: Every terminal outcome of a reserved turn MUST settle quota exactly once (actual/estimated per DESIGN §5.7–5.9) and enqueue exactly one usage event with `dedupe_key = {tenant}/{turn}/{request}`.

Context, errors, web search, background

- **FR-026**: Context assembly MUST be system prompt + guards, thread summary (with preamble), recent messages (limit, snapshot boundary, uncompressed), current message + images; truncation drops oldest whole turns, then the summary.
- **FR-027**: Tool availability and guard texts MUST be reflected in the provider request (file_search, web_search, code_interpreter, max_tool_calls, include outputs).
- **FR-028**: All REST errors MUST be canonical Problem objects per ADR-0004; SSE errors MUST use the streaming error codes; provider ids, URLs and keys MUST be sanitized out of client-visible messages.
- **FR-029**: Web search MUST be reported via `tool` events, cited via `citations` (`source: web`), counted in usage, limited per turn (`web_search_calls_exceeded`) and per day (429 `web_search`), and rejected under the kill switch (400 `FEATURE_DISABLED`).
- **FR-030**: Chat deletion MUST enqueue a chat cleanup outbox message that deletes provider files then the vector store with idempotent retries.
- **FR-031**: Thread summaries MUST be triggered at finalization (threshold or truncation), generated via the outbox, committed with frontier CAS, retried/dead-lettered on failure, and invalidated by mutations that cover the summarized range.
- **FR-032**: The gear MUST register its OAGW upstreams/routes at start, use the bundled static model-policy and audit plugins via types-registry, and accept configuration keys/defaults per DESIGN Appendix B.

Testing (explicitly requested)

- **FR-033**: Every item of `acceptance-criteria.md` MUST be covered by automated tests that pass: unit tests for domain logic (credits, cascade, estimation, truncation, error mapping, sanitization, SSE translation) and integration tests that drive the HTTP/SSE API against an SQLite database and a mock OpenAI-compatible provider, asserting HTTP responses, SSE events, database rows and provider requests.

### Key Entities

- **Chat**: owner tenant/user, selected model (immutable), optional title, timestamps, soft delete.
- **Turn**: one user-initiated execution keyed by `(chat, request_id)`; state, reserve fields, effective model, error code, tool counters.
- **Message**: user/assistant content tied to a turn's request id; token usage and effective model on assistant messages.
- **Attachment**: document or image with status, purposes, provider file, thumbnail, cleanup state.
- **Message–attachment link**: immutable association created at send/retry/edit.
- **Thread summary**: compressed history with a frontier.
- **Chat vector store**: one per chat.
- **Quota usage bucket**: per user, period and bucket (`total`, `tier:premium`) counters.
- **Reaction**: one like/dislike per user per assistant message.
- **Outbox events**: usage, audit, attachment cleanup, chat cleanup, thread summary.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of acceptance-criteria items have at least one passing automated test.
- **SC-002**: 0 cross-user or cross-tenant reads succeed in the isolation tests.
- **SC-003**: First streamed text reaches the client before the provider finishes generating (streaming verified with a delayed mock).
- **SC-004**: 0 provider calls occur for requests rejected at preflight or replayed.
- **SC-005**: Every reserved turn produces exactly 1 usage event in all tested outcomes (completed, failed, cancelled, orphan).
- **SC-006**: The example server builds with the required feature set and serves all 19 routes.

## Assumptions

- The bundled static model policy plugin supplies the catalog, kill switches and limits; no external billing service is needed.
- The license gate uses the platform base license feature (ADR-0008) as interim gate.
- Only the `openai_responses` adapter is exercised end to end; other adapter kinds are accepted in configuration and implemented on a best-effort basis (ADR-0005 notes they have no E2E coverage).
- MCP, document summaries, hard purge, image daily quotas and other "Not implemented"/"Future" items in the ADRs stay out of scope.
- Leader election is a no-op elector unless built with the `k8s` feature.
