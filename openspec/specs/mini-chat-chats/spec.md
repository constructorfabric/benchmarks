# mini-chat-chats Specification

## Purpose
Defines the `mini-chat` gear's chat CRUD, model selection/locking, list filtering/ordering/pagination (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: Create chat
`POST /mini-chat/v1/chats` SHALL create a chat owned by the caller's tenant and user and return 201 with a `ChatDetail` (`id`, `model`, `title` omitted when absent, `is_temporary=false`, `message_count=0`, `created_at`, `updated_at`) and a `Location: /mini-chat/v1/chats/{id}` header. `title` is optional; when present it is trimmed and MUST be 1–255 characters, else 400 `invalid_argument` with `field_violations[title].reason = INVALID_TITLE` (checked before authorization and model lookup). `model` is optional; when absent the first enabled catalog entry with `preference.is_default` is used, else the first enabled entry; an unknown or disabled model returns 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`.

#### Scenario: Default model
- **WHEN** a chat is created without `model`
- **THEN** the response `model` is the first enabled `is_default` catalog entry

#### Scenario: Disabled model rejected
- **WHEN** a chat is created with a disabled or unknown model id
- **THEN** the response is 400 with `INVALID_MODEL`

#### Scenario: Blank title rejected
- **WHEN** a chat is created with title `"   "` or a 256-character title
- **THEN** the response is 400 with `INVALID_TITLE`

### Requirement: Get, update and delete chat
`GET /chats/{id}` SHALL return the `ChatDetail` (no embedded messages). `PATCH /chats/{id}` SHALL update only `title` (trimmed, 1–255, `INVALID_TITLE` otherwise), ignore unknown fields, never change `model`, bump `updated_at`, and return 200 with the `ChatDetail`; a body without a string `title` is 422. `DELETE /chats/{id}` SHALL soft-delete the chat, mark its attachments `cleanup_status=pending`, enqueue a chat-cleanup outbox message in the same transaction and return 204; a second delete returns 404. A missing, deleted or foreign chat SHALL return 404 `not_found` with `context.resource_type = gts.cf.core.mini_chat.chat.v1~`.

#### Scenario: Model is immutable
- **WHEN** `PATCH /chats/{id}` is sent with `{"title":"x","model":"other"}`
- **THEN** the response is 200, the title changes and `model` is unchanged

#### Scenario: Deleted chat is hidden
- **WHEN** a chat is deleted and then fetched
- **THEN** the fetch returns 404 and a repeated delete returns 404

### Requirement: List chats
`GET /chats` SHALL return `{items, page_info:{limit,next_cursor,prev_cursor}}` of the caller's non-deleted chats, default order `updated_at desc` with `id` tiebreak, default `limit` 20, values above 100 clamped to 100, `limit=0` → 400 `INVALID_LIMIT`. It SHALL support OData `$filter` over `updated_at`, `id`, `title` and `$orderby` over the same fields, opaque cursors, and return 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~` for malformed filters, unknown fields, malformed cursors and unsupported `$` options.

#### Scenario: Pagination
- **WHEN** 3 chats exist and `limit=2` is requested
- **THEN** two items are returned with a `next_cursor`, and following it returns the third item

#### Scenario: Malformed filter
- **WHEN** `$filter=nonexistent eq 'x'` is requested
- **THEN** the response is 400 with resource type `gts.cf.core.odata.query.v1~`

### Requirement: Chat ordering reflects activity
`chats.updated_at` SHALL be bumped on create, rename, delete and in the transaction of every sent message, retry and edit, so the default list order reflects the latest activity.

#### Scenario: Sending a message moves a chat to the top
- **WHEN** two chats exist and a message is sent to the older one
- **THEN** that chat is listed first

### Requirement: Message count
`message_count` SHALL equal the number of non-deleted messages of the chat.

#### Scenario: Count after one turn
- **WHEN** one turn completes
- **THEN** `message_count` is 2
