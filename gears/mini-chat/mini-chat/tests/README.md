# mini-chat tests — acceptance-criteria traceability

Run everything with:

```sh
cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk --offline
```

Integration tests (`tests/*.rs`) drive the real gear services and REST router
against a temporary SQLite database, a scripted PDP (`TenantPdp`), recording
policy/audit plugins, and a fake OpenAI-compatible provider behind the
`ProviderTransport` seam (`tests/common/mod.rs`). Unit tests live next to the
code (`src/**/*_tests.rs`, `#[cfg(test)]` modules).

| Acceptance criterion (`docs/acceptance-criteria.md`) | Tests |
|---|---|
| **Principles** — tenant & owner isolation on every resource | `chats::isolation_between_users_and_tenants`, `chats::authz_denial_and_pdp_failure_fail_closed`, `turns::foreign_users_and_unknown_turns_get_404` |
| Context window budget (input message and full request) | `streaming::context_assembly_history_and_budget`, `context_tests::*` (`mandatory_overflow_is_rejected`, `drops_oldest_whole_turns_first`) |
| Streaming never buffered before relay | `persistence::deltas_are_relayed_without_buffering` |
| Chat model immutable once set | `chats::title_and_model_validation`, `chats::create_get_update_delete_lifecycle` |
| Quota checked before any provider call | `streaming::preflight_rejections_are_json_and_have_no_side_effects`, `streaming::quota_downgrade_and_rejection`, `turns::mutation_runs_full_preflight_and_leaves_turn_unchanged` |
| **Chat CRUD** lifecycle + model/title validation | `chats::create_get_update_delete_lifecycle`, `chats::title_and_model_validation` |
| List filtering/ordering/pagination + malformed input | `chats::list_pagination_filter_order_and_errors` |
| Ordering reflects latest activity | `chats::ordering_reflects_latest_activity_and_message_count` |
| **Messages** list filter/order/pagination | `messages::message_list_contract_order_filter_and_pagination` |
| Message response contract (identity, attachments, reaction always present) | `messages::message_list_contract_order_filter_and_pagination`, `attachments::xlsx_enables_code_interpreter_and_image_is_input_image` |
| Message count & chronological order across turns | `messages::message_list_contract_order_filter_and_pagination`, `chats::ordering_reflects_latest_activity_and_message_count` |
| **Streaming** send correlated by request id | `streaming::stream_event_order_request_id_and_persistence`, `streaming::server_generates_request_id_when_omitted` |
| Preflight validation before provider call | `streaming::preflight_rejections_are_json_and_have_no_side_effects`, `streaming::kill_switches_and_vision_guards`, `attachments::invalid_attachment_ids_are_rejected_before_the_turn` |
| Assistant message + usage persisted on completion | `streaming::stream_event_order_request_id_and_persistence`, `streaming::premium_turn_reserves_and_settles_both_buckets` |
| **SSE contract** (start, delta, tool, citations, done, error, ping) and order | `streaming::stream_event_order_request_id_and_persistence`, `streaming::ping_only_before_first_content`, `streaming::web_search_tool_citations_limits_and_quota`, `attachments::file_citations_map_to_attachment`, `llm_tests::translates_text_tools_and_completion` |
| `done` exposes usage + quota/downgrade without internal ids | `streaming::quota_downgrade_and_rejection`, `streaming::stream_event_order_request_id_and_persistence` |
| Error event terminal + sanitized | `streaming::provider_errors_are_sanitized_terminal_errors`, `persistence::sse_stream_interrupted_when_task_ends_without_terminal` |
| **Replay** side-effect free | `streaming::replay_is_side_effect_free_and_conflicts_are_rejected` |
| Conflicting request-id reuse rejected | `streaming::replay_is_side_effect_free_and_conflicts_are_rejected`, `turns::retry_latest_turn_replaces_it` |
| Replay checked before parallel guard | `streaming::parallel_turn_guard_and_replay_priority` |
| **Parallel turns** — one running turn per chat | `streaming::parallel_turn_guard_and_replay_priority`, `persistence::one_running_turn_per_chat_and_cas_finalization` |
| New turn accepted after terminal | `streaming::parallel_turn_guard_and_replay_priority` |
| **Turn mutations** latest + terminal only | `turns::only_latest_turn_can_be_mutated`, `turns::running_turn_cannot_be_mutated`, `turns::delete_latest_turn` |
| Mutation runs full pipeline, new request id | `turns::retry_latest_turn_replaces_it`, `turns::edit_latest_turn`, `turns::mutation_runs_full_preflight_and_leaves_turn_unchanged` |
| Concurrent mutations deterministic | `turns::concurrent_retries_resolve_deterministically` |
| Attachments / tool history carried forward | `turns::retry_carries_attachments_forward` |
| **Turn lifecycle** state machine | `turns::turn_status_running_then_done`, `turns::turn_status_error_on_provider_failure`, `turns::turn_status_cancelled_with_partial_content`, `persistence::one_running_turn_per_chat_and_cas_finalization` |
| Partial / null content on cancel or failure | `turns::turn_status_cancelled_with_partial_content`, `turns::turn_status_cancelled_without_content` |
| **Attachments** upload/get/delete + size/type/limit validation | `attachments::upload_text_document_is_indexed_and_hides_provider_ids`, `attachments::upload_png_image_gets_thumbnail_and_no_vector_store`, `attachments::upload_validation_errors`, `attachments::upload_oversize_image_is_rejected`, `attachments::upload_document_count_limit`, `attachments::upload_total_size_limit`, `attachments::upload_xlsx_without_code_interpreter_is_rejected`, `attachments::upload_image_with_disable_images_kill_switch` |
| Async indexing incl. failure / timeout | `attachments::indexing_failure_marks_row_failed_and_deletes_file`, `attachments::provider_upload_failure_marks_row_failed` |
| Attachments available to provider tools | `attachments::ready_document_enables_file_search`, `attachments::xlsx_enables_code_interpreter_and_image_is_input_image`, `attachments::file_citations_map_to_attachment` |
| Cleanup & abandoned-upload recovery | `attachments::delete_unreferenced_attachment_cleans_up_provider_file`, `attachments::delete_referenced_attachment_is_locked`, `attachments::chat_deletion_cleanup_gives_up_after_max_attempts`, `attachments::upload_reaper_fails_abandoned_rows` |
| **Models API** enabled only, no internal fields | `chats::models_api_hides_disabled_and_internal_fields` |
| **Reactions** assistant only, idempotent | `messages::reactions_on_assistant_messages_only_and_idempotent` |
| **Quota status** accurate & consistent | `chats::quota_status_reflects_usage`, `quota_tests::status_skips_zero_limits_and_flags` |
| **Quota enforcement** reserve-before-execute | `streaming::premium_turn_reserves_and_settles_both_buckets`, `quota_tests::*` |
| Tier downgrade | `streaming::quota_downgrade_and_rejection`, `quota_tests::premium_exhausted_downgrades`, `quota_tests::standard_never_upgrades` |
| Credits/tokens per model & tier | `estimate_tests::credits_round_per_component`, `quota_tests::surcharges_follow_tool_support`, `streaming::premium_turn_reserves_and_settles_both_buckets` |
| **Settlement** exactly once, actual vs estimated | `quota_tests::billing_derivation_table`, `quota_tests::settlement_amounts`, `persistence::one_running_turn_per_chat_and_cas_finalization`, `background::orphan_watchdog_finalizes_stale_turns_with_estimated_charge` |
| Usage published reliably, once per turn | `streaming::usage_published_once_with_canonical_dedupe_key`, `background::usage_event_published_exactly_once_per_turn` |
| **Context assembly** deterministic within budget | `context_tests::*`, `streaming::context_assembly_history_and_budget`, `background::thread_summary_is_created_applied_and_invalidated` |
| Tool availability & guidance in request | `attachments::ready_document_enables_file_search`, `streaming::web_search_tool_citations_limits_and_quota`, `llm_tests::responses_body_shape` |
| **Error mapping** canonical across REST & SSE | `error_tests::*`, `streaming::preflight_rejections_are_json_and_have_no_side_effects`, `streaming::provider_errors_are_sanitized_terminal_errors` |
| Provider error details sanitized | `streaming::provider_errors_are_sanitized_terminal_errors`, `sanitize` unit tests |
| **Web search** reported, cited, accounted, limited | `streaming::web_search_tool_citations_limits_and_quota`, `quota_tests::daily_tool_quotas_apply_only_when_tool_is_sent` |
| **Cleanup** chat deletion → provider resources | `attachments::chat_deletion_cleans_up_files_and_vector_store`, `attachments::chat_deletion_cleanup_gives_up_after_max_attempts` |
| Thread summary success/failure/invalidation | `background::thread_summary_is_created_applied_and_invalidated`, `background::empty_summary_creates_no_thread_summary`, `summary` unit tests |
| **Authorization** owner scoping, cross-tenant rejected | `chats::isolation_between_users_and_tenants`, `chats::authz_denial_and_pdp_failure_fail_closed`, `turns::foreign_users_and_unknown_turns_get_404` |
| Persistence schema (DESIGN §3.7) | `persistence::schema_has_design_tables_and_columns` |
| Concurrent writers never surface SQLite lock errors | `persistence::concurrent_writers_do_not_surface_lock_errors`, `error_tests::quota_permission_and_availability` |
| Orphan watchdog (stale vs. progressing turns, estimated settlement, CAS loser) | `background::orphan_watchdog_finalizes_stale_turns_with_estimated_charge`, `persistence::one_running_turn_per_chat_and_cas_finalization` |
| Configuration (DESIGN Appendix B) | `config_tests::*` |
| SDK contract | `mini-chat-sdk` `sdk_tests::*` |
