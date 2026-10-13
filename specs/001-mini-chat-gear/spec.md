# Feature Specification: Mini Chat Gear (multi-tenant AI chat) and its SDK

**Feature Branch**: `001-mini-chat-gear`

**Created**: 2026-10-03

**Status**: Draft

**Input**: User description: "Implement the `mini-chat` gear (multi-tenant AI chat) and create its SDK
crate. The contract is gears/mini-chat/docs (PRD.md, DESIGN.md, ADR/*, acceptance-criteria.md) plus
docs/api/api.json. Every item of acceptance-criteria.md MUST be implemented AND covered by automated
tests (tests are mandatory for this feature)."

**Normative references**: `gears/mini-chat/docs/PRD.md`, `gears/mini-chat/docs/DESIGN.md`,
`gears/mini-chat/docs/ADR/0001..0010`, `gears/mini-chat/docs/acceptance-criteria.md`, and the
mini-chat subset of the generated OpenAPI (`docs/api/api.json`, copied to
`specs/001-mini-chat-gear/openapi-mini-chat.json`). Where this spec summarises a rule, the
referenced document holds the exact codes and values; on conflict the referenced documents win.

## User Scenarios & Testing *(mandatory)*

Tests are **mandatory** for this feature: every acceptance-criteria item (AC-01 … AC-42 below) MUST
be covered by at least one automated test that passes.

### User Story 1 - Manage chats (Priority: P1)

A signed-in user creates a chat (optionally choosing a model and a title), lists their chats ordered
by latest activity with filtering and paging, opens one chat, renames it and deletes it. Nobody else
can see these chats.

**Why this priority**: every other capability lives inside a chat.

**Independent Test**: create/list/get/rename/delete through the REST API with two users and two
tenants; verify statuses, bodies, ordering, paging, validation and 404 masking.

**Acceptance Scenarios**:

1. **Given** a catalog with a default model, **When** the user creates a chat without a model,
   **Then** 201 is returned with `Location: /mini-chat/v1/chats/{id}`, the default model and
   `message_count = 0`.
2. **Given** an unknown or disabled model, **When** creating a chat, **Then** 400
   `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`.
3. **Given** a whitespace-only or 256-character title, **When** creating or renaming, **Then** 400
   `INVALID_TITLE`.
4. **Given** several chats, **When** listing with `$filter`, `$orderby`, `limit` and `cursor`,
   **Then** the page follows the query; a malformed cursor or unknown field is 400 with the OData
   resource type; `limit > 100` is clamped; `limit=0` is 400 `INVALID_LIMIT`.
5. **Given** a rename body that also contains `model`, **When** patching, **Then** only the title
   changes and `updated_at` moves forward.
6. **Given** a chat of another user or tenant, **When** reading, renaming or deleting it, **Then**
   404 `not_found` with the chat resource type.
7. **Given** a deleted chat, **When** deleting it again or reading any sub-resource, **Then** 404.
8. **Given** two chats, **When** a message is sent in the older one, **Then** it lists first.

### User Story 2 - Send a message and receive a streamed answer (Priority: P1)

The user sends a message and receives the assistant answer as an SSE stream, correlated by a request
id. The answer and its usage are stored; reconnecting with the same request id replays the stored
answer without a new provider call.

**Why this priority**: this is the core product value.

**Independent Test**: with a mock OpenAI-compatible provider behind the outbound gateway, send a
message and assert the SSE event sequence, the provider request shape, the persisted messages,
turn, quota rows and outbox rows.

**Acceptance Scenarios**:

1. **Given** a chat, **When** sending `{content}`, **Then** the stream is
   `stream_started` (request_id, message_id, is_new_turn=true) → `delta`* → `done` (usage,
   effective_model, selected_model, quota_decision, quota_warnings) and closes.
2. **Given** a completed turn, **When** the same `request_id` is sent again, **Then** the stream is
   `stream_started(is_new_turn=false)` → one `delta` with the stored text → `done`; no provider call,
   no quota or outbox change.
3. **Given** a running turn, **When** any new message is sent, **Then** 409 `aborted`
   `turn_already_running`; reusing the running/failed/cancelled/deleted turn's request id gives 409
   `request_id_conflict`; replay is checked before the parallel-turn guard.
4. **Given** empty content, invalid attachment ids, too many images, a too-long message or an
   exhausted quota, **When** sending, **Then** a JSON `Problem` is returned before any provider call
   and no stream opens.
5. **Given** a provider error mid-stream, **When** streaming, **Then** a terminal
   `error{code,message}` with a sanitized message; the turn is `failed`.
6. **Given** a client disconnect, **When** the stream is dropped, **Then** the turn becomes
   `cancelled`, partial text (if any) is persisted, and estimated settlement is applied.

### User Story 3 - Browse history, turn status and reactions (Priority: P1)

The user lists messages of a chat (filtering, ordering, paging), checks a turn's status after a
disconnect, and likes/dislikes assistant answers.

**Independent Test**: after several turns, list messages and verify the message contract
(`request_id`, `attachments`, `my_reaction`, `model`, token counts), ordering and counts; query turn
status; set/replace/remove reactions.

**Acceptance Scenarios**:

1. **Given** two completed turns, **When** listing messages, **Then** four messages in
   chronological order, each with non-null `request_id`, an `attachments` array and `my_reaction`.
2. **Given** an assistant message, **When** `PUT reaction {like}` then `{dislike}`, **Then** the
   reaction is replaced; `DELETE` is idempotent (204); a user message gives 400
   `failed_precondition` `reaction_target`; an invalid value gives 400 `INVALID_REACTION`.
3. **Given** a turn, **When** `GET /turns/{request_id}`, **Then** state is one of
   `running|done|error|cancelled` with `assistant_message_id` / `error_code` as specified.

### User Story 4 - Retry, edit and delete the last turn (Priority: P2)

The user retries or edits the latest terminal turn (getting a new streamed answer with a new
request id) or deletes it.

**Acceptance Scenarios**:

1. **Given** the latest turn is completed, **When** retrying, **Then** a new stream with a new
   request id; the old turn is soft-deleted with `replaced_by_request_id`.
2. **Given** a non-latest turn, **When** mutating it, **Then** 409 `NOT_LATEST_TURN`; a running turn
   gives 400 `turn_state`; another user's turn gives 403/404 as specified.
3. **Given** exhausted quota, **When** retrying, **Then** 429 and the previous turn stays intact.
4. **Given** two concurrent mutations, **Then** one succeeds and the other gets a deterministic 409.
5. **Given** an original message with attachments, **When** retrying/editing, **Then** non-deleted
   attachment links are copied to the new user message.

### User Story 5 - Attachments: documents and images (Priority: P2)

The user uploads documents (indexed into a per-chat vector store) and images (with thumbnails),
checks their status, references them in messages and deletes unreferenced ones.

**Acceptance Scenarios**:

1. **Given** a PDF, **When** uploading, **Then** 201 `status: ready` after the provider reports
   indexing completed; `file_search` is offered to the model on later turns.
2. **Given** an image, **When** uploading, **Then** 201 `status: ready` with a WebP `img_thumbnail`.
3. **Given** an unsupported type, an oversized file or a per-chat limit, **Then** 400
   `UNSUPPORTED_CONTENT_TYPE` / 400 `FILE_TOO_LARGE` / 429 `document_limit` / `storage_limit`.
4. **Given** indexing failure, **Then** 503 with `Retry-After: 10` and the row is `failed`
   (`indexing_failed`); indexing still running at 25 s returns `uploaded` and finishes in background.
5. **Given** an attachment referenced by a message, **When** deleting, **Then** 409
   `attachment_locked`; an unreferenced one returns 204, and again 204.
6. **Given** an abandoned upload, **Then** the upload reaper marks it `failed` (`upload_abandoned`).

### User Story 6 - Quota, models and policy (Priority: P2)

Users see enabled models and their quota status; usage is limited per tier and period with
automatic premium→standard downgrade; tool quotas and kill switches apply.

**Acceptance Scenarios**:

1. **Given** premium daily quota exhausted, **When** sending in a premium chat, **Then** the turn
   runs on the standard model and `done` carries `quota_decision=downgrade`, `downgrade_from`,
   `downgrade_reason=premium_quota_exhausted`.
2. **Given** all tiers exhausted, **Then** 429 `resource_exhausted` with subject `tokens`.
3. **Given** a daily web-search quota exhausted and `web_search.enabled=true`, **Then** 429 with
   subject `web_search`.
4. **Given** `GET /v1/quota/status`, **Then** tiers `premium` and `total` with daily/monthly periods
   consistent with `quota_usage`.
5. **Given** `GET /v1/models`, **Then** only enabled models without internal fields.

### User Story 7 - Background reliability (Priority: P3)

Chat deletion cleans provider resources through the outbox; thread summaries compress long chats;
the orphan watchdog fails stuck turns; usage and audit events are published exactly once per turn.

### Edge Cases

- Stream request with a `request_id` of a soft-deleted turn → 409 `request_id_conflict`.
- Provider sends `response.incomplete` → turn `completed`, `done` sent.
- Provider 429 mid-stream → SSE `error{code: rate_limited}`.
- Web search started more than `web_search_max_calls_per_message` times → SSE
  `error{code: web_search_calls_exceeded}`, turn `failed`, estimated settlement.
- Chat model removed from catalog → 400 `INVALID_MODEL` on stream/upload/retry/edit; disabled model
  → downgrade `model_disabled`.
- Image turn downgraded to a model without vision → 400 `VISION_NOT_SUPPORTED` before provider call.
- Non-UUID path parameter → 400 `invalid_path_params`; malformed JSON → 400; schema mismatch → 422;
  missing JSON content type → 415.
- PDP failure → 503 with `Retry-After: 5`; PDP deny → 403.
- Cancellation with no streamed text → turn `cancelled`, no assistant message.

## Requirements *(mandatory)*

### Functional Requirements

Each requirement points at the governing document section; acceptance items (AC-xx) refer to the
numbered list in "Acceptance Criteria Traceability" below.

**Packaging**

- **FR-001**: The gear crate (`cf-gears-mini-chat`, lib `mini_chat`) MUST register the `mini-chat`
  gear with the runtime (deps: types-registry, authn-resolver, authz-resolver, oagw; capabilities:
  db, rest, stateful) and keep the modules `infra::plugins::static_audit` and
  `infra::plugins::static_model_policy`, each registering a bundled plugin gear
  (`static-mini-chat-audit-plugin`, `static-mini-chat-model-policy-plugin`).
- **FR-002**: An SDK crate (`cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) MUST be a workspace member
  and a dependency of the gear; it holds the plugin contracts (`MiniChatModelPolicyPluginClientV1`,
  `MiniChatAuditPluginClientV1`, their GTS specs), the policy/catalog/limits/kill-switch types,
  `UsageEvent` and audit event types.

**Chats (DESIGN §3.3, PRD §5.1)**

- **FR-010**: Chat CRUD with title/model validation, default-model algorithm, `Location` header,
  `title` omitted when absent, `message_count`, soft delete (second delete 404). (AC-06)
- **FR-011**: `GET /chats` cursor pagination with OData `$filter`/`$orderby` on `updated_at`, `id`,
  `title`, `$select` accepted and ignored, default order `updated_at desc, id`, errors per ADR-0004.
  (AC-07)
- **FR-012**: `chats.updated_at` bumped on create, rename and in the transaction of every sent
  message, retry and edit. (AC-08)

**Messages (DESIGN §3.3)**

- **FR-020**: `GET /chats/{id}/messages` cursor pagination with OData on `created_at`, `id`, `role`,
  default `created_at asc`. (AC-09)
- **FR-021**: Message DTO always carries `id`, `request_id`, `role`, `content`, `attachments` (non-
  deleted only), `my_reaction`, `created_at`; `model`, `input_tokens`, `output_tokens` only when
  present/non-zero. (AC-10)
- **FR-022**: `message_count` and chronological order are correct across turns, mutations and
  deletions. (AC-11)

**Streaming (DESIGN §3.3 Streaming Contract, SSE Event Definitions, Provider Event Translation)**

- **FR-030**: `POST /chats/{id}/messages:stream` streams SSE correlated by a client or server
  generated `request_id`. (AC-12)
- **FR-031**: Preflight validation (content, attachment ids, image count, kill switches, vision,
  input length, context budget, quota) runs before any provider call and returns a JSON `Problem`.
  (AC-13, AC-03, AC-05)
- **FR-032**: On provider completion the assistant message, usage, turn finalization, quota
  settlement, usage event and audit event are persisted in one transaction before `done`. (AC-14)
- **FR-033**: Full SSE event contract: `stream_started`, `ping` (before first content only),
  `delta`, `tool`, `citations` (at most one, before terminal), `done`, `error`; ordering per DESIGN.
  (AC-15)
- **FR-034**: `done` exposes usage (token counts only), effective/selected model, quota decision,
  optional downgrade fields and `quota_warnings`, never internal ids. (AC-16)
- **FR-035**: `error` is terminal and carries a sanitized message. (AC-17, AC-36)
- **FR-036**: Streaming responses are relayed without buffering through bounded channels. (AC-03)

**Idempotency, parallel turns, lifecycle (DESIGN §4 Turn Lifecycle)**

- **FR-040**: Replay of a completed request id is side-effect free; conflicting reuse is 409
  `request_id_conflict`; replay is checked before the parallel-turn guard. (AC-18..AC-20)
- **FR-041**: At most one running turn per chat (409 `turn_already_running`); a new turn is accepted
  once the previous one is terminal. (AC-21, AC-22)
- **FR-042**: Turn state machine running → completed/failed/cancelled with CAS guard; partial text on
  cancel persisted when non-empty; null content cases handled. (AC-27, AC-28)
- **FR-043**: Turn status endpoint with state mapping and optional fields. (AC-27)

**Turn mutations (DESIGN §3.9)**

- **FR-050**: Retry/edit/delete only on the latest terminal turn; errors `NOT_LATEST_TURN`,
  `turn_state`, `GENERATION_IN_PROGRESS`, 403 for foreign requester. (AC-23, AC-25)
- **FR-051**: Mutations run the full send pipeline (preflight before commit, new server request id,
  context budget, quota, attachments, images) and stream like `messages:stream`. (AC-24)
- **FR-052**: Mutations copy non-deleted attachment links and carry `web_search_enabled`; summary
  invalidation on mutation. (AC-26, AC-41)

**Attachments (DESIGN §3.3, §3.6 File Upload, B.8, B.9.5)**

- **FR-060**: Upload/get/delete for documents and images with MIME allowlist, size limits, per-chat
  document/storage limits, code-interpreter purpose routing, `disable_images`, thumbnails. (AC-29)
- **FR-061**: Synchronous indexing with 25 s deadline, background continuation up to 10 minutes,
  failure → `indexing_failed`, 503. (AC-30)
- **FR-062**: Ready documents enable `file_search` (chat vector store), ready XLSX enable
  `code_interpreter` (container file ids), images go as `input_image` items. (AC-31)
- **FR-063**: Attachment deletion via outbox, `attachment_locked`, idempotent delete, upload reaper.
  (AC-32)

**Models, reactions, quota (DESIGN §3.3, §5)**

- **FR-070**: Models list/get of enabled catalog entries only, without internal fields. (AC-33)
- **FR-071**: Reactions on assistant messages only, idempotent. (AC-34)
- **FR-072**: Quota status per tier (`premium`, `total`) and period with warning/exhausted flags.
  (AC-35)
- **FR-073**: Reserve-before-execute on every provider call; reserve re-check; tier downgrade;
  per-model credit multipliers; settlement exactly once (actual/estimated); usage published once.
  (AC-05, AC-37..AC-40 numbering per traceability list)

**Context, errors, web search, cleanup, authorization**

- **FR-080**: Context assembly: system prompt + tool guards, thread summary (with preamble),
  recent messages (limit, oldest whole turns dropped first), current message + images, within the
  budget; tool list per availability rules. (AC-02, AC-41, AC-42)
- **FR-081**: All errors map to the ADR-0004 canonical contract; provider errors sanitized.
- **FR-082**: Web search: tool inclusion, guard, citations (`source: web`), per-turn and daily
  limits, kill switch, call counts accounted.
- **FR-083**: Chat deletion enqueues cleanup; cleanup handlers delete provider files and the vector
  store with retries; thread summary worker with CAS frontier; orphan watchdog.
- **FR-084**: Every operation is authorized through the PDP and scoped to tenant + owner.

**Provider integration (DESIGN §3.5, §4 Provider Request Metadata, ADR-0005)**

- **FR-090**: Provider entries from config are provisioned as OAGW upstreams + routes at start; chat
  requests go to `{alias}{api_path}`; RAG requests to `{alias}/v1/...` (openai) or
  `{alias}/openai/...?api-version=` (azure). The OpenAI Responses request carries `model`, `input`,
  `instructions`, `stream: true`, `max_output_tokens`, `tools`, `max_tool_calls`, `user`,
  `metadata`, `include` (when code interpreter).

### Key Entities

- **Chat**: owned by one user in one tenant; selected model (immutable), title, timestamps, soft
  delete.
- **Message**: user/assistant content of a turn, request id, effective model, token usage,
  compression flag, soft delete.
- **Turn**: one user-initiated provider invocation keyed by `(chat, request_id)`; state, reserve
  snapshot, effective model, tool counters, error code, progress timestamp.
- **Attachment**: document or image in a chat; status, purposes, provider file reference (internal),
  thumbnail, cleanup state.
- **Message attachment link**: which attachments a message referenced.
- **Thread summary**: compressed earlier history with an inclusive frontier.
- **Chat vector store**: provider vector store bound to one chat.
- **Quota usage**: per user, period, bucket (`total`, `tier:premium`) spent/reserved credits and
  counters.
- **Reaction**: like/dislike of one user on one assistant message.
- **Usage event / audit event**: outbox payloads published to the policy and audit plugins.

## Acceptance Criteria Traceability *(each item MUST be implemented and tested)*

| AC | acceptance-criteria.md item |
|----|-----------------------------|
| AC-01 | Tenant and owner isolation enforced on every resource |
| AC-02 | Context window budget enforced (input message and assembled request) |
| AC-03 | Streaming responses are never buffered before relaying |
| AC-04 | A chat's model is immutable once set |
| AC-05 | Quota is checked before any outbound provider call |
| AC-06 | Chat create/get/list/update/delete lifecycle with model and title validation |
| AC-07 | List filtering, ordering, pagination, malformed input validation |
| AC-08 | Chat ordering reflects most recent activity |
| AC-09 | List messages with filtering, ordering, pagination |
| AC-10 | Message response contract: identity, attachments, reaction fields always present |
| AC-11 | Message count and chronological ordering across turns |
| AC-12 | Send-message streams a response correlated by request id |
| AC-13 | Preflight validation before any provider call |
| AC-14 | Assistant message and usage persisted once a stream completes |
| AC-15 | Full streaming event contract and ordering |
| AC-16 | `done` exposes usage and quota/downgrade outcome without internal ids |
| AC-17 | Error event is terminal with sanitized message |
| AC-18 | Replay returns stored result without side effects |
| AC-19 | Conflicting reuse of a request id rejected consistently |
| AC-20 | Replay checked before the parallel-turn guard |
| AC-21 | Only one turn per chat at a time |
| AC-22 | New turn accepted once previous is terminal |
| AC-23 | Retry/edit/delete only on latest terminal turn |
| AC-24 | Mutation goes through full send pipeline with a new request id |
| AC-25 | Concurrent mutations resolve deterministically |
| AC-26 | Mutated turns carry forward attachment and tool-usage history |
| AC-27 | Turn state machine consistent end to end |
| AC-28 | Partial and null-content cases on cancellation/failure |
| AC-29 | Attachment upload/get/delete with size, type, per-chat limit validation |
| AC-30 | Asynchronous indexing lifecycle incl. provider failure and timeout |
| AC-31 | Attachments made available to the relevant provider tools |
| AC-32 | Cleanup and abandoned-upload recovery under failure |
| AC-33 | Models list/get reflects only enabled entries, no internal fields |
| AC-34 | Reactions on assistant messages only, idempotently |
| AC-35 | Quota status accurate and consistent with usage |
| AC-36 | Reserve-before-execute on every provider call |
| AC-37 | Tier downgrade when a higher tier is exhausted |
| AC-38 | Credits and tokens accounted per model and tier |
| AC-39 | Every terminal outcome settles quota exactly once |
| AC-40 | Usage published reliably and exactly once per turn |
| AC-41 | System prompt, summary, recent history assembled and truncated deterministically |
| AC-42 | Tool availability and guidance reflected in the assembled request |
| AC-43 | All errors map to the canonical contract across REST and streaming |
| AC-44 | Provider error details sanitized |
| AC-45 | Web search reported, cited, accounted, quota-limited |
| AC-46 | Chat deletion triggers reliable background cleanup |
| AC-47 | Thread summary generation, failure/retry, mutation invalidation |
| AC-48 | Every operation scoped to its owner; foreign access rejected consistently |

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of the 48 acceptance items above are covered by at least one passing automated
  test.
- **SC-002**: A user can create a chat and receive the first streamed answer fragment within the
  provider's own latency plus less than 50 ms of added in-gear delay.
- **SC-003**: Zero cross-tenant or cross-user reads or writes in isolation tests.
- **SC-004**: Every turn that reserved quota produces exactly one settlement and one usage event in
  all tested outcomes (completed, failed, cancelled, orphaned).
- **SC-005**: Replaying a completed request causes zero provider calls and zero quota or event
  changes.
- **SC-006**: The example server builds with the required feature set and starts with the
  mini-chat configuration.

## Assumptions

- The platform pieces (gateway, authn/authz static plugins, single-tenant resolver, OAGW, credstore,
  types-registry) behave as in the rest of the workspace; the bundled static plugins are used.
- The LLM/RAG provider is OpenAI-compatible (`kind: openai_responses`, storage `openai` or `azure`);
  other adapter kinds are implemented to the extent the documents describe, without E2E coverage.
- Kill switches and limits are static deployment configuration.
- Deferred items (MCP, document summaries, hard purge, chunk cap, image quotas, snapshot cache,
  knowledge-search iteration billing) are out of scope, as recorded in ADR-0006..0009.
- License gate: routes require the platform base license feature (ADR-0008).
