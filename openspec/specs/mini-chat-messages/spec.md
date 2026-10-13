# mini-chat-messages Specification

## Purpose
Message listing, message representation and reactions of the mini-chat gear.

## Requirements

### Requirement: List messages
`GET /v1/chats/{id}/messages` SHALL return `{items, page_info}` of non-deleted messages of the chat, default order `created_at asc, id asc`, `limit` default 20 (clamped to 100, 0 rejected), OData `$filter`/`$orderby` on `created_at`, `id`, `role`, errors as for the chat list.

#### Scenario: Chronological order
- **WHEN** two turns were completed
- **THEN** the list returns user, assistant, user, assistant in that order

#### Scenario: Filter by id
- **WHEN** `$filter=id eq '<assistant_message_id>'` is sent
- **THEN** exactly that message is returned

### Requirement: Message response contract
Each message SHALL contain `id`, `request_id` (non-null; user and assistant of one turn share it), `role`, `content`, `attachments` (always an array of `{attachment_id, kind, filename, status, img_thumbnail?}` of non-deleted attachments from `message_attachments`), `my_reaction` (always present: `like`, `dislike` or `null`), `created_at`; optional `model` (assistant messages), `input_tokens`/`output_tokens` (omitted when 0). A stored message with a null `request_id` SHALL fail the request with an internal error.

#### Scenario: Fields always present
- **WHEN** messages are listed for a turn without attachments or reactions
- **THEN** every item has `attachments: []`, `my_reaction: null` and a `request_id`; the assistant item has `model`, `input_tokens` and `output_tokens`

### Requirement: Message count and ordering across turns
`message_count` SHALL equal the number of non-deleted messages; retry/edit/delete SHALL soft-delete the replaced messages so counts and ordering stay consistent.

#### Scenario: Count after two turns and a delete
- **WHEN** two turns complete and the last one is deleted
- **THEN** `message_count` is 4 then 2

### Requirement: Reactions on assistant messages
`PUT /v1/chats/{id}/messages/{msg_id}/reaction` with `{"reaction":"like"|"dislike"}` SHALL upsert one reaction per user per message and return 200 `{message_id, reaction, created_at}`; an invalid value is 400 `INVALID_REACTION` (checked before authorization); a body without `reaction` is 422. `DELETE` SHALL return 204 whether or not a reaction existed. Both on a user/system message SHALL return 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]`. An unknown message SHALL return 404 with the message resource type; an unknown chat 404 with the chat resource type.

#### Scenario: Change reaction
- **WHEN** a user puts `like` then `dislike` on an assistant message
- **THEN** the message lists `my_reaction = dislike` and exactly one row exists in `message_reactions`

#### Scenario: Reaction on user message
- **WHEN** a reaction is put on a user message
- **THEN** the response is 400 `failed_precondition` with subject `reaction_target`
