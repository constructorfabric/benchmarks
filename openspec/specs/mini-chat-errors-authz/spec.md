# mini-chat-errors-authz Specification

## Purpose
Defines the `mini-chat` gear's canonical error contract, sanitization, PEP authorization and tenant/owner isolation (source: `gears/mini-chat/docs/`). Introduced by change `implement-mini-chat-gear`.

## Requirements

### Requirement: Canonical REST error contract
Every REST error SHALL be an RFC 9457 `Problem` (`type`, `title`, `status`, `detail`, `instance`, `trace_id`, `context`) from `toolkit-canonical-errors` with the HTTP status fixed by the category and the machine-readable reason in `context.reason` (aborted, permission_denied), `context.field_violations[].reason` (invalid_argument, out_of_range), `context.violations[]` (failed_precondition, resource_exhausted), `context.resource_type` (not_found) or `context.resource_name` (already_exists). Non-UUID path parameters SHALL return 400 `invalid_path_params`; malformed JSON 400; schema mismatch 422.

#### Scenario: Non-UUID chat id
- **WHEN** `GET /mini-chat/v1/chats/not-a-uuid` is requested
- **THEN** the response is 400 `invalid_argument`

#### Scenario: Running-turn conflict shape
- **WHEN** a send hits a running turn
- **THEN** the response is 409 with `context.reason = "turn_already_running"` and no top-level `code`

### Requirement: Streaming error codes and sanitization
After the stream opens, errors SHALL be `event: error` with `{code, message}` where `code` ∈ {`provider_error`, `provider_timeout`, `rate_limited`, `web_search_calls_exceeded`, `code_interpreter_calls_exceeded`, `agentic_iterations_exceeded`, `unexpected_tool_use`, `message_persistence_failed`, `finalization_failed`, `stream_interrupted`}. Messages SHALL replace provider ids (`resp_`, `chatcmpl-`, `cmpl-`, `msg_` + alphanumerics; `file-`, `file_`, `assistant-`, `vs_` + ≥12 alphanumerics) with `[provider_id]`, URLs with `[url]`, and `sk-` keys (≥10 chars) / `Bearer` tokens with `[credential]`.

#### Scenario: Ordinary words untouched
- **WHEN** a provider message contains `file_search` and `file-based`
- **THEN** they are left unchanged

### Requirement: Authorization and isolation
Every operation SHALL evaluate the AuthZ resolver with the chat (or model / user-quota) resource type and the action from DESIGN §3.8, compile the constraints into an `AccessScope` with tenant and owner predicates, and apply it to every query. PDP denial → 403 `permission_denied`; PDP failure → 503 with `Retry-After: 5`; a chat of another user or tenant SHALL be invisible (404) for every chat-scoped endpoint, including messages, attachments, turns and reactions.

#### Scenario: Foreign user access
- **WHEN** user B requests user A's chat, messages, attachment, turn or reaction endpoints
- **THEN** each returns 404 and nothing is modified
