# mini-chat-chats Specification

## Purpose
Chat CRUD for the mini-chat gear: create, get, list (OData filter/order/cursor pagination), title update and soft delete of per-user chats with an immutable model.

## Requirements

### Requirement: Chat create, get, update, delete
The gear SHALL serve `POST/GET /mini-chat/v1/chats`, `GET/PATCH/DELETE /mini-chat/v1/chats/{id}` returning `ChatDetail` (`id`, `model`, `title` omitted when absent, `is_temporary=false`, `message_count`, `created_at`, `updated_at`). Create returns 201 with `Location: /mini-chat/v1/chats/{id}`; delete soft-deletes the chat, returns 204, enqueues chat cleanup, and a second delete returns 404. Titles are trimmed and MUST be 1–255 characters (else 400 `invalid_argument`, `field_violations[title].reason = INVALID_TITLE`), checked before authorization and model lookup. PATCH changes only `title` (unknown fields ignored), sets `updated_at`, and a missing/null/non-string title is rejected by the JSON extractor with 422.

#### Scenario: Create with default model
- **WHEN** a user creates a chat without `model`
- **THEN** the chat model is the first enabled catalog entry with `preference.is_default = true`, else the first enabled entry, and the response is 201 with a `Location` header

#### Scenario: Invalid or disabled model
- **WHEN** a user creates a chat with an unknown or disabled model
- **THEN** the response is 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`

#### Scenario: Whitespace title
- **WHEN** a user creates or renames a chat with title `"   "` or a 256-character title
- **THEN** the response is 400 with `INVALID_TITLE`

#### Scenario: Delete twice
- **WHEN** a chat is deleted and then deleted again
- **THEN** the first call returns 204 and the second returns 404 `not_found` with `resource_type = gts.cf.core.mini_chat.chat.v1~`

### Requirement: Model immutability
A chat's model SHALL never change after creation; PATCH with `model` leaves it unchanged, and the effective model of a turn is recorded on the assistant message only.

#### Scenario: Rename with model field
- **WHEN** a user sends `PATCH {"title":"Renamed","model":"other"}`
- **THEN** the response is 200, the title changes and `model` is unchanged

### Requirement: Chat listing with OData and pagination
`GET /v1/chats` SHALL return non-deleted chats of the caller ordered by `updated_at desc, id desc` by default, with cursor pagination (`limit` default 20, values above 100 clamped to 100, `0` → 400 `INVALID_LIMIT`), `$filter`/`$orderby` over `updated_at`, `id`, `title`; malformed filters, unknown fields and malformed cursors return 400 `invalid_argument` with `context.resource_type = gts.cf.core.odata.query.v1~`. `$select` is validated and ignored.

#### Scenario: Pagination walk
- **WHEN** a user with 3 chats lists with `limit=2` and follows `next_cursor`
- **THEN** the first page has 2 items and a `next_cursor`, the second page has the remaining item and no `next_cursor`

#### Scenario: Malformed filter
- **WHEN** the user lists with `$filter=bogus eq 1` or `cursor=garbage`
- **THEN** the response is 400 with `resource_type = gts.cf.core.odata.query.v1~`

### Requirement: Ordering reflects activity
`chats.updated_at` SHALL be bumped on create, rename, delete and in the transaction of every sent message, retry and edit, so listing order reflects the latest activity.

#### Scenario: Message bumps chat
- **WHEN** a user sends a message to the older of two chats
- **THEN** that chat is listed first afterwards

### Requirement: Tenant and owner isolation
Every chat-scoped operation SHALL evaluate the AuthZ PDP for the chat resource type `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, compile constraints to an access scope including tenant and owner predicates, and return 404 for chats of other users or tenants; PDP denial returns 403 and PDP evaluation failure returns 503 with `Retry-After: 5`.

#### Scenario: Foreign chat
- **WHEN** user B reads, renames, deletes, lists messages of, streams into or uploads to user A's chat
- **THEN** every call returns 404 and user B's chat list does not contain A's chat
