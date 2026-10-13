# SSE contract (messages:stream, retry, edit)

Headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`. Each event:
`event: <name>\ndata: <json>\n\n`. Ordering: `stream_started ping* (delta|tool)* citations? (done|error)`.

| Event | Data |
|---|---|
| stream_started | `{request_id, message_id, is_new_turn, thread_summary_applied?: {token_estimate}}` |
| ping | `{}` — every `streaming.sse_ping_interval_seconds` of idle time, only before first delta/tool |
| delta | `{type: "text"|"reasoning", content}` |
| tool | `{phase: "start"|"done", name, details}` — file_search done `{files_searched}`, code_interpreter done `{output}` |
| citations | `{items: [{source: file|web, title, url?, attachment_id?, snippet, span?}]}` once, before terminal, only on normal completion with ≥1 mapped citation |
| done | `{usage: {input_tokens, output_tokens}, effective_model, selected_model, quota_decision: allow|downgrade, downgrade_from?, downgrade_reason?, quota_warnings?}` |
| error | `{code, message}` — codes: provider_error, provider_timeout, rate_limited, web_search_calls_exceeded, code_interpreter_calls_exceeded, agentic_iterations_exceeded, unexpected_tool_use, message_persistence_failed, finalization_failed, stream_interrupted |

Replay: `stream_started(is_new_turn=false, message_id=persisted)` → one `delta` with stored text →
`done` (usage from stored message, quota_decision/downgrade_from rebuilt from models, no
downgrade_reason, no quota_warnings). Terminal events are sent only after the finalization
transaction commits; after a client disconnect nothing is sent. Keep-alive comment every 30 s after
content starts.
