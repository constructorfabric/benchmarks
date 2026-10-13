# REST + SSE contract

Schema source of truth: `docs/api/api.json` (paths `/mini-chat/v1/...`, component schemas
`ChatDetailDto`, `Page_ChatDetailDto`, `MiniChatMessageDto`, `AttachmentDetailDto`, `MiniChatSseEvent`,
`TurnStatusResponse`, `ModelDto`, `QuotaStatusResponse`, ...). Error table: ADR-0004.

| Op id | Method + path (under `/mini-chat`) | Success | Notes |
|---|---|---|---|
| mini_chat.create_chat | POST /v1/chats | 201 ChatDetail + Location | throttling zones rl/ifl_mini_chat_chat |
| mini_chat.list_chats | GET /v1/chats | 200 Page<ChatDetail> | OData updated_at/id/title |
| mini_chat.get_chat | GET /v1/chats/{id} | 200 | `title` omitted when null |
| mini_chat.update_chat | PATCH /v1/chats/{id} | 200 | title only |
| mini_chat.delete_chat | DELETE /v1/chats/{id} | 204 | second delete 404 |
| mini_chat.list_messages | GET /v1/chats/{id}/messages | 200 Page<Message> | OData created_at/id/role, default asc |
| mini_chat.stream_message | POST /v1/chats/{id}/messages:stream | 200 SSE | replay on completed request_id |
| mini_chat.upload_attachment | POST /v1/chats/{id}/attachments | 201 AttachmentDetail | multipart `file` |
| mini_chat.get_attachment | GET /v1/chats/{id}/attachments/{attachment_id} | 200 | |
| mini_chat.delete_attachment | DELETE /v1/chats/{id}/attachments/{attachment_id} | 204 | idempotent, 409 attachment_locked |
| mini_chat.get_turn | GET /v1/chats/{id}/turns/{request_id} | 200 TurnStatus | 404 deleted |
| mini_chat.retry_turn | POST /v1/chats/{id}/turns/{request_id}/retry | 200 SSE | |
| mini_chat.edit_turn | PATCH /v1/chats/{id}/turns/{request_id} | 200 SSE | body {content} |
| mini_chat.delete_turn | DELETE /v1/chats/{id}/turns/{request_id} | 204 | |
| mini_chat.put_reaction | PUT /v1/chats/{id}/messages/{msg_id}/reaction | 200 Reaction | |
| mini_chat.delete_reaction | DELETE same | 204 | |
| mini_chat.list_models | GET /v1/models | 200 {items} | enabled only |
| mini_chat.get_model | GET /v1/models/{id} | 200 / 404 | |
| mini_chat.get_quota_status | GET /v1/quota/status | 200 | tiers premium,total |

## SSE

`event: <name>\ndata: <json>\n\n`; headers `Content-Type: text/event-stream`, `Cache-Control: no-cache`.
Grammar `stream_started ping* (delta|tool)* citations? (done|error)`; comment keep-alive every 30 s.

- `stream_started {request_id, message_id, is_new_turn, thread_summary_applied?{token_estimate}}`
- `ping {}` (only before first delta/tool, every `streaming.sse_ping_interval_seconds`)
- `delta {type: "text"|"reasoning", content}`
- `tool {phase: "start"|"done", name, details}`
- `citations {items: [{source, title, url?, attachment_id?, snippet, span?}]}`
- `done {usage{input_tokens,output_tokens}, effective_model, selected_model, quota_decision,
  downgrade_from?, downgrade_reason?, quota_warnings?[{tier,period,remaining_percentage,warning,exhausted,next_reset?}]}`
- `error {code, message}` codes: provider_error, provider_timeout, rate_limited,
  web_search_calls_exceeded, code_interpreter_calls_exceeded, agentic_iterations_exceeded,
  unexpected_tool_use, message_persistence_failed, finalization_failed, stream_interrupted.
