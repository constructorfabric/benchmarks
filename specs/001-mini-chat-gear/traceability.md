# Traceability: acceptance criteria → automated tests

Source: `gears/mini-chat/docs/acceptance-criteria.md`. Paths are relative to
`gears/mini-chat/mini-chat/` (`G`) unless noted. Router tests drive the real axum
router (in-process, SQLite, real migrations + outbox, fake OAGW provider);
black-box tests (`tests/blackbox/`, `BB`) drive the debug server binary against a
mock OpenAI-compatible provider.

| Acceptance item | Automated tests |
|---|---|
| **Principles** — tenant and owner isolation on every resource | `src/api/rest/authz_tests.rs::foreign_users_and_tenants_get_404_everywhere`, `chats_tests.rs::foreign_and_cross_tenant_access_is_404`, `attachments_tests.rs::get_delete_ownership_and_cleanup`, `quota_tests.rs::quota_status_endpoint_matches_usage` (per-user), BB `test_chats_and_stream.py` |
| Context window budget (input message and full request) | `stream_tests.rs::context_budget_is_enforced_for_message_and_full_request`, `history_is_truncated_by_whole_turns_within_budget`, `mutations_tests.rs::post_commit_setup_failure_marks_new_turn_failed`, `domain/context_tests.rs`, `domain/estimate_tests.rs` |
| Streaming never buffered | `stream_tests.rs::deltas_are_relayed_without_buffering`, `ping_before_first_content` |
| Chat model immutable | `chats_tests.rs::get_update_delete_lifecycle_and_model_immutability` |
| Quota checked before any provider call | `quota_tests.rs::premium_exhaustion_downgrades_then_rejects`, `mutations_tests.rs::preflight_rejection_leaves_previous_turn`, `stream_tests.rs::preflight_validation_happens_before_provider_call` |
| **Chat CRUD** lifecycle + validation | `chats_tests.rs::{create_uses_default_model_and_returns_location, create_validates_title_and_model, get_update_delete_lifecycle_and_model_immutability, request_shape_errors}` |
| List filtering / ordering / pagination / malformed input | `chats_tests.rs::list_orders_by_activity_and_paginates`, `domain/odata_compat_tests.rs` |
| Ordering by most recent activity | `chats_tests.rs::list_orders_by_activity_and_paginates` |
| **Messages API** list with filter/order/pagination | `stream_tests.rs::server_generates_request_id_and_history_is_sent` (incl. `$filter=id eq '<uuid>'`, cursor continuation) |
| Message contract (identity, attachments, reactions always present) | `stream_tests.rs::stream_contract_and_persistence`, `attachments_tests.rs::document_upload_indexes_and_feeds_file_search`, `models_reactions_tests.rs::reactions_set_replace_remove_idempotently` |
| Message count and chronological order across turns | `stream_tests.rs::{stream_contract_and_persistence, server_generates_request_id_and_history_is_sent}`, `mutations_tests.rs::delete_last_turn_and_not_latest_rules` |
| **Streaming** send correlated by request id | `stream_tests.rs::{stream_contract_and_persistence, server_generates_request_id_and_history_is_sent}` |
| Preflight validation before provider call | `stream_tests.rs::preflight_validation_happens_before_provider_call`, `attachments_tests.rs::image_guards` |
| Assistant message and usage persisted on completion | `stream_tests.rs::stream_contract_and_persistence`, `quota_tests.rs::reserve_during_stream_then_actual_settlement` |
| **SSE contract** (start, delta, tool, citations, done, error, ping) + ordering | `stream_tests.rs::{stream_contract_and_persistence, ping_before_first_content}`, `attachments_tests.rs::file_citations_map_to_attachments_without_provider_ids`, `quota_tests.rs::web_search_tool_citations_accounting_and_limits`, `infra/llm/openai_responses_tests.rs`, `infra/llm/sse_parser_tests.rs` |
| `done` exposes usage + quota/downgrade without internal ids | `stream_tests.rs::stream_contract_and_persistence`, `quota_tests.rs::{premium_exhaustion_downgrades_then_rejects, kill_switches_force_standard_and_disable_web_search, disabled_chat_model_downgrades_with_model_disabled}` |
| `error` terminal + sanitized | `stream_tests.rs::provider_errors_map_to_sanitized_sse_errors`, `domain/sanitize_tests.rs` |
| **Replay** without side effects | `turns_tests.rs::replay_returns_stored_result_without_side_effects` |
| Conflicting request id reuse | `turns_tests.rs::request_id_reuse_conflicts_and_parallel_guard`, `mutations_tests.rs::retry_creates_new_turn_with_server_request_id` |
| Replay before parallel guard | `turns_tests.rs::replay_is_checked_before_parallel_guard` |
| **Parallel turns**: one running turn per chat | `turns_tests.rs::request_id_reuse_conflicts_and_parallel_guard` |
| New turn accepted after terminal state | `turns_tests.rs::{request_id_reuse_conflicts_and_parallel_guard, client_disconnect_cancels_with_partial_content_and_estimated_settlement}` |
| **Mutations** only latest terminal turn | `mutations_tests.rs::{delete_last_turn_and_not_latest_rules, running_turn_and_ownership_rules}` |
| Full send pipeline + new request id | `mutations_tests.rs::{retry_creates_new_turn_with_server_request_id, edit_replaces_content_and_validates, preflight_rejection_leaves_previous_turn, post_commit_setup_failure_marks_new_turn_failed}` |
| Concurrent mutations deterministic | `mutations_tests.rs::concurrent_mutations_resolve_deterministically` |
| Attachments and tool history carried forward | `attachments_tests.rs::retry_and_edit_carry_attachments_forward`, `mutations_tests.rs::retry_after_failure_and_web_search_flag_carried_forward` |
| **Turn lifecycle** state machine end to end | `turns_tests.rs::{turn_status_endpoint_maps_states, deleted_turn_status_is_404, orphan_watchdog_finalizes_stale_turns_once}`, `stream_tests.rs::provider_errors_map_to_sanitized_sse_errors` |
| Partial / null content on cancel or failure | `turns_tests.rs::{client_disconnect_cancels_with_partial_content_and_estimated_settlement, disconnect_before_content_cancels_without_message}`, `stream_tests.rs::{provider_errors_map_to_sanitized_sse_errors, incomplete_completes_and_empty_answer_completes}` |
| **Attachments** upload/get/delete + size/type/limits | `attachments_tests.rs::{document_upload_indexes_and_feeds_file_search, image_upload_thumbnail_and_multimodal_input, upload_validation_errors, code_interpreter_only_file_needs_code_interpreter, disable_images_kill_switch, get_delete_ownership_and_cleanup}`, `domain/attachment_service_tests.rs`, `domain/thumbnail_tests.rs` |
| Async indexing incl. failure / timeout | `attachments_tests.rs::{indexing_failure_returns_503_and_marks_failed, slow_indexing_returns_uploaded_then_background_ready}` |
| Attachments available to provider tools | `attachments_tests.rs::{document_upload_indexes_and_feeds_file_search, image_upload_thumbnail_and_multimodal_input, code_interpreter_only_file_needs_code_interpreter, image_guards}` |
| Cleanup and abandoned-upload recovery | `attachments_tests.rs::{chat_deletion_cleans_provider_resources, upload_reaper_marks_abandoned_rows, get_delete_ownership_and_cleanup}` |
| **Models API** enabled only, no internals | `models_reactions_tests.rs::models_list_and_get_show_only_enabled_without_internals` |
| **Reactions** assistant only, idempotent | `models_reactions_tests.rs::{reactions_set_replace_remove_idempotently, reaction_value_is_validated_before_authorization}` |
| **Quota status** accurate | `quota_tests.rs::quota_status_endpoint_matches_usage`, `domain/quota_tests.rs` |
| **Quota enforcement** reserve-before-execute | `quota_tests.rs::reserve_during_stream_then_actual_settlement`, `domain/quota_tests.rs` |
| Tier downgrade | `quota_tests.rs::{premium_exhaustion_downgrades_then_rejects, kill_switches_force_standard_and_disable_web_search, disabled_chat_model_downgrades_with_model_disabled}`, `domain/quota_tests.rs` |
| Credits/tokens per model and tier | `quota_tests.rs::reserve_during_stream_then_actual_settlement`, `domain/credits_tests.rs`, `domain/billing_tests.rs` |
| **Settlement** exactly once, actual vs estimated | `quota_tests.rs::{reserve_during_stream_then_actual_settlement, overshoot_beyond_tolerance_is_capped_and_turn_stays_completed, provider_failure_without_usage_settles_estimated, web_search_calls_exceeded_mid_turn}`, `turns_tests.rs::{client_disconnect_cancels_with_partial_content_and_estimated_settlement, orphan_watchdog_finalizes_stale_turns_once, orphan_without_reserve_fields_is_finalized_without_settlement}`, `domain/billing_tests.rs` |
| Usage published reliably, once per turn | `quota_tests.rs::reserve_during_stream_then_actual_settlement`, `turns_tests.rs::{replay_returns_stored_result_without_side_effects, orphan_watchdog_finalizes_stale_turns_once}` |
| **Context assembly** deterministic within budget | `domain/context_tests.rs`, `stream_tests.rs::history_is_truncated_by_whole_turns_within_budget`, `summary_tests.rs::long_chat_gets_summarized_and_summary_is_used` |
| Tool availability + guidance | `knowledge_search_tests.rs::{knowledge_search_loop_retrieves_and_answers, retrievals_beyond_the_limit_get_search_limit_reached, agentic_loop_is_hard_capped, file_search_wins_over_knowledge_search, knowledge_search_off_when_provider_cannot_serve_it}`, `quota_tests.rs::web_search_tool_citations_accounting_and_limits`, `attachments_tests.rs::{document_upload_indexes_and_feeds_file_search, code_interpreter_only_file_needs_code_interpreter}`, `stream_tests.rs::stream_contract_and_persistence` (no tools by default) |
| **Error mapping** canonical across REST and SSE | `domain/error_tests.rs`, `stream_tests.rs::function_tool_use_without_knowledge_search_is_unexpected_tool_use`, `knowledge_search_tests.rs::other_function_tools_are_unexpected_even_with_knowledge_search`, `infra/llm/openai_responses_tests.rs::{function_calls_are_translated_by_every_adapter, tool_exchanges_are_replayed_in_each_adapter_format}`, `chats_tests.rs::request_shape_errors`, `stream_tests.rs::provider_errors_map_to_sanitized_sse_errors`, every `assert_problem` call |
| Provider error sanitization | `domain/sanitize_tests.rs`, `stream_tests.rs::provider_errors_map_to_sanitized_sse_errors` |
| **Web search** reported, cited, accounted, limited | `quota_tests.rs::{web_search_tool_citations_accounting_and_limits, web_search_calls_exceeded_mid_turn, kill_switches_force_standard_and_disable_web_search}` |
| **Cleanup**: chat deletion cleanup | `attachments_tests.rs::chat_deletion_cleans_provider_resources`, BB `test_attachments.py` |
| Thread summary generation / retry / invalidation | `summary_tests.rs::{long_chat_gets_summarized_and_summary_is_used (incl. `stream_started.thread_summary_applied`), empty_summary_is_retried_then_dead_lettered}`, `mutations_tests.rs::mutation_invalidates_covering_summary`, `domain/workers/thread_summary_tests.rs`, BB `test_thread_summary.py` |
| **Authorization** owner scoping, consistent rejection | `authz_tests.rs::{foreign_users_and_tenants_get_404_everywhere, pdp_denial_is_403_and_failure_is_503_on_every_route, pdp_receives_the_documented_actions}`, `chats_tests.rs::pdp_denial_and_failure` |

Supporting suites: `config_tests.rs` (Appendix B config), `infra/oagw_provision_tests.rs`
(OAGW upstreams/routes), `infra/plugins/static_model_policy_tests.rs` (bundled
policy plugin), `mini-chat-sdk/src/models_tests.rs` (SDK types).
