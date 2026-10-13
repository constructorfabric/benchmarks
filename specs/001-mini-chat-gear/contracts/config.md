# Configuration contract (DESIGN.md Appendix B)

Gear config section `gears.mini-chat.config`. Unknown keys rejected (deny_unknown_fields) at top level and in:
`streaming`, `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`,
`providers.<id>` (+ `tenant_overrides.<tid>`), `thumbnail`, `knowledge_search`.
Worker sections (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept+ignore
unknown keys. `${VAR}` expansion in provider `host`, `auth_config` (and overrides), `client_credentials`.

| Key | Default | Validation |
|---|---|---|
| url_prefix | `/mini-chat` | |
| vendor | `constructorfabric` | non-empty |
| client_credentials.client_id / client_secret | required | non-empty |
| metrics.prefix | "" (= mini_chat) | |
| providers.<id> | one `openai` entry: kind openai_responses, host api.openai.com, apikey auth secret `cred://openai-key`, storage_kind openai | rag_provider refs must exist |
| providers.kind | required | openai_responses / openai_chat_completions / vllm_responses / anthropic_messages |
| providers.host | required | non-empty; chars [A-Za-z0-9.-_:[]] |
| providers.port | 443 (80 if use_http) | != 0 |
| providers.use_http | false | |
| providers.upstream_alias | host | |
| providers.api_path | `/v1/responses` | `{model}` replaced by provider_model_id |
| providers.auth_plugin_type, auth_config | — | |
| providers.storage_kind | required (openai/azure) | azure requires api_version |
| providers.storage_backend | provider id | |
| providers.api_version | — | [A-Za-z0-9.-] |
| providers.rag_provider | — | |
| providers.tenant_overrides.<tid> | {} | host/upstream_alias/auth_plugin_type/auth_config; must set host or upstream_alias |
| streaming.sse_ping_interval_seconds | 15 | 5..=60 |
| streaming.sse_channel_capacity | 32 | 16..=64 |
| streaming.max_output_tokens | 32768 | |
| context.recent_messages_limit | 10 | 0..=100 |
| context.web_search_guard | "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request." | |
| context.file_search_guard | built-in | |
| estimation_budgets.minimal_generation_floor | 50 | >0, <= streaming.max_output_tokens |
| estimation_budgets.{bytes_per_token_conservative 4, fixed_overhead_tokens 100, safety_margin_pct 10, image_token_budget 1000, tool_surcharge_tokens 500, web_search_surcharge_tokens 500, code_interpreter_surcharge_tokens 1000} | deprecated | warn if non-default |
| quota.overshoot_tolerance_factor | 1.10 | 1.0..=1.5 |
| quota.warning_threshold_pct | 80 | 1..=99 |
| quota.web_search_max_calls_per_message | 2 | >0 |
| quota.web_search_daily_quota | 75 | >0 |
| quota.code_interpreter_max_calls_per_message | 10 | >0 |
| quota.code_interpreter_daily_quota | 50 | >0 |
| rag.max_documents_per_chat | 50 | >0 |
| rag.max_total_upload_mb_per_chat | 100 | >0 |
| rag.allow_csv_upload | true | text/csv -> text/plain |
| rag.max_concurrent_uploads | 10 | 1..=256 |
| rag.uploaded_file_max_size_kb | 25600 | >0 |
| rag.uploaded_image_max_size_kb | 5120 | >0 |
| rag.max_images_per_message | 4 | >0 |
| thumbnail.width/height | 128 | >0 |
| thumbnail.max_bytes | 131072 | |
| thumbnail.max_pixels | 100000000 | |
| thumbnail.max_decode_bytes | 33554432 | |
| knowledge_search.enabled | false | vector_store_id/provider_id required when enabled |
| knowledge_search.max_calls_per_message 3, top_k 5, max_chunk_chars 2000, guard | | |
| orphan_watchdog.enabled | true | |
| orphan_watchdog.timeout_secs | 300 | 90..=3600 |
| orphan_watchdog.scan_interval_secs | 60 | 1..=3600 |
| upload_reaper.enabled | true | |
| upload_reaper.scan_interval_secs | 60 | 1..=3600 |
| upload_reaper.stale_after_secs | 300 | 60..=86400 |
| cleanup_worker.max_attempts | 5 | >0 (+ deprecated enabled, poll_interval_secs, reconcile_interval_secs, stale_in_progress_timeout_secs, batch_size) |
| outbox.queue_name | mini-chat.usage_snapshot | non-empty |
| outbox.cleanup_queue_name | mini-chat.attachment_cleanup | |
| outbox.chat_cleanup_queue_name | mini-chat.chat_cleanup | |
| outbox.thread_summary_queue_name | mini-chat.thread_summary | |
| outbox.audit_queue_name | mini-chat.audit | |
| outbox.num_partitions | 4 | power of 2, 1..=64 |
| thread_summary_worker.enabled | true | |
| thread_summary_worker.claim_timeout_secs | 300 | 30..=3600 |
| thread_summary_worker.max_attempts | 3 | >0 |
| thread_summary_worker.compression_threshold_pct | 80 | 1..=99 |
| thread_summary_worker.summary_model_id | "" (= gpt-4.1-mini) | |
| thread_summary_worker.summary_system_prompt | built-in (B.5.5) | |
| thread_summary_worker.message_content_limit | 4000 | 0 = no truncation |
| thread_summary_worker.reconcile_interval_secs | 60 | deprecated |

## Bundled plugin gears
`static-mini-chat-model-policy-plugin`: vendor (constructorfabric), priority (100), model_catalog (required key
when section present; [] when absent; multipliers 1..=1e10, bytes_per_token_conservative != 0), kill_switches
(all false, deny unknown), default_standard_limits {daily 100_000_000, monthly 1_000_000_000},
default_premium_limits {daily 50_000_000, monthly 500_000_000}. Snapshot version fixed 1. Deny unknown top-level.
`static-mini-chat-audit-plugin`: enabled (true), vendor, priority. Deny unknown top-level.

## Prompts (B.5.5)
- Summary preamble: "This conversation has earlier messages that have been summarized. The summary below covers
  the earlier portion of the conversation. Recent messages follow after."
- Summary default system prompt: "You are a conversation summarizer. Given a conversation (and optionally an
  existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning)
  followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent
  information not present in the conversation."
- Opening: "Summarize the following conversation:" / existing: "The existing summary below covers the earlier
  conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary
  concise. If the combined information is too large, prioritize: current topic and recent decisions > user
  preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting
  the summary grow unboundedly." + `<existing_summary>` block + "New messages to incorporate:".
- Analysis instruction: see DESIGN.md lines 7008–7024 (copy verbatim).

## Upload reaper scan
select <=100 rows status IN (pending, uploaded) AND deleted_at IS NULL AND cleanup_status IS NULL AND updated_at <
cutoff order by updated_at; per row txn CAS -> failed/upload_abandoned; if provider_file_id: cleanup_status pending
+ enqueue attachment cleanup {event_type: attachment_upload_abandoned}.
