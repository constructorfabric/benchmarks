# DESIGN.md condensed implementation notes (authoritative facts, by section)

## §1–2 Principles/constraints
- Kill switches (policy snapshot): disable_premium_tier, force_standard_tier, disable_file_search,
  disable_web_search (400 failed_precondition violations[{subject:web_search,type:FEATURE_DISABLED}]),
  disable_code_interpreter (upload: XLSX-only rejected 400 invalid_argument CODE_INTERPRETER_UNAVAILABLE;
  stream: tool silently omitted), disable_images (upload + stream: 400 failed_precondition subject images).
- Vision: after cascade, if request has images and effective model lacks "VISION_INPUT" in
  multimodal_capabilities -> 400 invalid_argument field_violations[content_type].reason=VISION_NOT_SUPPORTED.
- token_budget = min(max_input_tokens (0 => no limit), context_window - max_output_tokens_applied)
  - surcharges (tool_surcharge? web_search_surcharge_tokens, code_interpreter_surcharge_tokens, file search
  via tool_surcharge_tokens; only when tool in request) - fixed_overhead_tokens.
  max_output_tokens_applied = min(catalog max_output_tokens, streaming.max_output_tokens), persisted on chat_turns.
- Drop oldest whole turns first; summary dropped only if it alone doesn't fit after mandatory items.
- SSE channel bounded: streaming.sse_channel_capacity default 32 (16..64).
- Default model: first enabled with preference.is_default, else first enabled (catalog order).
- effective_model recorded in messages.model, done.effective_model, audit.

## §3.2 Components
- Quota preflight estimate = message size tokens + prior_context_tokens (input+output tokens of most
  recent non-deleted assistant msg with non-zero tokens) + surcharges + max_output_tokens_applied.
- Buckets: `total` (all models; standard limits) and `tier:premium` (premium limits). Premium turn
  charged to both. Availability: spent + reserved + this_reserve <= limit for every period of every bucket.
- Periods daily (UTC midnight) / monthly (1st UTC). period_start computed at preflight and persisted;
  watchdog derives from chat_turns.started_at.
- Cancel/disconnect debit: min(reserve_tokens, estimated_input_tokens + minimal_generation_floor_applied).
- Warning: remaining_percentage <= 100 - warning_threshold_pct; exhausted: remaining_percentage==0
  (floored int). limit<=0 periods skipped. quota_warnings next_reset only when warning||exhausted.
- Quota status: used_credits_micro = spent + reserved; next_reset daily=tomorrow 00:00Z, monthly=1st next month.
- System task usage event (thread summary): billing_outcome "system_task", settlement_method "none",
  actual_credits_micro 0, requester_type "system", no user_id/turn_id keys,
  dedupe_key "{tenant_simple}/thread_summary_update/{system_request_id_simple}", system_task_type.
- Audit queue default `mini-chat.audit` (outbox.audit_queue_name); usage queue `outbox.queue_name`
  default `mini-chat.usage_snapshot`. No plugin => ack+drop. 120 attempts then dead-letter.
- Gear lifecycle: init (config deny_unknown_fields, validate), start (S2S creds -> OAGW upstream+route per
  provider + tenant override; alias defaults to host; "already exists" reused; secret not readable ->
  background reconcile 2s doubling to 60s), outbox queues: usage, attachment cleanup, chat cleanup,
  thread summary (lease claim_timeout_secs), audit (lease 60s); then orphan watchdog + upload reaper.

## §3.3 API contracts (REST)
- Prefix `url_prefix` default `/mini-chat`. Location header on create: `/mini-chat/v1/chats/{id}`.
- Create chat: title optional (null/absent = untitled); trimmed 1..255 else 400 INVALID_TITLE (field title);
  title validated BEFORE authz and model lookup. model invalid/disabled -> 400 field_violations[model] INVALID_MODEL.
- ChatDetail: `title` OMITTED when null (all ChatDetail responses). is_temporary false. message_count.
- updated_at bumped on create, rename, delete, every sent message, retry, edit (same txn).
- List chats: default order updated_at desc, id tiebreak; limit default 20, >100 clamp, 0 -> 400 INVALID_LIMIT
  (field `$top`); $filter/$orderby on updated_at,id,title; $select accepted+ignored (syntax validated).
- PATCH chat: `title` required non-null string (else 422), trimmed 1..255 else 400; unknown fields ignored;
  sets updated_at=now; returns 200 ChatDetail. 404 masking.
- DELETE chat: soft-delete row only, 204; second delete 404; running turn not cancelled; enqueue chat cleanup.
  chat-cleanup outbox payload too large -> 400 invalid_argument (detail; context.format).
- List messages: default created_at asc; $filter/$orderby on created_at,id,role; same limit rules.
  Message: request_id required (null in DB => internal error), attachments array (from message_attachments,
  non-deleted attachments only; AttachmentSummary{attachment_id,kind,filename,status,img_thumbnail (only image && ready)}),
  my_reaction always present (null for user/system), model optional (absent for user msgs),
  input_tokens/output_tokens omitted when 0.
- Get attachment: AttachmentDetail{id,filename,content_type,size_bytes,status,kind,error_code?,doc_summary?,
  img_thumbnail?,summary_updated_at?,created_at}; optional fields OMITTED when null; img_thumbnail only
  status=ready && kind=image; error_code only when failed. 404 resource_type attachment for missing/deleted/
  other chat/other uploader.
- Upload: before reading body: chat lookup (404 chat type), chat model in catalog (400 INVALID_MODEL; plugin
  failure 500). filename default "upload", truncate to 255 keeping extension; octet-stream -> infer MIME from ext.
  Errors: BOUNDARY_REQUIRED(content_type), MULTIPART_ERROR(multipart), MISSING_FILE(file), MISSING_CONTENT_TYPE(content_type);
  size > min(rag.uploaded_file_max_size_kb, model max_file_size_mb) (images: uploaded_image_max_size_kb) ->
  400 out_of_range field_violations[content_length] FILE_TOO_LARGE; unsupported MIME 400 invalid_argument
  UNSUPPORTED_CONTENT_TYPE; XLSX w/o code interpreter 400 invalid_argument field_violations[file] CODE_INTERPRETER_UNAVAILABLE
  (resource_type attachment); disable_images -> 400 failed_precondition images FEATURE_DISABLED; vector store other
  provider -> 409 already_exists provider_mismatch; doc count/storage -> 429 resource_exhausted document_limit/storage_limit;
  provider failure / indexing failed within 25s -> row failed error_code indexing_failed, 503 Retry-After 10;
  concurrency limit -> 503 Retry-After 5.
- Stream request: {content, request_id?, attachment_ids?, web_search?{enabled}}.
  Replay of completed non-deleted turn: stream_started(is_new_turn false, message_id = persisted assistant id),
  one delta with full text, done (quota_decision/downgrade_from rebuilt from stored models: selected=chat.model,
  effective=assistant msg model; downgrade_reason omitted). No citations.
  Other states or soft-deleted -> 409 aborted reason request_id_conflict (generic detail).
  Insert race on unique(chat_id,request_id) -> re-read -> request_id_conflict.
  Running turn in chat (different request_id) -> 409 turn_already_running (also on unique running-index race).
- Attachment validation (inside reserve txn, after reserve + user message insert -> rollback on failure):
  tenant, uploader == user, chat matches, status==ready, unique ids; list longer than
  max_documents_per_chat+max_images_per_message -> 400 before any query. Error: 400 invalid_argument
  field_violations[attachment].reason=invalid_attachment.
- Turn status: {request_id, state running|done|error|cancelled, error_code? (omit null), assistant_message_id?
  (done; cancelled with partial text), updated_at}. Soft-deleted -> 404 (turn resource type).
  error codes: provider_error, provider_timeout, orphan_timeout, context_length_exceeded, turn_setup_failed,
  quota_exceeded, streaming codes.

## SSE events (exact)
- stream_started {request_id, message_id (pre-allocated new; persisted on replay), is_new_turn,
  thread_summary_applied?{token_estimate}} (omit when none). For retry/edit request_id is the new turn's.
- delta {type:"text"|"reasoning", content}. tool {phase:start|done, name, details{}}.
  file_search: start on response.file_search_call.searching; done on .completed details{files_searched: len(results)}.
  web_search: response.web_search_call.searching -> start; .completed -> done details{}.
  code_interpreter: response.code_interpreter_call.in_progress -> start details{}; response.output_item.done item
  type code_interpreter_call -> done details{output: logs joined "\n" capped 8192 + "...[truncated]"};
  request sets include:["code_interpreter_call.outputs"] when code_interpreter tool present.
- citations {items:[{source:"file"|"web", title, url?, attachment_id?, span?{start,end}, snippet}]} once,
  after deltas before done, only on normally completed stream with >=1 mapped citation; not on incomplete.
  file citation: attachment_id (map provider file_id->attachment), title=filename, snippet "", no span.
  web: url, title, snippet = annotation text or answer text in [start,end) of the output_text part (char offsets),
  span when both indices present. score never serialized.
- done {usage{input_tokens,output_tokens}, effective_model, selected_model, quota_decision allow|downgrade,
  downgrade_from? (=selected), downgrade_reason? premium_quota_exhausted|force_standard_tier|disable_premium_tier|model_disabled,
  quota_warnings? (CAS-winning completed/incomplete only)}. Sent only after finalization commit.
  Persist failure -> error message_persistence_failed; finalization txn failure -> error finalization_failed.
- error {code,message}; ping {} only between stream_started and first delta/tool every sse_ping_interval_seconds
  (default 15, range 5..60, reset by every event). Axum keep-alive comment every 30s. Close after terminal.
- Client disconnect: no error event; turn -> cancelled (CAS). Setup runs in spawned task so a committed turn
  gets its provider task even if client disconnects pre-stream.
- Provider translation: response.output_text.delta -> delta text; response.completed -> done usage from
  response.usage, response.id persisted as provider_response_id; response.incomplete -> done (completed);
  response.failed -> error provider_error (msg from response.error then top-level error; usage kept);
  SSE `error` event -> provider_error (parse response.failed-like, then flat {code,message}, else raw);
  HTTP error/disconnect -> provider_error or provider_timeout (gateway timeout or gateway 504 deadline_exceeded
  Problem; provider's own 504 JSON -> provider_error); provider 429 -> rate_limited (message includes
  "retry in {N}s" if numeric Retry-After). Event name from `event:` line; if missing or "message" use data.type.
- Sanitization: provider ids -> "[provider_id]": resp_, chatcmpl-, cmpl-, msg_ + [A-Za-z0-9]+;
  file-, file_, assistant-, vs_ + >=12 alnum; URLs -> "[url]"; sk- + >=10 alnum and Bearer tokens -> "[credential]".
- Stream error codes: provider_error, provider_timeout, rate_limited, web_search_calls_exceeded,
  code_interpreter_calls_exceeded, agentic_iterations_exceeded, unexpected_tool_use (any function tool use when
  knowledge search off), message_persistence_failed, finalization_failed, stream_interrupted.

## REST error table extras
- Empty content: 400 field_violations[content] EMPTY_CONTENT. Reaction invalid: 400 field_violations[reaction]
  INVALID_REACTION checked BEFORE authz; missing reaction field -> 422.
- 422 body schema: field_violations[body] invalid_json_body; 400 json_syntax_error; 415 missing_json_content_type;
  path non-UUID 400 invalid_path_params. (platform extractors)
- TOO_MANY_IMAGES field image_count (out_of_range). INPUT_TOO_LONG, CONTEXT_BUDGET_EXCEEDED out_of_range 400.
- turn_state STATE failed_precondition; reaction_target STATE.
- Retry/edit/delete by non-requester -> 403 permission_denied AUTHZ_DENIED. PDP failure 503 Retry-After 5.
- NOT_LATEST_TURN / GENERATION_IN_PROGRESS: aborted 409 (context.reason).
- attachment_locked: already_exists 409 resource_name. unique_violation generic.
- 429 resource_exhausted violations[{subject: tokens|web_search|code_interpreter, description:"quota_exceeded"}].
- Upload storage failure 503 retry 10; provider/policy resolution failure pre-stream 500 internal.

## Models API
- GET /v1/models {items:[ModelDto{model_id, display_name, tier standard|premium, multiplier_display,
  description? (omit when absent), multimodal_capabilities, context_window}]}, enabled only, catalog order.
  GET /v1/models/{id} 404 not_found resource_type model if missing/disabled. Snapshot read per request.
## Reactions
- PUT body {reaction}: 200 {message_id, reaction, created_at}; upsert. DELETE 204 idempotent.
  Errors: chat 404 (chat type), msg not in chat 404 (message type), non-assistant 400 reaction_target STATE.

## §3.5 OAGW provisioning / provider endpoints
- Per provider entry: Upstream (host, port, use_http, auth_plugin_type, auth_config, alias=upstream_alias||host);
  Route POST prefix derived from api_path ({model} placeholder as path suffix; query allowlist from api_path, e.g.
  api-version). RAG routes for entries with storage_kind: POST {prefix}/files, DELETE {prefix}/files/*,
  POST {prefix}/vector_stores*, DELETE {prefix}/vector_stores/*, GET {prefix}/vector_stores/*; prefix `/v1` (openai)
  or `/openai` + api-version query (azure). Best-effort RAG routes.
- Chat request to `{alias}{api_path}`; RAG to `{alias}{prefix}/...`.
## Send sequence (normative order)
 authz(send_message, chat id) [403 deny / 503 PDP failure] -> load chat scoped (404) -> idempotency check
 (replay / 409 request_id_conflict) + parallel guard (409 turn_already_running) -> snapshot boundary, prior
 context tokens, attachment counts, image count limit -> preflight (policy snapshot, kill switches:
 web_search disabled checked BEFORE cascade -> 400; quota cascade; daily web_search/code_interpreter only when
 tool sent -> 429) -> input token limit (INPUT_TOO_LONG), image guards (disable_images 400, vision 400) ->
 context assembly -> resolve provider -> single reserve txn: quota reserve + limit re-check (429), user message
 insert (+touch chats.updated_at), validate attachment_ids + insert message_attachments, insert chat_turns
 running -> commit -> stream_started -> provider call -> finalization txn (assistant msg + usage, CAS turn,
 settle quota, enqueue usage + audit (+thread summary)) -> done/error.
- Retry/edit: 1 preview validate (latest, terminal, ownership) 2 resolve chat model (no enabled filter) +
  mutation preflight (quota cascade, kill switches, image guards for original images) -> JSON error, nothing
  changes 3 commit: soft-delete prev turn, insert new user msg (retry copies; edit new content), relink original
  attachments (non-deleted), insert running turn, delete thread summary if it covers mutated turn, bump
  chats.updated_at, enqueue mutation audit 4 context assembly, provider resolution, reserve + re-check; failure ->
  JSON error + new turn failed with context_length_exceeded | quota_exceeded (429) | turn_setup_failed 5 stream.
## Upload sequence
 handler: MIME from part headers; authz + chat model resolve (before body); service: disable_images; purposes;
 stream body with byte counter (FILE_TOO_LARGE); insert row pending (+purpose flags), per-chat limits (429);
 provider POST files (multipart purpose=assistants) -> status uploaded + provider_file_id;
 document+file_search: get/create chat vector store; POST vector_stores/{id}/files; poll GET
 vector_stores/{id}/files/{file_id} 250ms doubling to 2s until 25s after upload start; missing status ->
 in_progress; completed -> ready; failed/cancelled/unknown or non-transient read error -> failed indexing_failed +
 best-effort provider file delete, 503 Retry-After 10 generic detail "Service temporarily unavailable";
 transient read err (5xx/gateway) keeps polling; still in_progress at 25s -> 201 uploaded + background task
 (rounds of 20s, refresh updated_at each round, 250ms doubling to 5s, up to 10min): completed -> ready;
 failure/timeout -> one txn: failed indexing_failed, cleanup_status=pending + outbox attachment cleanup event
 (event_type attachment_indexing_failed); stop when row deleted/not uploaded/cleanup_status set/gear stop.
 code_interpreter-only (XLSX): uploaded->ready, no vector store. image: thumbnail (webp, fit WxH, <=max_bytes
 else null; skip when > max_decode_bytes, pixels>max_pixels) then ready.
 Kind: image/png|jpeg|webp|gif => image, else document. Purposes: XLSX => for_code_interpreter; other docs =>
 for_file_search; images none.
 Thumbnail config: thumbnail.width 128, height 128, max_bytes 131072, max_pixels 100000000, max_decode_bytes 33554432.
## Cancellation
 Disconnect drops SSE relay -> cancel token -> abort provider request; send failure on channel = disconnect.
 Finalize cancelled: persist partial response, estimated settlement.

## Thread summary (outbox queue `outbox.thread_summary_queue_name` default `mini-chat.thread_summary`, partition by chat_id)
- Trigger (only if thread_summary_worker.enabled, default true), evaluated at finalization of completed turns:
  proactive: no summary AND assembled_context_tokens >= effective_budget*pct/100 (effective_budget =
  min(max_input_tokens(0=none), context_window - max_output_tokens_applied), no surcharges/overhead);
  urgent: context assembly truncated messages. Summary exists and no truncation -> no trigger.
- Enqueue in finalization txn payload: tenant_id, chat_id, system_request_id (uuid v4), base_frontier_created_at,
  base_frontier_message_id, frozen_target_created_at, frozen_target_message_id, system_task_type
  "thread_summary_update". Frozen target = latest non-deleted message NOT of the causing turn; none -> no enqueue.
- Handler: lease claim_timeout_secs (default 300, 30..3600); max_attempts (default 3) -> Reject on last Retry.
  summary_model_id (empty = gpt-4.1-mini) resolved with enabled filter; missing/disabled -> Reject.
  message_content_limit chars truncation + "...". max_output_tokens = summary model catalog max_output_tokens.
  System prompt: catalog thread_summary_prompt if non-empty else summary_system_prompt else built-in (B.5.5).
  User prompt: instruction; <existing_summary> block if summary exists + merge instruction; entries
  "User: <content>"/"Assistant: <content>" separated by blank line; analysis instruction (B.5.5).
  Load messages deleted_at null, is_compressed false, (created_at,id) in (base, target] asc.
  Fit to budget (context_window - max_output_tokens, cap max_input_tokens>0): drop oldest ceil(n/5) keeping >=2.
  Parse: remove <analysis>..</analysis>, take inside <summary>..</summary>, collapse blank-line runs; no summary
  block -> whole text unless contains "<analysis"/"<summary" -> empty; empty -> Retry.
  token_estimate = output_tokens - reasoning_tokens or ceil(bytes/4) if <=0.
  Commit txn: check target frontier message not deleted (else Ok skip), CAS on base frontier, upsert summary,
  mark range is_compressed=true, enqueue system usage event. CAS lose -> finish.
- Context assembly filters is_compressed=false.
## Chat deletion cleanup
- Soft-delete txn: chats.deleted_at, mark attachments cleanup_status=pending (those with provider file?),
  enqueue chat-cleanup payload {tenant_id, chat_id, system_request_id, reason:"chat_soft_delete", chat_deleted_at}
  queue `outbox.chat_cleanup_queue_name` default `mini-chat.chat_cleanup`, partition by chat_id.
- Handler: chat must be soft-deleted else Reject. For each attachment cleanup_status=pending: DELETE
  {alias}{prefix}/files/{provider_file_id} (2xx/404 success -> done; else cleanup_attempts++, last_cleanup_error,
  cleanup_updated_at; at cleanup_worker.max_attempts (5) -> failed). When all terminal and chat_vector_stores row
  exists: DELETE {prefix}/vector_stores/{id} (2xx/404 -> delete row; else Retry until max attempts -> Reject).
  Pending remain -> Retry.
- Attachment cleanup queue `outbox.cleanup_queue_name` default `mini-chat.attachment_cleanup`; checks parent chat
  not soft-deleted then deletes provider file; same attempt accounting; max -> failed + Reject.
- outbox.num_partitions default 4 (power of 2, 1..64).
- SQLite: UUID columns TEXT but values written as 16-byte BLOBs.

## §3.8 AuthZ (PEP)
- Chat resource GTS: `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`; Model:
  `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~` (require_constraints=false); UserQuota:
  `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`.
- supported_properties owner_tenant_id(chats.tenant_id), owner_id(chats.user_id), id(chats.id).
- Actions: create(no id, props owner_tenant_id/owner_id), list, read, delete, update, list_messages, send_message,
  upload_attachment, read_attachment, delete_attachment, read_turn, retry_turn, edit_turn, delete_turn,
  set_reaction, delete_reaction (resource.id = chat_id); models list/read (no constraints); quota read.
- Defense in depth: always add owner predicate (user_id = subject) to scope.
- Fail closed: deny -> 403 AUTHZ_DENIED; compile failure -> 403; PDP failure -> 503 Retry-After 5;
  0 rows -> 404; decision true w/o constraints when required -> 403.
## §3.9 Turn mutations
- Latest = non-deleted greatest (started_at,id). Order of checks for retry: 404 chat/turn (turn missing in chat:
  404 turn? deleted turn -> 409 NOT_LATEST_TURN), running target -> 400 turn_state (checked BEFORE latest check),
  not latest -> 409 NOT_LATEST_TURN, requester != caller -> 403. Concurrent insert race -> 409 GENERATION_IN_PROGRESS.
- Retry/edit: old turn deleted_at + replaced_by_request_id = new request id; old messages soft-deleted;
  new user msg (retry copies content; edit new content), copy non-deleted attachment links; web_search_enabled reused.
- Delete: deleted_at only (+ messages soft-deleted), 204.
- Summary: in mutation txn, if summary frontier >= turn's user message (created_at,id) -> delete summary row and clear
  is_compressed on all chat messages.
- Audit: turn_completed / turn_failed (finalization; cancelled & orphan -> turn_failed); turn_retry/turn_edit
  {actor_user_id, chat_id, original_request_id, new_request_id, timestamp}; turn_delete {actor_user_id, chat_id,
  request_id, timestamp}. Setup failure after commit -> no finalization audit.

## §4 Context plan
- Order: system prompt (catalog system_prompt) + tool guards appended (context.file_search_guard,
  context.web_search_guard, knowledge_search.guard) -> thread summary (as a single USER-role message with preamble
  B.5.5) -> recent messages (context.recent_messages_limit default 10, range 0..100; query: request_id not null,
  deleted_at null, is_compressed false, <= snapshot boundary, > summary frontier if summary; desc limit K then
  reverse) -> user message + images (input_image with provider file_id).
- Estimation per item: bytes / bytes_per_token_conservative + fixed_overhead? + safety_margin_pct; image_token_budget
  per image. Budget: input_limit = min(max_input_tokens(0=none), context_window - max_output_tokens_applied);
  token_budget = input_limit - surcharges - fixed_overhead_tokens; if max_output_tokens_applied >= context_window or
  deductions reach input_limit -> CONTEXT_BUDGET_EXCEEDED.
- Algorithm: mandatory (system+guards, user msg+images) > budget -> 400 CONTEXT_BUDGET_EXCEEDED; summary kept if
  fits else dropped; recent messages newest->oldest kept while fit, first non-fit and older dropped; drop leading
  assistant messages; messages_truncated if any recent msg dropped.
- Snapshot boundary: latest message (created_at,id) at preflight.
## Tools
- file_search: {"type":"file_search","vector_store_ids":[vs],"max_num_results":N} only when chat has >=1 ready,
  non-deleted document attachment (for_file_search) and model tool_support.file_search and !disable_file_search.
  Citation map provider_file_id -> (attachment_id, filename) from ready non-deleted attachments (built only when
  file_search enabled); unknown/deleted -> omit citation.
- code_interpreter: {"type":"code_interpreter","container":{"type":"auto","file_ids":[...]}} + include
  ["code_interpreter_call.outputs"] when ready for_code_interpreter attachments, model support, kill switch off.
- web_search: tool with search_context_size = catalog web_search_context_size when request enabled and model
  tool_support.web_search. Guard text: "Use web_search only if the answer cannot be obtained from the provided
  context or your training data. Never use it for general knowledge questions. At most one web_search call per request."
  Preflight: kill switch (400) -> daily quota: daily total row web_search_calls >= quota.web_search_daily_quota ->
  429 web_search. Mid-turn: count tool start events; > web_search_max_calls_per_message -> cancel stream, turn failed
  web_search_calls_exceeded, SSE error, estimated settlement, billing_outcome failed.
  Same for code interpreter (code_interpreter_max_calls_per_message 10, daily 50).
- Knowledge search off by default; any function tool call with KS off -> unexpected_tool_use.
## Attachment deletion
- Referenced by any message_attachments row -> 409 already_exists resource_name attachment_locked.
- Other chat / other uploader -> 404 attachment (uploader check before idempotency check). Already deleted ->
  204, no duplicate outbox.
- Phase 1 txn: deleted_at=now (+ cleanup_status pending), enqueue `mini-chat.attachment_cleanup` payload
  {event_type:"attachment_deleted", tenant_id, chat_id, attachment_id, provider_file_id|null, vector_store_id:null,
  storage_backend, attachment_kind, deleted_at, secondary_ref|null}; partition by tenant.
- Handler: parent chat soft-deleted -> ack; provider_file_id null -> done; DELETE file (2xx/404 ok) -> done.
  Event types also attachment_upload_abandoned (reaper), attachment_indexing_failed.
## RAG limits: rag.max_documents_per_chat 50 (non-deleted non-failed documents) -> 429 document_limit;
  rag.max_total_upload_mb_per_chat 100 (all non-deleted non-failed incl images) -> 429 storage_limit.
## Catalog entry fields: id, provider_model_id, display_name, description, icon, multiplier_display, provider_id,
  provider_display_name, tier, enabled, multimodal_capabilities, context_window, max_output_tokens, max_input_tokens,
  input/output_tokens_credit_multiplier_micro, estimation_budgets, max_num_results, web_search_context_size,
  max_tool_calls (default 2; sent by openai_responses), general_config{type, available_from, max_file_size_mb,
  api_params{temperature?,top_p?,frequency_penalty?,presence_penalty?,stop,extra_body,reasoning_effort},
  features, tool_support{web_search,file_search,image_generation,code_interpreter,mcp}, supported_endpoints},
  preference{is_default, sort_order}, system_prompt, thread_summary_prompt.
  extra_body merged at top level except controlled keys (model,input,messages,instructions,system,stream,
  stream_options,max_output_tokens,max_completion_tokens,max_tokens,max_tool_calls,tools,tool_choice,include,store,
  previous_response_id,user,metadata).

## Downgrade cascade (normative)
1 selected model lookup: enabled -> own tier; disabled -> own tier + reason model_disabled; missing -> premium +
  model_disabled. 2 cascade [premium, standard] or [standard]. 3 per tier: premium skipped if force_standard_tier
  or disable_premium_tier (reason set if none). candidate = selected (if this tier & enabled) else enabled
  is_default of tier else first enabled of tier; none -> next tier. availability: for all required buckets
  (standard: total; premium: total + tier:premium) and all periods: spent + reserved + this_reserve <= limit;
  premium unavailable -> reason premium_quota_exhausted (if none). first pass -> effective. decision allow iff
  effective == selected && no reason. 4 none -> 429 quota_exceeded (tokens).
- Reserve txn: increment reserved for buckets/periods, re-read rows, any over limit -> rollback 429.
- Limits: bucket total -> user_limits.standard.limit_{daily,monthly}; tier:premium -> user_limits.premium.
- candidate_reserve(m): est_input = tokens(msg bytes) + prior_context_tokens + surcharges (m's estimation_budgets);
  max_out = min(m.max_output_tokens, streaming.max_output_tokens); credits_micro(est_input, max_out, in_mult, out_mult).
- Effective model persisted once; settlement uses stored effective_model + policy_version_applied snapshot.
- Provider request metadata: "user": "{tenant_simple}{user_simple}" (64 chars; fallback "{t}:{u}" if non-uuid);
  summary uses chat tenant + system subject 11111111-6a88-4768-9dfc-6bcd5187d9ed. openai_responses also sends
  metadata {tenant_id, user_id, chat_id, request_type chat|summary, feature none|file_search|web_search|
  code_interpreter|file_search+web_search|...} (combination of tools joined '+').
- Multimodal: user input content [{type:input_text,text},{type:input_image,file_id}].
## Metrics: prefix metrics.prefix default mini_chat (many counters; low priority).
## SSE headers: Content-Type text/event-stream, Cache-Control no-cache.
## Turn states: completed (response.completed or incomplete), failed, cancelled; pre-reserve failures create no row.

## Orphan watchdog
- orphan_watchdog.enabled true, scan_interval_secs 60, timeout_secs 300 (90..3600 validated). Scan up to 100
  candidates: state running, deleted_at null, COALESCE(last_progress_at, started_at) <= now - timeout.
  CAS UPDATE state failed, error_code orphan_timeout, completed_at, updated_at with same predicates;
  rows=1 -> estimated settlement (charged credits_micro(est_input, floor_applied)), usage event billing_outcome
  "aborted", settlement_method "estimated", audit turn_failed (selected_model = effective_model, quota decision "unknown").
  period_start derived from started_at. Retry/edit turn with NULL reserve -> skip settlement, still enqueue events.
- last_progress_at refreshed at most every 30s on deltas/tool events.
## Idempotency check order: (chat_id, request_id) lookup first (completed & not deleted -> replay; else 409
   request_id_conflict incl soft-deleted), then any running turn -> 409 turn_already_running.
## Cancel: partial content persisted as assistant message when non-empty (cancelled turn assistant_message_id set).
## Chat delete: only chat row soft-deleted; attachments cleanup_status=pending; outbox chat cleanup.
## Policy plugin: every preflight asks plugin current version, snapshot, user limits; settlement asks snapshot of
   policy_version_applied. Static plugin: fixed version 1, publish_usage logs.
- Snapshot: policy_version, model_catalog (ModelCatalogEntry; `enabled` defaults false when omitted),
  kill switches (disable_premium_tier, force_standard_tier, disable_web_search, disable_code_interpreter,
  disable_file_search, disable_images). UserLimits{user_id, policy_version, standard: TierLimits, premium: TierLimits}
  TierLimits{limit_daily_credits_micro, limit_monthly_credits_micro}. get_user_limits(user_id, policy_version).
- estimation_budgets per catalog entry (defaults when omitted): image_token_budget, tool_surcharge_tokens (file_search),
  web_search_surcharge_tokens, code_interpreter_surcharge_tokens, bytes_per_token_conservative, fixed_overhead_tokens,
  safety_margin_pct, minimal_generation_floor (ignored). Gear config estimation_budgets.minimal_generation_floor
  (default 50, >0, <= streaming.max_output_tokens); floor applied = min(floor, max_output_tokens_applied).

## §5.3 Credits
- credits_micro(in,out,im,om) = ceil_div(in*im,1e6) + ceil_div(out*om,1e6); checked: tokens 0..=10_000_000,
  mult 1..=10_000_000_000 else error; preflight error -> reserve i64::MAX (candidate unavailable); settlement
  error -> finalization fails (finalization_failed). Static plugin rejects zero/out-of-range multipliers and
  bytes_per_token_conservative=0 at init.
## §5.4.1 Reserve
- estimated_text_tokens = ceil((ceil(utf8_bytes / bpt) + fixed_overhead_tokens) * (100 + safety_margin_pct) / 100)
- estimated_input = text + prior_context_tokens + images*image_token_budget + tool_surcharge(file_search: chat has
  ready document && model tool_support.file_search && !disable_file_search) + web_search_surcharge (enabled && model
  supports) + code_interpreter_surcharge (ready XLSX && support && !kill)
- reserve_tokens = est_input + max_output_tokens_applied; reserved_credits = credits_micro(est_input, max_out, ...).
- period_start: daily = UTC date; monthly = first of month (DATE).
- Preflight check txn reads rows (no write); reserve txn increments then re-reads; over limit -> rollback 429.
- Usage known: completed always actual (0 usage => 0 credits); failed actual iff usage present with non-zero field;
  cancelled always estimated. Pre-provider failure => released (0).
- Estimated: charged credits = credits_micro(est_input, floor_applied, im, om) [but tokens min(reserve, est+floor)].
- Overshoot: actual_tokens > reserve_tokens on actual settlement: factor = actual/reserve; <= quota.overshoot_tolerance_factor
  -> commit actual; else commit reserved_credits (capped). Turn stays completed.
- Settlement row updates: total: spent += committed, reserved -= turn_reserved, calls += 1, input/output += actual
  (actual only), web_search/code_interpreter += counts (actual+estimated); premium: spent, reserved, calls.
- Usage event fields: tenant_id, user_id, chat_id, turn_id, request_id, dedupe_key "{tenant}/{turn}/{request}"
  (simple hex), selected_model, effective_model, terminal_state, billing_outcome, settlement_method,
  policy_version_applied, usage{input_tokens,output_tokens,cache_read_input_tokens,cache_write_input_tokens,
  reasoning_tokens}|null, actual_credits_micro, file_search_calls, web_search_calls, code_interpreter_calls,
  timestamp, requester_type.
- quota.overshoot_tolerance_factor default 1.10 (1.00..1.50).
- Usage event JSON (UsageEvent): tenant_id, user_id, chat_id, turn_id, request_id, effective_model, selected_model,
  terminal_state, billing_outcome, usage, actual_credits_micro, settlement_method, policy_version_applied,
  web_search_calls, code_interpreter_calls, file_search_calls, timestamp, requester_type, dedupe_key,
  system_task_type. dedupe_key simple hex. Partition: usage & audit by tenant_id; chat cleanup & summary by chat_id.
- Audit handler: deserialize first (bad -> Reject); plugin lookup none -> Ok(drop); client missing -> Retry;
  30s timeout transient -> Retry; permanent -> Reject; attempt 120 Retry -> Reject.
- Usage handler: publish_usage via policy plugin; transient -> Retry; bad payload/permanent -> Reject.

## §5.7–5.9 Finalization (single txn): CAS UPDATE chat_turns SET state, completed_at, ... WHERE id AND state='running'
  -> rows=1: insert assistant msg (completed; cancelled with non-empty text best-effort), settle quota, enqueue usage +
  audit (+summary). rows=0: no-op; provider task ends w/o terminal -> SSE error stream_interrupted.
- completed (incl incomplete): assistant_message_id set, error_code NULL; empty text still inserted.
- Persist failure on completed -> failed message_persistence_failed (settle actual/estimated) -> SSE error.
- Finalization txn failure on completed -> turn stays running; SSE error finalization_failed. On error paths -> original code.
- Billing outcome derivation: completed -> completed/actual (0 usage => 0 credits); failed + provider/tool/persistence
  codes -> failed/actual if usage has non-zero field else estimated; failed + context_length_exceeded|validation_error|
  input_too_long|turn_setup_failed -> failed/released; cancelled -> aborted/estimated (usage null);
  orphan_timeout -> aborted/estimated; unknown code -> failed/estimated.
- Usage payload usage=null on estimated paths; actual usage object on actual.
- Orphan with NULL reserve fields: usage event aborted/estimated, actual_credits 0, usage null, effective_model=""
  selected_model="" policy_version_applied 0; no quota change.
- Unstarted retry/edit failure: plain CAS to failed, no settlement, no outbox, no audit.
