# Streaming, Outbox and Provider Wire Contracts

REST schemas: `contracts/openapi.json` (extracted from `docs/api/api.json`). This file captures the non-REST contracts; DESIGN.md is normative.

## SSE (`messages:stream`, retry, edit)
Headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`.
Order: `stream_started ping* (delta | tool)* citations? (done | error)`; connection closed right after the terminal event.

| event | data |
|---|---|
| stream_started | `{request_id, message_id, is_new_turn, thread_summary_applied?: {token_estimate}}` |
| ping | `{}` — only before first delta/tool, every `streaming.sse_ping_interval_seconds` |
| delta | `{type: "text"|"reasoning", content}` |
| tool | `{phase: "start"|"done", name, details}` |
| citations | `{items: [{source: "file"|"web", title, url?, attachment_id?, snippet, span?}]}` |
| done | `{usage:{input_tokens,output_tokens}, effective_model, selected_model, quota_decision, downgrade_from?, downgrade_reason?, quota_warnings?}` |
| error | `{code, message}` codes: provider_error, provider_timeout, rate_limited, web_search_calls_exceeded, code_interpreter_calls_exceeded, agentic_iterations_exceeded, unexpected_tool_use, message_persistence_failed, finalization_failed, stream_interrupted |

Replay: `stream_started(is_new_turn=false, message_id=persisted)`, one `delta` with full text, `done` with stored usage, `quota_decision` rebuilt from stored models, no `downgrade_reason`, no `quota_warnings`.

## Outbox payloads
Usage (`mini-chat.usage_snapshot`, partition by tenant): `UsageEvent` JSON — `tenant_id, user_id, chat_id, turn_id, request_id, effective_model, selected_model, terminal_state, billing_outcome, usage|null, actual_credits_micro, settlement_method, policy_version_applied, web_search_calls, code_interpreter_calls, file_search_calls, timestamp, requester_type, dedupe_key, system_task_type?`. `dedupe_key = {tenant.simple}/{turn.simple}/{request.simple}`.

Audit (`mini-chat.audit`): turn audit (`turn_completed|turn_failed`) and mutation audit (`turn_retry|turn_edit|turn_delete` with `original_request_id`/`new_request_id`).

Attachment cleanup (`mini-chat.attachment_cleanup`): `{event_type, tenant_id, chat_id, attachment_id, provider_file_id, vector_store_id: null, storage_backend, attachment_kind, deleted_at, secondary_ref}`; `event_type ∈ attachment_deleted | attachment_upload_abandoned | attachment_indexing_failed`.

Chat cleanup (`mini-chat.chat_cleanup`, partition by chat): `{tenant_id, chat_id, system_request_id, reason: "chat_soft_delete", chat_deleted_at}`.

Thread summary (`mini-chat.thread_summary`, partition by chat): `{tenant_id, chat_id, system_request_id, base_frontier_created_at?, base_frontier_message_id?, frozen_target_created_at, frozen_target_message_id, system_task_type: "thread_summary_update"}`.

## Provider (OpenAI Responses via OAGW, `/{alias}{api_path}`)
`POST {api_path}` JSON: `model` (provider_model_id), `instructions` (system prompt + guards), `input` (messages; user content array with `input_text`/`input_image{file_id}`), `stream: true`, `max_output_tokens`, `max_tool_calls`, `tools` (`file_search{vector_store_ids,max_num_results}`, `web_search{search_context_size}`, `code_interpreter{container:{type:auto,file_ids}}`), `include: ["code_interpreter_call.outputs"]` when code interpreter, `user` (64 hex chars), `metadata{tenant_id,user_id,chat_id,request_type,feature}`, `store: false`, api_params (temperature etc.) when set.
Stream events consumed: `response.output_text.delta`, `response.output_text.annotation.added`, `response.file_search_call.searching|completed`, `response.web_search_call.searching|completed`, `response.code_interpreter_call.in_progress`, `response.output_item.done`, `response.completed`, `response.incomplete`, `response.failed`, `error`.

Files: `POST {prefix}/files` multipart (`purpose=assistants`, `file`), `DELETE {prefix}/files/{id}`. Vector stores: `POST {prefix}/vector_stores`, `POST {prefix}/vector_stores/{id}/files {file_id, attributes:{attachment_id}}`, `GET {prefix}/vector_stores/{id}/files/{file_id}`, `DELETE {prefix}/vector_stores/{id}`. Prefix `/v1` (openai) or `/openai` + `?api-version=` (azure).
