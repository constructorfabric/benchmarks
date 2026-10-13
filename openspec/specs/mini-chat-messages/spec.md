# mini-chat-messages Specification

## Purpose
Message listing contract, message counting and ordering, and message reactions.

## Requirements

### Requirement: List messages
`GET /v1/chats/{id}/messages` SHALL return `{items: [Message], page_info}` of non-deleted messages of the chat with cursor pagination (`limit` default 20, max 100, `0` → 400), default order `created_at asc, id asc`, `$filter` and `$orderby` over `created_at`, `id`, `role`; invalid OData input SHALL return 400 with `context.resource_type = gts.cf.core.odata.query.v1~`.

#### Scenario: Chronological order
- **WHEN** two turns were sent
- **THEN** messages are listed user, assistant, user, assistant

#### Scenario: Filter by id
- **WHEN** `$filter=id eq '{assistant_message_id}'` is used
- **THEN** exactly that message is returned

### Requirement: Message response contract
Each `Message` SHALL include `id`, `request_id` (always non-null; user and assistant messages of one turn share it), `role`, `content`, `attachments` (always present array of `AttachmentSummary {attachment_id, kind, filename, status, img_thumbnail?}` of non-deleted linked attachments, `img_thumbnail` only for ready images), `my_reaction` (always present: `like`, `dislike` or `null`; always `null` for non-assistant messages), `created_at`; optional `model` (assistant), `input_tokens` and `output_tokens` (omitted when 0). A stored message with a null `request_id` SHALL fail the request with 500.

#### Scenario: Consistent fields
- **WHEN** a completed turn's messages are listed
- **THEN** both messages have the same `request_id`, `attachments: []`, `my_reaction: null`, and the assistant message has `model`, `input_tokens`, `output_tokens`

### Requirement: Message counting
`message_count` on the chat SHALL equal the number of non-deleted messages: each completed turn adds a user and an assistant message; a failed turn keeps only the user message; a cancelled turn keeps the user message and a partial assistant message when text was received; turn mutations soft-delete the replaced turn's messages.

#### Scenario: Two turns
- **WHEN** two turns complete
- **THEN** `message_count` is 4

### Requirement: Reactions
`PUT /v1/chats/{id}/messages/{msg_id}/reaction` with `{reaction: like|dislike}` SHALL upsert the caller's reaction on an assistant message and return 200 `{message_id, reaction, created_at}`; an invalid value SHALL return 400 `INVALID_REACTION` (before authorization), a missing field 422. `DELETE` SHALL return 204 whether or not a reaction existed. Both SHALL return 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]` for user/system messages, 404 (message type) for a message not in the chat, and 404 (chat type) for an inaccessible chat. The reaction SHALL be reflected in `my_reaction`.

#### Scenario: Idempotent upsert
- **WHEN** `like` then `dislike` is PUT on the same assistant message
- **THEN** one `message_reactions` row exists with `dislike` and `my_reaction` is `dislike`

#### Scenario: User message rejected
- **WHEN** a reaction is PUT on a user message
- **THEN** the response is 400 with subject `reaction_target`
