# Gear configuration contract (`gears.mini-chat.config`)

Source: DESIGN Appendix B. `deny_unknown_fields` on: top level, `streaming`, `estimation_budgets`,
`quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>`,
`tenant_overrides.<tid>`, `thumbnail`, `knowledge_search`. Worker sections (`orphan_watchdog`,
`upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept unknown keys.

| Key | Default | Validation |
|---|---|---|
| url_prefix | `/mini-chat` | |
| vendor | `constructorfabric` | non-empty |
| client_credentials.client_id / client_secret | required | non-empty, `${VAR}` expansion |
| metrics.prefix | `""` (= mini_chat) | |
| providers.<id> | `openai` entry (openai_responses, api.openai.com, apikey `cred://openai-key`, storage_kind openai) | see below |
| streaming.sse_ping_interval_seconds | 15 | 5..=60 |
| streaming.sse_channel_capacity | 32 | 16..=64 |
| streaming.max_output_tokens | 32768 | >0 |
| estimation_budgets.minimal_generation_floor | 50 | >0, <= streaming.max_output_tokens |
| estimation_budgets.{bytes_per_token_conservative 4, fixed_overhead_tokens 100, safety_margin_pct 10, image_token_budget 1000, tool_surcharge_tokens 500, web_search_surcharge_tokens 500, code_interpreter_surcharge_tokens 1000} | | deprecated: warn if non-default |
| quota.overshoot_tolerance_factor | 1.10 | 1.0..=1.5 |
| quota.warning_threshold_pct | 80 | 1..=99 |
| quota.web_search_max_calls_per_message | 2 | >0 |
| quota.web_search_daily_quota | 75 | >0 |
| quota.code_interpreter_max_calls_per_message | 10 | >0 |
| quota.code_interpreter_daily_quota | 50 | >0 |
| outbox.queue_name | mini-chat.usage_snapshot | non-empty |
| outbox.cleanup_queue_name | mini-chat.attachment_cleanup | non-empty |
| outbox.chat_cleanup_queue_name | mini-chat.chat_cleanup | non-empty |
| outbox.thread_summary_queue_name | mini-chat.thread_summary | non-empty |
| outbox.audit_queue_name | mini-chat.audit | non-empty |
| outbox.num_partitions | 4 | power of two 1..=64 |
| context.recent_messages_limit | 10 | 0..=100 |
| context.web_search_guard / file_search_guard | built-in text | |
| rag.uploaded_file_max_size_kb | 25600 | >0 |
| rag.uploaded_image_max_size_kb | 5120 | >0 |
| rag.max_images_per_message | 4 | >0 |
| rag.max_documents_per_chat | 50 | >0 |
| rag.max_total_upload_mb_per_chat | 100 | >0 |
| rag.allow_csv_upload | true | |
| rag.max_concurrent_uploads | 10 | 1..=256 |
| thumbnail.{width 128, height 128, max_bytes 131072, max_pixels 100000000, max_decode_bytes 33554432} | | >0 |
| knowledge_search.{enabled false, vector_store_id, provider_id, max_calls_per_message 3, top_k 5, max_chunk_chars 2000, guard} | | required ids when enabled |
| orphan_watchdog.{enabled true, timeout_secs 300 (90..=3600), scan_interval_secs 60 (1..=3600)} | | |
| upload_reaper.{enabled true, scan_interval_secs 60 (1..=3600), stale_after_secs 300 (60..=86400)} | | |
| thread_summary_worker.{enabled true, claim_timeout_secs 300 (30..=3600), max_attempts 3, compression_threshold_pct 80 (1..=99), summary_model_id "" (=gpt-4.1-mini), summary_system_prompt, message_content_limit 4000, reconcile_interval_secs 60 (deprecated)} | | |
| cleanup_worker.{max_attempts 5, enabled, poll_interval_secs, reconcile_interval_secs, stale_in_progress_timeout_secs, batch_size (deprecated)} | | |

Provider entry: `kind` (openai_responses|openai_chat_completions|vllm_responses|anthropic_messages,
required), `host` (required, `${VAR}`, charset letters/digits/`.-_:[]`), `port` (443/80), `use_http`
(false), `upstream_alias` (= host), `api_path` (`/v1/responses`, `{model}` placeholder),
`auth_plugin_type`, `auth_config` (map, `${VAR}`), `storage_kind` (openai|azure, required),
`storage_backend` (= provider id), `api_version` (required for azure; letters/digits/`.-`),
`rag_provider`, `tenant_overrides.<tenant_id>` {host, upstream_alias, auth_plugin_type,
auth_config}.

Bundled plugin sections: `static-mini-chat-model-policy-plugin.config` {vendor, priority,
model_catalog (required when section present), kill_switches, default_standard_limits
(100_000_000 / 1_000_000_000), default_premium_limits (50_000_000 / 500_000_000)};
`static-mini-chat-audit-plugin.config` {enabled true, vendor, priority}.
