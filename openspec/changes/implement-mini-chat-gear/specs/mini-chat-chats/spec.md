## ADDED Requirements

### Requirement: Route prefix and resource types
All routes SHALL be served under the gear `url_prefix` (default `/mini-chat`) with paths `/v1/...`. Not-found errors SHALL be canonical `not_found` Problems (404) whose `context.resource_type` is `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~` for the missing resource.

#### Scenario: Unknown chat
- **WHEN** `GET /mini-chat/v1/chats/{random-uuid}` is called
- **THEN** the response is 404 with `context.resource_type = gts.cf.core.mini_chat.chat.v1~`

### Requirement: Create chat
`POST /v1/chats` SHALL accept `{title?, model?}`. A present `title` SHALL be trimmed and MUST be 1–255 characters, else 400 `invalid_argument` with `field_violations[title].reason = INVALID_TITLE` (validated before authorization and model lookup). An absent model SHALL resolve to the first enabled catalog entry with `preference.is_default = true`, else the first enabled entry. An unknown or disabled model SHALL return 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`. Success SHALL return 201 with `ChatDetail` and `Location: /mini-chat/v1/chats/{id}`.

#### Scenario: Default model
- **WHEN** a chat is created without `model`
- **THEN** the response is 201 and `model` is the default catalog model

#### Scenario: Disabled model rejected
- **WHEN** a chat is created with a disabled or unknown model id
- **THEN** the response is 400 with `INVALID_MODEL`

#### Scenario: Whitespace title rejected
- **WHEN** a chat is created with `title: "   "` or a 256-character title
- **THEN** the response is 400 with `INVALID_TITLE`

### Requirement: ChatDetail shape
`ChatDetail` SHALL contain `id`, `model`, `title` (omitted when the chat has no title, never `null`), `is_temporary` (false), `message_count`, `created_at`, `updated_at` (RFC 3339) and SHALL NOT contain `user_id` or `tenant_id`. `message_count` SHALL count non-deleted messages of the chat.

#### Scenario: Untitled chat
- **WHEN** a chat without title is read
- **THEN** the JSON has no `title` key

### Requirement: Get, update and delete chat
`GET /v1/chats/{id}` SHALL return the `ChatDetail`. `PATCH /v1/chats/{id}` SHALL require a string `title` (schema mismatch → 422 from the JSON extractor), trim it, validate 1–255 characters (400 `INVALID_TITLE`), ignore unknown fields, never change `model`, bump `updated_at` and return 200 `ChatDetail`. `DELETE /v1/chats/{id}` SHALL soft-delete the chat (204), mark its attachments `cleanup_status = pending` and enqueue a chat-cleanup outbox message in the same transaction; a second delete SHALL return 404. Missing, soft-deleted or foreign chats SHALL return 404.

#### Scenario: Model is immutable
- **WHEN** `PATCH /v1/chats/{id}` is sent with `{"title":"Renamed","model":"other"}`
- **THEN** the response is 200, the title is `Renamed` and `model` is unchanged

#### Scenario: Double delete
- **WHEN** a chat is deleted twice
- **THEN** the first call returns 204 and the second returns 404

### Requirement: List chats
`GET /v1/chats` SHALL return `{items: [ChatDetail], page_info: {limit, next_cursor, prev_cursor}}` of non-deleted chats of the caller, ordered by `updated_at desc, id desc` by default, with `limit` default 20 clamped to 100 (`limit=0` → 400 `INVALID_LIMIT`), opaque `cursor`, `$filter` and `$orderby` over `updated_at`, `id`, `title`. Malformed filters, unknown fields and malformed cursors SHALL return 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~`. `$select` SHALL be accepted and ignored. `id` filters SHALL accept the quoted UUID literal (`id eq '<uuid>'`), and ordering by `title` SHALL paginate over untitled chats (NULL titles sort as empty strings). `chats.updated_at` SHALL be bumped on create, rename, delete and in the transaction of every sent message, retry and edit.

#### Scenario: Activity ordering
- **WHEN** a message is sent in the older of two chats
- **THEN** that chat is listed first

#### Scenario: Pagination
- **WHEN** three chats exist and `limit=2`
- **THEN** the first page has 2 items and a `next_cursor` that returns the remaining chat

#### Scenario: Quoted id filter and title ordering
- **WHEN** `$filter=id eq '<chat id>'` is sent, or `$orderby=title asc` pages through titled and untitled chats
- **THEN** the filter returns exactly that chat and paging returns every chat once

#### Scenario: Bad filter
- **WHEN** `$filter=bogus eq 1` is sent
- **THEN** the response is 400 with `context.resource_type = gts.cf.core.odata.query.v1~`

### Requirement: Models API
`GET /v1/models` SHALL return `{items: [...]}` of enabled catalog entries only, each with `model_id`, `display_name`, `tier` (`standard`|`premium`), `multiplier_display`, `description` (omitted when empty), `multimodal_capabilities`, `context_window`, and no provider, multiplier, `max_output`, `is_default` or policy fields. `GET /v1/models/{id}` SHALL return the same projection or 404 (`gts.cf.core.mini_chat.model.v1~`) when disabled or missing.

#### Scenario: Disabled model hidden
- **WHEN** a catalog entry has `enabled: false`
- **THEN** it is absent from `GET /v1/models` and `GET /v1/models/{id}` returns 404

### Requirement: Tenant and owner isolation
Every chat-scoped operation SHALL be authorized through the AuthZ resolver (PEP) with the chat resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, and every query SHALL be scoped by `tenant_id` and the owner `user_id` of the security context. A chat of another user or tenant SHALL be indistinguishable from a missing chat (404) for read, update, delete, messages, attachments, turns, reactions and streaming. PDP denial SHALL return 403 `permission_denied`; PDP evaluation failure SHALL return 503 with `Retry-After: 5`.

#### Scenario: Foreign chat
- **WHEN** user B reads, renames, deletes, lists messages of or streams into user A's chat
- **THEN** every call returns 404 and user A's chat is unchanged

#### Scenario: List isolation
- **WHEN** user B lists chats
- **THEN** user A's chats are not returned
