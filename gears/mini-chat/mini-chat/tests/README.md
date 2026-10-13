# mini-chat tests

| Layer | Location | Run |
|-------|----------|-----|
| Unit tests (pure logic) | `src/**/*_tests.rs`, `../mini-chat-sdk/src/*_tests.rs` | `cargo test -p cf-gears-mini-chat --lib`, `cargo test -p cf-gears-mini-chat-sdk` |
| Integration tests | `tests/*.rs` (harness in `tests/common/`) | `cargo test -p cf-gears-mini-chat --tests` |
| Black-box smoke suite | `tests/e2e_smoke/` | `tests/e2e_smoke/run_smoke.sh` (see its README) |

The integration harness (`tests/common/mod.rs`) builds the full service graph: a file-backed SQLite
database with the gear and outbox migrations, the real OAGW data plane from
`oagw::test_support::build_test_gateway` (in-memory control plane, mock credstore holding
`openai-key`), an in-process OpenAI-compatible mock provider (`tests/common/mock_provider.rs`), a
configurable PDP stub, scriptable model-policy / audit plugins and the running outbox pipeline.
Requests go through the gear's axum router; the caller is selected with the `x-test-user` header
(`a1`, `a2` = same tenant, `b` = other tenant).

## Acceptance criteria → tests

AC numbers follow `specs/001-mini-chat-gear/spec.md` (same order as `docs/acceptance-criteria.md`).

| AC | Acceptance item | Tests |
|----|-----------------|-------|
| AC-01 | Tenant and owner isolation | `chats_api::chats_are_isolated_per_owner_and_tenant`, `attachments::document_upload_indexes_into_the_chat_vector_store` (404 for a2), `messages_turns::reactions_on_assistant_messages` (tenant b), `messages_turns::turn_status_states_and_not_found` |
| AC-02 | Context window budget (message + assembled request) | `acceptance_gaps::input_message_and_assembled_request_budgets`, unit `domain::context_tests` |
| AC-03 | Streaming never buffered | `acceptance_gaps::deltas_are_relayed_before_the_provider_finishes` |
| AC-04 | Chat model immutable | `acceptance_gaps::chat_model_is_immutable` |
| AC-05 | Quota checked before provider calls | `messages_turns::premium_exhaustion_downgrades_and_total_exhaustion_rejects`, `streaming::web_search_daily_quota_is_checked_at_preflight` |
| AC-06 | Chat CRUD with model/title validation | `chats_api::create_chat_uses_default_model_and_returns_location`, `create_chat_with_explicit_and_invalid_models`, `title_validation`, `get_rename_delete_lifecycle` |
| AC-07 | List filtering/ordering/pagination/validation | `chats_api::list_orders_by_updated_at_desc_and_paginates`, `list_supports_filter_and_orderby`, `malformed_json_bodies_map_to_canonical_errors` |
| AC-08 | Ordering by most recent activity | `acceptance_gaps::chat_list_reflects_most_recent_activity` |
| AC-09 | Message list filtering/ordering/pagination | `messages_turns::message_list_order_filter_and_paging` |
| AC-10 | Message response contract | `messages_turns::message_list_order_filter_and_paging`, `streaming::happy_stream_event_contract_and_persistence` |
| AC-11 | Message count and chronological order | `streaming::happy_stream_event_contract_and_persistence`, `streaming::history_is_sent_on_the_next_turn`, `messages_turns::edit_and_delete_latest_turn` |
| AC-12 | Send-message stream correlated by request id | `streaming::happy_stream_event_contract_and_persistence` |
| AC-13 | Preflight validation before provider calls | `streaming::preflight_validation_errors_open_no_stream`, `attachments::vision_is_checked_against_the_effective_model`, `attachments::too_many_images_is_rejected` |
| AC-14 | Assistant message and usage persisted on completion | `streaming::happy_stream_event_contract_and_persistence`, `streaming::usage_and_audit_events_are_published_once_per_turn` |
| AC-15 | Event contract and ordering (incl. tools, citations, keepalive) | `streaming::happy_stream_event_contract_and_persistence`, `streaming::web_search_tool_events_citations_and_limits`, `attachments::ready_attachments_are_wired_into_provider_tools`, `acceptance_gaps::ping_events_are_sent_until_the_first_content` |
| AC-16 | `done` exposes usage and quota outcome, no internal ids | `streaming::happy_stream_event_contract_and_persistence`, `messages_turns::premium_exhaustion_downgrades_and_total_exhaustion_rejects` |
| AC-17 | Terminal sanitized error event | `streaming::provider_failures_terminate_with_sanitized_error_events` |
| AC-18 | Replay without side effects | `streaming::replay_of_completed_request_is_side_effect_free` |
| AC-19 | Conflicting request id reuse | `streaming::parallel_turn_and_running_request_id_are_rejected` |
| AC-20 | Replay before the parallel-turn guard | `acceptance_gaps::replay_is_checked_before_the_parallel_turn_guard` |
| AC-21 | One running turn per chat | `streaming::parallel_turn_and_running_request_id_are_rejected` |
| AC-22 | New turn accepted after terminal state | `streaming::history_is_sent_on_the_next_turn`, `streaming::provider_failures_terminate_with_sanitized_error_events` |
| AC-23 | Mutations only on the latest terminal turn | `messages_turns::retry_replaces_the_latest_turn`, `edit_and_delete_latest_turn`, `mutation_of_a_running_turn_is_rejected` |
| AC-24 | Mutations use the full pipeline with a new request id | `messages_turns::retry_replaces_the_latest_turn`, `edit_and_delete_latest_turn` |
| AC-25 | Concurrent mutations deterministic | `acceptance_gaps::concurrent_mutations_resolve_to_a_single_winner` |
| AC-26 | Mutations carry forward attachments and tool history | `acceptance_gaps::retry_carries_forward_attachments_and_web_search` |
| AC-27 | Turn state machine end to end | `messages_turns::turn_status_states_and_not_found`, `streaming::client_disconnect_cancels_the_turn_and_settles_estimated`, `background::orphan_watchdog_finalizes_stale_running_turns` |
| AC-28 | Partial / null content on cancellation or failure | `streaming::client_disconnect_cancels_the_turn_and_settles_estimated`, `acceptance_gaps::cancellation_before_any_content_persists_no_assistant_message` |
| AC-29 | Attachment upload/get/delete with validation and limits | `attachments::*` (`document_upload…`, `image_upload…`, `upload_validation_errors`, `kill_switches_and_capabilities_gate_uploads`, `per_chat_limits`, `delete_attachment_lifecycle`, `model_removed_from_catalog_rejects_upload_before_body`), unit `infra::mime_tests`, `infra::thumbnail_tests` |
| AC-30 | Asynchronous indexing incl. failure and timeout | `attachments::provider_failures_mark_the_row_failed_and_return_503`, `background::slow_indexing_returns_uploaded_and_finishes_in_the_background` |
| AC-31 | Attachments available to provider tools | `attachments::ready_attachments_are_wired_into_provider_tools` |
| AC-32 | Cleanup and abandoned-upload recovery | `background::chat_deletion_cleans_provider_files_then_the_vector_store`, `failing_provider_deletes_are_retried_then_marked_failed`, `upload_reaper_fails_abandoned_uploads_and_schedules_cleanup`, `attachments::delete_attachment_lifecycle` |
| AC-33 | Models API | `messages_turns::models_api_lists_enabled_models_without_internal_fields` |
| AC-34 | Reactions | `messages_turns::reactions_on_assistant_messages` |
| AC-35 | Quota status | `messages_turns::quota_status_reflects_usage` |
| AC-36 | Reserve-before-execute | `acceptance_gaps::reserve_is_taken_before_the_provider_call_and_released_after`, `messages_turns::premium_exhaustion_downgrades_and_total_exhaustion_rejects`, unit `domain::quota_tests` |
| AC-37 | Tier downgrade | `messages_turns::premium_exhaustion_downgrades_and_total_exhaustion_rejects`, `kill_switches_force_standard_tier`, unit `domain::quota_tests` |
| AC-38 | Credits and tokens per model and tier | `streaming::usage_and_audit_events_are_published_once_per_turn`, `acceptance_gaps::downgraded_turns_are_charged_to_the_standard_tier_only`, unit `domain::credits_tests` |
| AC-39 | Exactly-once settlement per terminal outcome | `streaming::usage_and_audit_events_are_published_once_per_turn`, `streaming::client_disconnect_cancels_the_turn_and_settles_estimated`, `background::orphan_watchdog_finalizes_stale_running_turns` |
| AC-40 | Usage published reliably once per turn | `streaming::usage_and_audit_events_are_published_once_per_turn`, `background::thread_summary_is_generated_committed_and_used` (system task) |
| AC-41 | Deterministic context assembly and truncation | unit `domain::context_tests`, `background::thread_summary_is_generated_committed_and_used`, `streaming::history_is_sent_on_the_next_turn` |
| AC-42 | Tool availability and guidance | `streaming::web_search_tool_events_citations_and_limits`, `attachments::ready_attachments_are_wired_into_provider_tools`, `streaming::provider_request_wire_format` |
| AC-43 | Canonical error mapping (REST + streaming) | unit `domain::error_tests`, `chats_api::pdp_deny_and_failure_map_to_403_and_503`, `chats_api::malformed_json_bodies_map_to_canonical_errors` |
| AC-44 | Provider error sanitization | unit `domain::sanitize_tests`, `streaming::provider_failures_terminate_with_sanitized_error_events` |
| AC-45 | Web search reporting, citations, accounting, limits | `streaming::web_search_tool_events_citations_and_limits`, `streaming::web_search_daily_quota_is_checked_at_preflight` |
| AC-46 | Chat deletion cleanup | `background::chat_deletion_cleans_provider_files_then_the_vector_store`, `background::failing_provider_deletes_are_retried_then_marked_failed` |
| AC-47 | Thread summary lifecycle | `background::thread_summary_is_generated_committed_and_used`, `summary_model_unavailable_rejects_the_task_and_failures_keep_state`, `provider_failure_of_the_summary_call_keeps_the_previous_state`, `mutation_of_a_summarized_turn_invalidates_the_summary`, unit `infra::outbox::thread_summary_tests` |
| AC-48 | Owner scoping, consistent foreign-access rejection | `chats_api::chats_are_isolated_per_owner_and_tenant`, `chats_api::pdp_deny_and_failure_map_to_403_and_503` |
