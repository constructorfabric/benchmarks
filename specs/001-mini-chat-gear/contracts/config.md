# Configuration contract (DESIGN Appendix B)

`gears.mini-chat.config` (top level `deny_unknown_fields`):

- `url_prefix` (`/mini-chat`), `vendor` (`constructorfabric`), `client_credentials{client_id, client_secret}` (required),
  `metrics{prefix}`
- `providers.<id>`: `kind`, `host`, `port`, `use_http`, `upstream_alias`, `api_path` (`/v1/responses`),
  `auth_plugin_type`, `auth_config{}`, `storage_kind` (`openai|azure`, required), `storage_backend`, `api_version`,
  `rag_provider`, `tenant_overrides.<tenant>{host, upstream_alias, auth_plugin_type, auth_config}`.
  Default: one entry `openai` (`openai_responses`, `api.openai.com`, apikey `cred://openai-key`, storage openai).
- `streaming{sse_ping_interval_seconds 15 (5..=60), sse_channel_capacity 32 (16..=64), max_output_tokens 32768}`
- `estimation_budgets{minimal_generation_floor 50, + deprecated fields}`
- `quota{overshoot_tolerance_factor 1.10 (1.0..=1.5), warning_threshold_pct 80 (1..=99),
  web_search_max_calls_per_message 2, web_search_daily_quota 75, code_interpreter_max_calls_per_message 10,
  code_interpreter_daily_quota 50}`
- `outbox{queue_name, cleanup_queue_name, chat_cleanup_queue_name, thread_summary_queue_name, audit_queue_name, num_partitions 4}`
- `context{recent_messages_limit 10 (0..=100), web_search_guard, file_search_guard}`
- `rag{max_documents_per_chat 50, max_total_upload_mb_per_chat 100, allow_csv_upload true, max_concurrent_uploads 10,
  uploaded_file_max_size_kb 25600, uploaded_image_max_size_kb 5120, max_images_per_message 4}`
- `thumbnail{width 128, height 128, max_bytes 131072, max_pixels 1e8, max_decode_bytes 33554432}`
- `knowledge_search{enabled false, vector_store_id, provider_id, max_calls_per_message 3, top_k 5, max_chunk_chars 2000, guard}`
- workers (accept unknown keys): `orphan_watchdog{enabled true, timeout_secs 300 (90..=3600), scan_interval_secs 60}`,
  `upload_reaper{enabled true, scan_interval_secs 60, stale_after_secs 300 (60..=86400)}`,
  `thread_summary_worker{enabled true, claim_timeout_secs 300, max_attempts 3, compression_threshold_pct 80,
  summary_model_id "", summary_system_prompt, message_content_limit 4000, reconcile_interval_secs}`,
  `cleanup_worker{max_attempts 5, deprecated fields}`

Plugins: `static-mini-chat-model-policy-plugin{vendor, priority, model_catalog[], kill_switches{}, default_standard_limits,
default_premium_limits}`; `static-mini-chat-audit-plugin{vendor, priority, enabled}`.
