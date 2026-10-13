# mini-chat-chats Specification

## Purpose

Chat lifecycle of the mini-chat gear: create, get, list, rename and delete chats, model selection and locking, activity ordering.

## Requirements

### Requirement: Create chat
`POST /v1/chats` SHALL create a chat owned by the caller (tenant + user from the security context) and return 201 with the `ChatDetail` (`id`, `model`, `title` omitted when absent, `is_temporary=false`, `message_count`, `created_at`, `updated_at`) and a `Location` header `/mini-chat/v1/chats/{id}`. `title` is optional; when present it is trimmed and MUST be 1–255 characters, otherwise 400 `invalid_argument` with `field_violations[title].reason = INVALID_TITLE` (checked before authorization and model lookup). `model` is optional; when present it MUST be an enabled catalog entry, otherwise 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`; when absent the first enabled entry with `preference.is_default`, else the first enabled entry, is used (no enabled entry: 400 `INVALID_MODEL`).

#### Scenario: Default model
- **WHEN** a chat is created without `model`
- **THEN** the chat's `model` is the first enabled `is_default` catalog entry

#### Scenario: Disabled model rejected
- **WHEN** a chat is created with a disabled or unknown model
- **THEN** the response is 400 with `context.field_violations[0].reason = INVALID_MODEL`

#### Scenario: Whitespace title rejected
- **WHEN** a chat is created with title `"   "`
- **THEN** the response is 400 `INVALID_TITLE`

### Requirement: Get, update and delete chat
`GET /v1/chats/{id}` SHALL return the `ChatDetail` with `message_count` (non-deleted messages) and no embedded messages. `PATCH /v1/chats/{id}` SHALL update only `title` (trimmed, 1–255; invalid → 400 `INVALID_TITLE`; missing/null/wrong type → 422), ignore other fields, set `updated_at` and return 200 with the `ChatDetail`. `DELETE /v1/chats/{id}` SHALL soft-delete the chat, mark its attachments `cleanup_status = pending`, enqueue a chat-cleanup outbox message in the same transaction and return 204. Missing, foreign or deleted chats SHALL return 404 `not_found` with `context.resource_type = gts.cf.core.mini_chat.chat.v1~`; a non-UUID id SHALL return 400.

#### Scenario: Model immutable via PATCH
- **WHEN** `PATCH` is sent with `{"title": "Renamed", "model": "other"}`
- **THEN** the response is 200, the title is renamed and the model is unchanged

#### Scenario: Double delete
- **WHEN** a chat is deleted twice
- **THEN** the first returns 204 and the second returns 404

### Requirement: List chats
`GET /v1/chats` SHALL return the caller's non-deleted chats as `{items, page_info}` ordered by `updated_at desc, id desc` by default, with `limit` (default 20, clamped to 100, `0` → 400 `INVALID_LIMIT`), opaque `cursor` pagination, OData `$filter`/`$orderby` over `updated_at`, `id`, `title`, and `$select` accepted and ignored. Unknown fields, malformed filters and malformed cursors SHALL return 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~`.

#### Scenario: Pagination
- **WHEN** a user with 3 chats lists with `limit=2`
- **THEN** the first page has 2 items and a `next_cursor`, and following it returns the remaining chat

#### Scenario: Unknown filter field
- **WHEN** the list is requested with `$filter=foo eq 'x'`
- **THEN** the response is 400 `invalid_argument`

### Requirement: Activity ordering
`chats.updated_at` SHALL be bumped on create, rename and delete and in the same transaction as every sent message, retry and edit, so that list order reflects the most recent activity.

#### Scenario: Message bumps chat
- **WHEN** a message is sent in the older of two chats
- **THEN** that chat is listed first afterwards

### Requirement: Model locked per chat
The chat's selected model SHALL never change. If the model is later disabled, sends SHALL be downgraded (`model_disabled`); if it is removed from the catalog, `messages:stream`, retry, edit and upload SHALL return 400 `INVALID_MODEL`.

#### Scenario: Removed model
- **WHEN** a message is sent to a chat whose model is no longer in the catalog
- **THEN** the response is 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`
