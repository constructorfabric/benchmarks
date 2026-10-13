# SSE contract (PRD §7.2, DESIGN §3.3)

Ordering: `stream_started ping* (delta | tool)* citations? (done | error)`; close after terminal.
Headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`.

| event | data |
|---|---|
| stream_started | `{"request_id","message_id","is_new_turn", "thread_summary_applied"?: {"token_estimate"}}` |
| ping | `{}` — only before first delta/tool, every `streaming.sse_ping_interval_seconds` idle |
| delta | `{"type":"text"|"reasoning","content"}` |
| tool | `{"phase":"start"|"done","name","details":{}}` |
| citations | `{"items":[{"source":"file"|"web","title","url"?,"attachment_id"?,"span"?:{"start","end"},"snippet"}]}` |
| done | `{"usage":{"input_tokens","output_tokens"},"effective_model","selected_model","quota_decision":"allow"|"downgrade","downgrade_from"?,"downgrade_reason"?,"quota_warnings"?:[{"tier","period","remaining_percentage","warning","exhausted","next_reset"?}]}` |
| error | `{"code","message"}` |

Replay: `stream_started(is_new_turn=false)`, one `delta` with full stored text, `done` (no warnings, no reason).
Error codes: provider_error, provider_timeout, rate_limited, web_search_calls_exceeded,
code_interpreter_calls_exceeded, agentic_iterations_exceeded, unexpected_tool_use, message_persistence_failed,
finalization_failed, stream_interrupted.
Provider (OpenAI Responses) event mapping: see research-design-notes.md "SSE events (exact)".
