# Acceptance trace: mini-chat

Maps every bullet of [`gears/mini-chat/docs/acceptance-criteria.md`](../../docs/acceptance-criteria.md)
to the Rust tests of `cf-gears-mini-chat` that cover it, and to the black-box
E2E tests of the self-managed suite in
[`testing/e2e/suites/mini_chat_dev/`](../../../../testing/e2e/suites/mini_chat_dev/README.md).

Paths:
- Rust: `tests/<file>.rs::<test>` (integration tests in this directory) and
  `src/<path>_tests.rs::<test>` (unit tests), relative to `gears/mini-chat/mini-chat/`.
- E2E: `<file>.py::<test>`, relative to `testing/e2e/suites/mini_chat_dev/`.
  The E2E suite runs the real `cf-gears-example-server` binary against an
  OpenAI-compatible mock and checks HTTP/SSE, the gear's SQLite database and
  the provider requests.

Commands: `cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat` and
`python3 -m pytest testing/e2e/suites/mini_chat_dev -q` (after
`cargo build --bin cf-gears-example-server --no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`).

## Principles & Constraints

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Tenant and owner isolation enforced on every resource | `tests/chats.rs::isolation`, `tests/repos.rs::owner_scoped_rows_are_invisible_to_other_users_and_tenants`, `tests/authz.rs::chat_scope_hides_other_users_chats_when_pdp_returns_tenant_only`, `tests/messages.rs::foreign_chat_messages_404`, `tests/turn_status.rs::foreign_and_unknown_turn_404`, `tests/reactions.rs::foreign_chat_404_chat`, `src/domain/authz_tests.rs::unconstrained_becomes_tenant_and_owner` | `test_chats.py::test_chats_are_isolated_between_users_and_tenants`, `test_mutations.py::test_mutations_are_owner_scoped`, `test_attachments.py::test_delete_attachment_cleanup_and_locking`, `test_misc.py::test_reactions_set_replace_remove`, `test_quota.py::test_quota_is_per_user`, `test_streaming.py::test_turn_status_404_cases` |
| Context window budget enforced, for both the input message and the full assembled request | `tests/streaming.rs::input_too_long_and_context_budget`, `src/domain/context_tests.rs::input_too_long_compares_the_estimate`, `src/domain/context_tests.rs::mandatory_over_budget_is_context_budget_exceeded`, `src/domain/context_tests.rs::truncates_oldest_whole_turns` | `test_streaming.py::test_preflight_validation_before_provider_call`, `test_misc.py::test_thread_summary_generated_and_applied` |
| Streaming responses are never buffered before relaying | `tests/streaming.rs::deltas_relayed_before_provider_finishes`, `src/infra/llm/gateway_tests.rs::stream_yields_deltas_before_completion` | `test_streaming.py::test_running_turn_conflicts_and_replay_before_parallel_guard`, `test_streaming.py::test_client_disconnect_cancels_turn` |
| A chat's model is immutable once set | `tests/chats.rs::patch_renames_and_ignores_model`, `src/domain/services/model_resolver_tests.rs::chat_model_resolution_ignores_enabled_flag` | `test_chats.py::test_get_update_delete_lifecycle`, `test_quota.py::test_premium_exhausted_downgrades_to_standard` |
| Quota is checked before any outbound provider call | `tests/streaming.rs::preflight_rejections_are_json_not_sse`, `tests/quota_status.rs::preflight_reads_rows_for_downgrade_and_tool_quota` | `test_quota.py::test_total_exhausted_is_429_before_provider_call`, `test_quota.py::test_web_search_daily_quota_is_429` |

## Chat CRUD

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Create / get / list / update / delete lifecycle for chats, including model and title validation | `tests/chats.rs::create_returns_201_with_location_and_default_model`, `tests/chats.rs::create_with_unknown_or_disabled_model_is_400_invalid_model`, `tests/chats.rs::title_validation`, `tests/chats.rs::patch_invalid_title_is_400`, `tests/chats.rs::get_and_delete`, `src/domain/services/chat_service_tests.rs::length_is_counted_in_chars_after_trim` | `test_chats.py::test_create_chat_returns_201_location_and_default_model`, `test_chats.py::test_create_chat_rejects_unknown_or_disabled_model`, `test_chats.py::test_create_chat_rejects_invalid_title`, `test_chats.py::test_create_chat_accepts_255_char_title`, `test_chats.py::test_get_update_delete_lifecycle`, `test_chats.py::test_update_chat_validation` |
| List endpoint supports filtering, ordering, and pagination, with validation of malformed input | `tests/chats.rs::list_orders_by_updated_at_desc_and_paginates`, `tests/chats.rs::list_filter_updated_at_exact_and_ranges`, `tests/chats.rs::list_orderby_title`, `tests/chats.rs::list_limit_zero_is_400`, `tests/chats.rs::list_bad_cursor_is_400`, `tests/chats.rs::list_unknown_field_is_400_odata_resource_type` | `test_chats.py::test_list_default_order_and_paging`, `test_chats.py::test_list_filter_and_orderby`, `test_chats.py::test_list_limit_clamped_and_zero_rejected`, `test_chats.py::test_list_rejects_malformed_query` |
| Chat ordering reflects most recent activity | `tests/streaming.rs::chat_list_order_reflects_last_send`, `tests/chats.rs::list_orders_and_pages_fractional_timestamps` | `test_chats.py::test_list_activity_from_send_moves_chat_to_top`, `test_chats.py::test_list_default_order_and_paging` |

## Messages API

| Criterion | Rust tests | E2E tests |
|---|---|---|
| List messages with filtering, ordering, and pagination | `tests/messages.rs::messages_filter_orderby_pagination` | `test_streaming.py::test_message_count_and_order_across_turns` |
| Message response contract: identity, attachments, and reaction fields are always consistently present | `tests/messages.rs::list_messages_chronological_with_required_fields`, `tests/reactions.rs::my_reaction_is_per_user` | `test_streaming.py::test_completed_turn_is_persisted`, `test_attachments.py::test_upload_image_has_thumbnail_and_is_sent_as_input_image`, `test_misc.py::test_reactions_set_replace_remove` |
| Message count and chronological ordering are tracked correctly across turns | `tests/messages.rs::message_count_tracks_turns`, `tests/chats.rs::get_returns_message_count_of_non_deleted_messages` | `test_streaming.py::test_message_count_and_order_across_turns`, `test_mutations.py::test_delete_last_turn` |

## Streaming: Send Message

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Send-message endpoint streams a response, correlated by a request id | `tests/streaming.rs::send_streams_started_deltas_done`, `tests/streaming.rs::server_generates_request_id_when_omitted` | `test_streaming.py::test_stream_event_order_and_payloads`, `test_streaming.py::test_server_generates_request_id_when_omitted` |
| Preflight validation (content, attachments, limits) runs before any provider call | `tests/streaming.rs::preflight_rejections_are_json_not_sse`, `tests/attachments_in_turns.rs::attachment_validation_in_reserve_txn`, `tests/attachments_in_turns.rs::too_many_images` | `test_streaming.py::test_preflight_validation_before_provider_call`, `test_attachments.py::test_attachment_ids_validation`, `test_attachments.py::test_image_rejected_on_model_without_vision` |
| Assistant message and usage are persisted once a stream completes | `tests/streaming.rs::send_streams_started_deltas_done`, `tests/quota_status.rs::settle_moves_reserve_to_spent` | `test_streaming.py::test_completed_turn_is_persisted`, `test_quota.py::test_turn_settles_actual_usage_and_status_matches` |

## SSE Event Contract

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering | `tests/streaming.rs::send_streams_started_deltas_done`, `tests/streaming.rs::citations_once_before_done`, `tests/streaming.rs::ping_before_first_delta`, `tests/attachments_in_turns.rs::web_citations_source_web`, `src/infra/llm/providers/openai_responses_tests.rs::parse_stream` | `test_streaming.py::test_stream_event_order_and_payloads`, `test_streaming.py::test_web_search_tool_events_citations_and_accounting`, `test_attachments.py::test_file_citations_map_to_attachment`, `test_streaming.py::test_ping_before_first_delta`, `test_streaming.py::test_provider_error_event_is_terminal_and_sanitized` |
| Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers | `tests/streaming.rs::premium_exhausted_downgrades`, `tests/streaming.rs::done_quota_warnings_entries`, `tests/streaming.rs::disabled_model_downgrades_with_model_disabled` | `test_streaming.py::test_stream_event_order_and_payloads`, `test_quota.py::test_premium_exhausted_downgrades_to_standard`, `test_misc.py::test_kill_switches` |
| Error event is terminal and carries a sanitized message | `tests/streaming.rs::provider_http_500_sanitized_error_event`, `src/infra/llm/sanitize_tests.rs::credentials_are_replaced`, `src/infra/llm/sanitize_tests.rs::response_ids_are_replaced` | `test_streaming.py::test_provider_error_event_is_terminal_and_sanitized`, `test_streaming.py::test_provider_auth_error_message_is_sanitized`, `test_streaming.py::test_provider_http_errors_map_to_stream_codes` |

## Idempotency & Replay

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Replaying a known request id returns the stored result without side effects (no provider call, no quota change) | `tests/streaming.rs::replay_completed_request_id`, `tests/streaming.rs::replay_with_downgrade_has_downgrade_from_without_reason` | `test_streaming.py::test_replay_of_completed_turn_is_side_effect_free`, `test_quota.py::test_premium_exhausted_downgrades_to_standard` |
| Conflicting reuse of a request id across turn states is rejected consistently | `tests/streaming.rs::request_id_conflict_for_failed_or_running_or_deleted`, `tests/turn_mutations.rs::old_request_id_after_retry_conflicts` | `test_streaming.py::test_running_turn_conflicts_and_replay_before_parallel_guard`, `test_streaming.py::test_failed_turn_request_id_is_conflict`, `test_mutations.py::test_retry_creates_new_turn_and_soft_deletes_old` |
| Replay is checked before the parallel-turn guard | `tests/streaming.rs::replay_checked_before_parallel_guard` | `test_streaming.py::test_running_turn_conflicts_and_replay_before_parallel_guard` |

## Parallel Turn Enforcement

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Only one turn may run per chat at a time | `tests/streaming.rs::concurrent_sends_one_wins`, `tests/streaming.rs::insert_race_on_running_index_is_turn_already_running`, `tests/schema.rs::one_running_turn_per_chat` | `test_streaming.py::test_parallel_sends_only_one_turn_runs`, `test_streaming.py::test_running_turn_conflicts_and_replay_before_parallel_guard` |
| A new turn is accepted once the previous one reaches a terminal state | `tests/streaming.rs::new_turn_accepted_after_terminal`, `tests/workers.rs::stuck_turn_unblocks_chat` | `test_streaming.py::test_running_turn_conflicts_and_replay_before_parallel_guard`, `test_streaming.py::test_client_disconnect_cancels_turn`, `test_misc.py::test_orphan_watchdog_finalizes_stale_turn_and_stream_is_interrupted` |

## Turn Mutations

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Retry / edit / delete act only on the latest, terminal turn | `tests/turn_mutations.rs::non_latest_is_409_not_latest_turn`, `tests/turn_mutations.rs::already_deleted_is_409_not_latest_turn`, `tests/turn_mutations.rs::running_is_400_turn_state`, `tests/turn_mutations.rs::delete_latest_204_and_audit` | `test_mutations.py::test_delete_last_turn`, `test_mutations.py::test_mutation_of_running_turn_is_rejected`, `test_mutations.py::test_retry_creates_new_turn_and_soft_deletes_old` |
| A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id | `tests/turn_mutations.rs::retry_latest_creates_new_turn`, `tests/turn_mutations.rs::quota_rejection_leaves_previous_turn_intact`, `tests/turn_mutations.rs::setup_failure_after_commit_marks_new_turn_failed`, `tests/turn_mutations.rs::reserve_recheck_on_retry_fails_new_turn_quota_exceeded` | `test_mutations.py::test_retry_creates_new_turn_and_soft_deletes_old`, `test_mutations.py::test_edit_replaces_content_and_regenerates`, `test_mutations.py::test_edit_runs_preflight_quota` |
| Concurrent mutations resolve deterministically | `tests/turn_mutations.rs::concurrent_retries_deterministic` | `test_mutations.py::test_retry_after_failure_and_concurrent_retries` |
| Mutated turns correctly carry forward attachment and tool-usage history | `tests/turn_mutations.rs::edit_uses_new_content_and_copies_attachments`, `tests/turn_mutations.rs::retry_resends_web_search_flag` | `test_mutations.py::test_retry_carries_attachments` |

## Turn Lifecycle

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Turn state machine (running → completed / cancelled / failed) is consistent end to end | `tests/turn_status.rs::status_mapping_running_done_error_cancelled`, `tests/schema.rs::state_check_constraint`, `tests/streaming.rs::stream_interrupted_when_cas_lost` | `test_streaming.py::test_completed_turn_is_persisted`, `test_streaming.py::test_client_disconnect_cancels_turn`, `test_streaming.py::test_provider_error_event_is_terminal_and_sanitized`, `test_misc.py::test_orphan_watchdog_finalizes_stale_turn_and_stream_is_interrupted` |
| Partial and null-content cases on cancellation or failure are handled correctly | `tests/streaming.rs::client_disconnect_cancels_turn`, `tests/turn_status.rs::error_code_and_assistant_id_omitted_when_null` | `test_streaming.py::test_client_disconnect_cancels_turn`, `test_streaming.py::test_provider_stream_cut_without_terminal_is_provider_error` |

## Attachments

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation | `tests/attachments.rs::upload_document_ready_201`, `tests/attachments.rs::upload_image_ready_with_thumbnail`, `tests/attachments.rs::get_delete_rules`, `tests/attachments.rs::document_size_limit_is_min_of_rag_and_model`, `tests/attachments.rs::unsupported_content_type_is_400`, `tests/attachments.rs::per_chat_document_limit_429`, `tests/attachments.rs::per_chat_storage_limit_429_ignores_failed` | `test_attachments.py::test_upload_document_ready_and_indexed`, `test_attachments.py::test_upload_image_has_thumbnail_and_is_sent_as_input_image`, `test_attachments.py::test_upload_validation_errors`, `test_attachments.py::test_delete_attachment_cleanup_and_locking` |
| Asynchronous indexing lifecycle, including provider failure and timeout handling | `tests/attachments.rs::indexing_still_running_returns_uploaded_then_ready`, `tests/attachments.rs::indexing_failed_is_503_and_row_failed`, `tests/attachments.rs::background_indexing_timeout_fails_with_cleanup`, `tests/attachments.rs::provider_upload_failure_is_503_upload_failed` | `test_attachments.py::test_indexing_failure_marks_attachment_failed`, `test_attachments.py::test_provider_file_upload_failure_is_503` |
| Attachments are correctly made available to the relevant provider tools | `tests/attachments_in_turns.rs::file_search_tool_only_after_ready_document`, `tests/attachments_in_turns.rs::code_interpreter_tool_with_ready_xlsx`, `tests/attachments_in_turns.rs::image_in_message_sent_as_input_image`, `src/domain/tools_tests.rs::file_search_requires_ready_docs_and_support_and_switch` | `test_attachments.py::test_upload_document_ready_and_indexed`, `test_attachments.py::test_code_interpreter_tool_for_xlsx`, `test_attachments.py::test_upload_image_has_thumbnail_and_is_sent_as_input_image`, `test_attachments.py::test_file_citations_map_to_attachment` |
| Cleanup and abandoned-upload recovery behave correctly under failure | `tests/outbox_handlers.rs::attachment_cleanup_deletes_provider_file`, `tests/outbox_handlers.rs::attachment_cleanup_failure_counts_and_terminal_failed`, `tests/workers.rs::reaper_marks_abandoned_and_enqueues_cleanup`, `tests/workers.rs::reaper_skips_rows_with_cleanup_status` | `test_attachments.py::test_delete_attachment_cleanup_and_locking`, `test_misc.py::test_upload_reaper_fails_abandoned_upload_and_deletes_provider_file` |

## Models API

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Read-only model list/get reflects only enabled catalog entries, without exposing internal fields | `tests/models_api.rs::list_returns_only_enabled_in_catalog_order`, `tests/models_api.rs::model_dto_has_no_internal_fields`, `tests/models_api.rs::get_disabled_or_unknown_is_404_model` | `test_misc.py::test_models_list_shows_only_enabled_without_internals`, `test_misc.py::test_get_model_and_hidden_models` |

## Reactions API

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Set/remove reaction on assistant messages only, idempotently | `tests/reactions.rs::set_like_then_dislike_replaces`, `tests/reactions.rs::remove_is_idempotent_204`, `tests/reactions.rs::user_message_rejected_400_reaction_target_for_put_and_delete`, `tests/reactions.rs::invalid_value_400_before_authz` | `test_misc.py::test_reactions_set_replace_remove`, `test_misc.py::test_reaction_validation` |

## Quota Status API

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Quota status reporting is accurate and consistent with actual usage | `tests/quota_status.rs::status_reports_premium_and_total_daily_monthly`, `tests/quota_status.rs::warnings_follow_status_math`, `src/domain/services/quota_service_tests.rs::period_status_math` | `test_quota.py::test_quota_status_shape_for_fresh_user`, `test_quota.py::test_turn_settles_actual_usage_and_status_matches`, `test_quota.py::test_reserve_is_held_while_streaming`, `test_quota.py::test_quota_warning_threshold` |

## Quota Enforcement

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Reserve-before-execute quota flow enforced on every provider call | `tests/streaming.rs::reserve_recheck_rejects_concurrent_booking`, `tests/quota_status.rs::reserve_recheck_rejects_concurrent_overbooking`, `tests/turn_mutations.rs::reserve_recheck_on_retry_fails_new_turn_quota_exceeded` | `test_quota.py::test_reserve_is_held_while_streaming`, `test_quota.py::test_total_exhausted_is_429_before_provider_call`, `test_mutations.py::test_edit_runs_preflight_quota` |
| Tier downgrade applied when a higher tier is exhausted | `tests/streaming.rs::premium_exhausted_downgrades`, `src/domain/services/quota_service_tests.rs::cascade_truth_tables`, `src/domain/services/quota_service_tests.rs::cascade_premium_kill_switches` | `test_quota.py::test_premium_exhausted_downgrades_to_standard`, `test_misc.py::test_kill_switches` |
| Credits and tokens are accounted correctly per model and tier | `src/domain/estimation_tests.rs::credits_ceil_per_component`, `tests/quota_status.rs::settle_moves_reserve_to_spent`, `src/domain/billing_tests.rs::settlement_overshoot_cap` | `test_quota.py::test_turn_settles_actual_usage_and_status_matches`, `test_quota.py::test_settlement_completed_is_actual` |

## Settlement & Finalization

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate | `src/domain/billing_tests.rs::billing_derivation_table`, `src/domain/billing_tests.rs::estimated_settlement`, `tests/streaming.rs::provider_failed_event_settles_actual_when_usage_nonzero`, `tests/workers.rs::orphan_finalizes_stale_running_turn`, `tests/workers.rs::cas_loser_noop`, `tests/db_retry.rs::with_retry_outlasts_a_writer_holding_the_lock` | `test_quota.py::test_settlement_completed_is_actual`, `test_quota.py::test_settlement_provider_error_without_usage_is_estimated`, `test_quota.py::test_settlement_provider_error_with_usage_is_actual`, `test_quota.py::test_settlement_cancelled_is_estimated_aborted`, `test_misc.py::test_orphan_watchdog_finalizes_stale_turn_and_stream_is_interrupted`, `test_misc.py::test_concurrent_load_has_no_server_errors` |
| Usage is published reliably and exactly once per turn | `tests/outbox_handlers.rs::usage_published_once`, `tests/outbox_enqueue.rs::usage_row_written_in_tx_and_rolled_back` | `test_misc.py::test_usage_and_audit_events_published_once_per_turn`, `test_streaming.py::test_replay_of_completed_turn_is_side_effect_free` |

## Context Assembly

| Criterion | Rust tests | E2E tests |
|---|---|---|
| System prompt, thread summary, and recent history are assembled and truncated deterministically within budget | `src/domain/context_tests.rs::order_is_system_summary_history_current`, `src/domain/context_tests.rs::truncates_oldest_whole_turns`, `src/domain/context_tests.rs::deterministic_same_inputs_same_plan`, `tests/thread_summary.rs::next_turn_uses_summary`, `tests/thread_summary.rs::recent_history_starts_after_frontier` | `test_streaming.py::test_provider_request_contents`, `test_misc.py::test_thread_summary_generated_and_applied` |
| Tool availability and guidance are reflected correctly in the assembled request | `src/domain/context_tests.rs::guards_appended_only_for_sent_tools`, `src/domain/tools_tests.rs::guards_follow_the_selected_tools`, `src/domain/tools_tests.rs::web_search_dropped_when_model_lacks_support`, `src/infra/llm/providers/openai_responses_tests.rs::body_with_all_tools` | `test_streaming.py::test_web_search_tool_events_citations_and_accounting`, `test_attachments.py::test_upload_document_ready_and_indexed`, `test_attachments.py::test_code_interpreter_tool_for_xlsx`, `test_misc.py::test_kill_switches` |

## Error Mapping & Sanitization

| Criterion | Rust tests | E2E tests |
|---|---|---|
| All errors map to the canonical error contract, consistently across REST and streaming | `src/api/rest/error_tests.rs::chat_not_found_resource_type`, `src/api/rest/error_tests.rs::quota_tokens_is_429_with_subject_tokens`, `src/api/rest/error_tests.rs::turn_already_running_reason`, `src/api/rest/error_tests.rs::internal_variants_are_500_without_leaking_detail`, `tests/streaming.rs::preflight_rejections_are_json_not_sse` | `test_misc.py::test_problem_envelope_fields`, `test_misc.py::test_unauthenticated_requests_are_401_problem`, `test_chats.py::test_list_rejects_malformed_query`, `test_streaming.py::test_provider_http_errors_map_to_stream_codes` |
| Provider-originated error details are sanitized before reaching the client | `src/infra/llm/sanitize_tests.rs::file_id_is_replaced`, `src/infra/llm/sanitize_tests.rs::url_is_replaced`, `src/infra/llm/gateway_tests.rs::http_500_json_error_is_provider_error_sanitized`, `src/infra/llm/storage/storage_tests.rs::error_messages_never_carry_provider_ids` | `test_streaming.py::test_provider_error_event_is_terminal_and_sanitized`, `test_streaming.py::test_provider_auth_error_message_is_sanitized` |

## Web Search

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Web search tool use is reported, cited, accounted, and quota-limited correctly | `tests/attachments_in_turns.rs::web_search_tool_and_daily_quota`, `tests/attachments_in_turns.rs::web_citations_source_web`, `tests/attachments_in_turns.rs::web_search_calls_counted_in_quota_usage`, `tests/streaming.rs::web_search_calls_exceeded_mid_stream` | `test_streaming.py::test_web_search_tool_events_citations_and_accounting`, `test_streaming.py::test_web_search_calls_over_limit_fail_turn`, `test_quota.py::test_web_search_daily_quota_is_429`, `test_misc.py::test_kill_switches` |

## Cleanup & Recovery

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Chat deletion triggers reliable background cleanup of provider-side resources | `tests/outbox_handlers.rs::chat_cleanup_deletes_files_then_vector_store`, `tests/outbox_handlers.rs::chat_cleanup_failed_attachment_does_not_block_vector_store`, `tests/outbox_handlers.rs::vector_store_delete_failure_retries_then_rejects_at_max`, `tests/outbox_handlers.rs::pipeline_end_to_end` | `test_misc.py::test_chat_delete_enqueues_cleanup_and_deletes_provider_resources` |
| Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly | `tests/thread_summary.rs::handler_commits_summary_and_marks_compressed`, `tests/thread_summary.rs::provider_failure_keeps_previous_summary_retry_then_reject`, `tests/thread_summary.rs::frontier_deleted_skips_commit`, `tests/turn_mutations.rs::mutation_deletes_covering_summary` | `test_misc.py::test_thread_summary_generated_and_applied`, `test_misc.py::test_thread_summary_retried_after_provider_failure`, `test_misc.py::test_turn_delete_invalidates_covering_summary` |

## Authorization

| Criterion | Rust tests | E2E tests |
|---|---|---|
| Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently | `tests/authz.rs::chat_scope_request_shape_for_read`, `tests/authz.rs::quota_scope_is_tenant_and_owner_of_subject`, `tests/authz.rs::deny_maps_to_authz_denied_and_failure_to_unavailable`, `tests/turn_mutations.rs::other_users_turn`, `tests/repos.rs::insert_outside_scope_is_denied` | `test_chats.py::test_chats_are_isolated_between_users_and_tenants`, `test_mutations.py::test_mutations_are_owner_scoped`, `test_attachments.py::test_delete_attachment_cleanup_and_locking`, `test_chats.py::test_missing_or_invalid_token_is_401` |
