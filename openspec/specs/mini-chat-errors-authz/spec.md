# mini-chat-errors-authz Specification

## Purpose
Canonical error contract, error sanitization and authorization/tenant scoping of the mini-chat gear.

## Requirements

### Requirement: Canonical error contract
Every REST error SHALL be an RFC 9457 `Problem` (`type, title, status, detail, instance, trace_id, context`) produced from `CanonicalError`, with the category→status mapping and reasons of DESIGN §3.3 / ADR-0004 (`not_found` 404 with `context.resource_type = gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`; `invalid_argument` 400 with `field_violations[].reason`; `out_of_range` 400; `failed_precondition` 400 with `violations[{subject,description,type}]`; `aborted` 409 with `context.reason`; `already_exists` 409 with `resource_name`; `resource_exhausted` 429 with `violations[{subject,description:"quota_exceeded"}]`; `permission_denied` 403; `service_unavailable` 503 with `Retry-After`; `internal` 500). Pre-stream failures on streaming endpoints SHALL be JSON errors with no SSE stream; after the stream opens they SHALL be SSE `error` events.

#### Scenario: Non-UUID path parameter
- **WHEN** `GET /v1/chats/not-a-uuid` is sent
- **THEN** the response is 400 `invalid_argument` with reason `invalid_path_params`

#### Scenario: Pre-stream error is JSON
- **WHEN** a message with empty content is sent
- **THEN** the response is 400 `application/problem+json` and not `text/event-stream`

### Requirement: Tenant and owner isolation
Every operation SHALL be authorized through the PDP (PolicyEnforcer) for the chat/model/user-quota resource type with the documented action, and every chat-scoped query SHALL be constrained by tenant and owner. Another user's or tenant's chat, messages, turns, attachments and reactions SHALL be invisible (404); a PDP denial is 403 and a PDP failure 503 with `Retry-After: 5`.

#### Scenario: Foreign chat
- **WHEN** user B (other tenant or same tenant) reads, lists messages of, sends to, uploads to or deletes user A's chat
- **THEN** every request returns 404 and user A's data and quota are unchanged
