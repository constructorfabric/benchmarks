# mini-chat-chats Specification

## Purpose
Chat lifecycle (create, read, update, list, soft delete) of the mini-chat gear.

## Requirements

### Requirement: Create chat
`POST /mini-chat/v1/chats` SHALL create a chat owned by the caller's tenant and user. The optional `title` is trimmed and MUST be 1–255 characters; it is validated before authorization and the model lookup. The optional `model` MUST name an enabled catalog entry; when omitted the default model is the first enabled entry with `preference.is_default = true`, else the first enabled entry (tier not considered). The response is HTTP 201 with a `ChatDetail` body (`id`, `model`, `title` omitted when absent, `is_temporary=false`, `message_count=0`, `created_at`, `updated_at`) and a `Location: /mini-chat/v1/chats/{id}` header. `user_id`/`tenant_id` are never returned.

#### Scenario: Create with default model
- **WHEN** a user posts `{}` to `/v1/chats`
- **THEN** the response is 201, `model` is the default catalog model, `title` is absent and `Location` ends with the new chat id

#### Scenario: Invalid title
- **WHEN** the title is empty, whitespace-only or longer than 255 characters after trim
- **THEN** the response is 400 `invalid_argument` with `context.field_violations[0].field = title` and `reason = INVALID_TITLE`

#### Scenario: Unknown or disabled model
- **WHEN** `model` is not in the catalog or is disabled
- **THEN** the response is 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`

### Requirement: Get, update and delete chat
`GET /v1/chats/{id}` SHALL return the `ChatDetail` with `message_count` (non-deleted messages) and no embedded messages. `PATCH /v1/chats/{id}` SHALL update only `title` (trim, 1–255 chars, `INVALID_TITLE`), ignore unknown body fields, set `updated_at = now` and return 200 with the `ChatDetail`; a body without a string `title` is 422 from the JSON extractor. `DELETE /v1/chats/{id}` SHALL soft-delete the chat (204), mark its attachments `cleanup_status = pending`, enqueue a chat-cleanup outbox message in the same transaction, and a second delete SHALL return 404. Missing, foreign and soft-deleted chats SHALL return 404 `not_found` with `resource_type = gts.cf.core.mini_chat.chat.v1~`.

#### Scenario: Rename keeps model
- **WHEN** `PATCH` is sent with `{"title": "Renamed", "model": "other"}`
- **THEN** the response is 200, the title is `Renamed` and `model` is unchanged

#### Scenario: Delete twice
- **WHEN** a chat is deleted and deleted again
- **THEN** the first response is 204 and the second is 404; `GET` of the chat returns 404

### Requirement: Model is immutable once set
The chat's `model` (selected model) SHALL never change after creation; no endpoint modifies it.

#### Scenario: Model unchanged after turns
- **WHEN** messages are sent, retried and the chat renamed
- **THEN** `GET /v1/chats/{id}` still returns the model chosen at creation

### Requirement: List chats with OData and pagination
`GET /v1/chats` SHALL return `{items, page_info:{limit,next_cursor,prev_cursor}}` of the caller's non-deleted chats, default order `updated_at desc, id desc`, `limit` default 20, values above 100 clamped to 100, `limit=0` rejected with `INVALID_LIMIT`. `$filter` and `$orderby` SHALL support `updated_at`, `id`, `title`; unknown fields, malformed filters and malformed cursors SHALL return 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~`. `$select` is validated and ignored.

#### Scenario: Pagination with cursor
- **WHEN** a user with 3 chats lists with `limit=2` and follows `next_cursor`
- **THEN** the pages contain 2 and 1 chats without duplicates

#### Scenario: Unknown filter field
- **WHEN** `$filter=model eq 'x'` is sent
- **THEN** the response is 400 `invalid_argument` with the OData resource type

### Requirement: Chat ordering reflects most recent activity
`chats.updated_at` SHALL be bumped on create, rename, delete and in the same transaction as every sent message, retry and edit.

#### Scenario: Sending a message moves chat to the top
- **WHEN** chat A was created before chat B and a message is sent in chat A
- **THEN** `GET /v1/chats` lists chat A first
