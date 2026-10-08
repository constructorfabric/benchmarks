# Acceptance criteria → automated tests

Every item of `gears/mini-chat/docs/acceptance-criteria.md` and the tests that cover it. Run all of them
with `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk`. Test paths are module paths inside the
`mini_chat` lib (`src/...`). Router tests go through the real axum routes with an injected
`SecurityContext`, an in-memory SQLite database with the real migrations, the started outbox pipeline and
a scriptable OpenAI-compatible provider (`src/testing.rs`).

## Principles & Constraints

| Item | Tests |
|---|---|
| Tenant and owner isolation on every resource | `api::handlers::chats::tests::other_users_and_tenants_cannot_see_the_chat`, `domain::stream::tests::foreign_users_cannot_send`, `domain::turns::tests::retry_of_non_latest_or_foreign_turn_is_rejected`, `domain::stream::tests::turn_status_endpoint`, `api::handlers::attachments::tests::get_attachment_isolation`, `api::handlers::attachments::tests::delete_checks_uploader_and_existence`, `api::handlers::messages::tests::my_reaction_is_per_user_and_null_for_user_messages`, `domain::quota::tests::status::status_is_per_user` |
| Context window budget (message and assembled request) | `domain::stream::tests::input_too_long_is_rejected`, `domain::stream::tests::mandatory_context_over_budget_is_rejected`, `domain::context::tests::{effective_budget_uses_min_of_input_limit_and_window, surcharges_and_overhead_reduce_budget, oversized_mandatory_items_are_rejected, non_positive_budget_is_rejected, truncation_drops_oldest_whole_turns}` |
| Streaming responses never buffered | `domain::stream::tests::deltas_are_relayed_before_the_provider_finishes`, `domain::stream::tests::only_one_running_turn_per_chat` |
| A chat's model is immutable | `api::handlers::chats::tests::patch_renames_and_ignores_model`, `domain::stream::tests::premium_exhausted_downgrades_to_standard` (selected model unchanged), `domain::stream::tests::removed_chat_model_is_invalid_model` |
| Quota checked before any outbound provider call | `domain::stream::tests::quota_exhausted_rejects_with_429_before_provider`, `domain::turns::tests::preflight_rejection_keeps_previous_turn`, `domain::quota::tests::preflight::*` |

## Chat CRUD

| Item | Tests |
|---|---|
| Create / get / list / update / delete incl. model and title validation | `api::handlers::chats::tests::{create_uses_default_model_and_sets_location, create_default_falls_back_to_first_enabled_model, create_with_explicit_model_and_null_title, create_rejects_disabled_and_unknown_models, create_validates_title, create_title_checked_before_authorization_and_model, create_wrong_body_type_is_422, get_reports_non_deleted_message_count, get_unknown_or_bad_id, patch_renames_and_ignores_model, patch_title_validation, delete_soft_deletes_marks_attachments_and_enqueues_cleanup}`, `domain::chats::tests::title_validation_trims_and_bounds` |
| List filtering, ordering, pagination, malformed input | `api::handlers::chats::tests::{list_filters_by_title_id_and_time, list_paginates_every_chat_exactly_once, list_limit_clamped_and_zero_rejected, list_bad_odata_queries_are_400, list_items_carry_message_counts}`, `domain::chats::tests::{cursor_pagination_handles_equal_and_close_timestamps_on_sqlite, cursor_round_trips_timestamp_text, timestamp_filter_values_use_the_stored_text_format_on_sqlite, default_order_applies_only_without_order_and_cursor}` |
| Ordering reflects most recent activity | `api::handlers::chats::tests::list_orders_by_activity`, `domain::stream::tests::send_streams_events_in_order_and_persists_turn` (send bumps `updated_at`) |

## Messages API

| Item | Tests |
|---|---|
| List with filtering, ordering, pagination | `api::handlers::messages::tests::{chronological_order_filters_and_pagination, list_errors}` |
| Message contract (identity, attachments, reactions always present) | `api::handlers::messages::tests::{messages_contract_for_a_completed_turn, attachments_are_summarised_per_message, my_reaction_is_per_user_and_null_for_user_messages, zero_tokens_are_omitted_and_deleted_messages_excluded, null_request_id_fails_with_internal_error}`, `domain::stream::tests::image_is_sent_as_input_image_and_linked` |
| Message count and chronology across turns | `domain::stream::tests::history_is_included_in_order`, `api::handlers::chats::tests::get_reports_non_deleted_message_count`, `domain::turns::tests::delete_latest_turn` |

## Streaming: Send Message

| Item | Tests |
|---|---|
| Streams a response correlated by request id | `domain::stream::tests::{send_streams_events_in_order_and_persists_turn, client_request_id_of_any_version_is_used}` |
| Preflight validation before any provider call | `domain::stream::tests::{empty_content_is_rejected_before_provider, malformed_body_and_unknown_chat, invalid_attachment_ids_roll_back_everything, image_guards, input_too_long_is_rejected, web_search_kill_switch_rejects, removed_chat_model_is_invalid_model}` |
| Assistant message and usage persisted on completion | `domain::stream::tests::{send_streams_events_in_order_and_persists_turn, completed_turn_settles_quota_and_publishes_usage_once}` |

## SSE Event Contract

| Item | Tests |
|---|---|
| Full event contract and ordering (start, delta, tool, citations, done, error, ping) | `domain::stream::tests::{send_streams_events_in_order_and_persists_turn, web_search_tool_events_counts_and_citations, file_search_and_code_interpreter_tools_from_ready_attachments, ping_is_sent_before_first_content, provider_failure_is_terminal_sanitized_error}`, `infra::llm::responses::tests::*` (provider event translation) |
| Done exposes usage and quota/downgrade outcome without internal ids | `domain::stream::tests::{send_streams_events_in_order_and_persists_turn, premium_exhausted_downgrades_to_standard, sse_payloads_never_expose_provider_ids}` |
| Error event terminal with sanitized message | `domain::stream::tests::{provider_failure_is_terminal_sanitized_error, provider_http_429_maps_to_rate_limited, provider_http_500_maps_to_provider_error, transport_timeout_maps_to_provider_timeout, web_search_per_message_limit_fails_turn}`, `domain::sanitize::tests::*` |

## Idempotency & Replay

| Item | Tests |
|---|---|
| Replay returns stored result without side effects | `domain::stream::tests::replay_of_completed_turn_is_side_effect_free` |
| Conflicting reuse of a request id rejected consistently | `domain::stream::tests::request_id_of_failed_turn_conflicts`, `domain::stream::tests::only_one_running_turn_per_chat` (running), `domain::turns::tests::retry_latest_turn_regenerates_with_new_request_id` (soft-deleted) |
| Replay checked before the parallel-turn guard | `domain::stream::tests::only_one_running_turn_per_chat` (same request id while running → `request_id_conflict`), `domain::stream::setup::start_send` order |

## Parallel Turn Enforcement

| Item | Tests |
|---|---|
| One running turn per chat | `domain::stream::tests::only_one_running_turn_per_chat` |
| New turn accepted once the previous one is terminal | `domain::stream::tests::only_one_running_turn_per_chat` (final send) |

## Turn Mutations

| Item | Tests |
|---|---|
| Retry / edit / delete only on the latest terminal turn | `domain::turns::tests::{delete_latest_turn, retry_of_non_latest_or_foreign_turn_is_rejected, mutation_of_running_turn_is_failed_precondition}` |
| Full send pipeline and new request id | `domain::turns::tests::{retry_latest_turn_regenerates_with_new_request_id, edit_replaces_content, preflight_rejection_keeps_previous_turn, retry_with_images_respects_image_guards}` |
| Concurrent mutations resolve deterministically | `domain::turns::tests::concurrent_retries_resolve_deterministically` |
| Attachment and tool-usage history carried forward | `domain::turns::tests::retry_carries_attachments_and_web_search_forward` |

## Turn Lifecycle

| Item | Tests |
|---|---|
| running → completed / cancelled / failed consistently | `domain::stream::tests::{send_streams_events_in_order_and_persists_turn, provider_failure_is_terminal_sanitized_error, only_one_running_turn_per_chat, turn_status_endpoint}`, `infra::workers::orphan_watchdog::tests::*` |
| Partial and null content on cancellation / failure | `domain::stream::tests::{only_one_running_turn_per_chat (partial persisted), cancel_before_content_persists_no_message, provider_failure_is_terminal_sanitized_error (no assistant message)}` |

## Attachments

| Item | Tests |
|---|---|
| Upload / get / delete with size, type, per-chat limits | `api::handlers::attachments::tests::{document_upload_is_ready_and_indexed, image_upload_gets_a_webp_thumbnail, xlsx_is_routed_to_code_interpreter, xlsx_rejected_when_code_interpreter_unavailable, image_rejected_when_images_disabled, unsupported_and_inferred_content_types, multipart_errors, file_too_large_uses_the_kind_limit, model_file_size_limit_applies, per_chat_document_and_storage_limits, storage_limit_counts_non_failed_rows, removed_chat_model_is_invalid_model, foreign_unknown_or_deleted_chat_is_404, get_attachment_isolation, delete_is_idempotent_and_cleans_up_once, delete_of_referenced_attachment_is_locked, delete_checks_uploader_and_existence, filename_defaults_and_truncation, csv_*}`, `domain::attachments::unit_tests::*` |
| Asynchronous indexing incl. provider failure and timeout | `api::handlers::attachments::tests::{indexing_still_running_finishes_in_background, indexing_failure_within_deadline_is_503, background_indexing_failure_hands_file_to_cleanup, background_indexing_times_out, background_task_stops_for_deleted_attachment, provider_upload_failure_marks_row_failed, concurrency_limit_is_503}` |
| Attachments available to provider tools | `domain::stream::tests::{file_search_and_code_interpreter_tools_from_ready_attachments, image_is_sent_as_input_image_and_linked, no_tools_without_attachments_or_web_search}`, `api::handlers::attachments::tests::{existing_store_is_reused, second_document_reuses_the_chat_vector_store, vector_store_of_another_backend_is_provider_mismatch, stale_placeholder_is_reclaimed}` |
| Cleanup and abandoned-upload recovery | `infra::outbox::handlers::attachment_cleanup::tests::*`, `infra::workers::upload_reaper::tests::*` |

## Models API

| Item | Tests |
|---|---|
| Only enabled entries, no internal fields | `api::handlers::models::tests::*` |

## Reactions API

| Item | Tests |
|---|---|
| Set / remove on assistant messages only, idempotent | `api::handlers::messages::tests::{put_reaction_upserts_one_row, delete_reaction_is_idempotent, reaction_on_user_message_is_a_failed_precondition, invalid_reaction_value_is_checked_before_authorization, reaction_not_found_cases}` |

## Quota Status API

| Item | Tests |
|---|---|
| Accurate and consistent with usage | `domain::quota::tests::status::*`, `domain::stream::tests::completed_turn_settles_quota_and_publishes_usage_once` |

## Quota Enforcement

| Item | Tests |
|---|---|
| Reserve-before-execute on every provider call | `domain::quota::tests::preflight::{reserve_books_total_and_premium_rows, reserve_standard_books_total_only, reserve_recheck_rolls_back_second_reserve, reserve_recheck_against_changed_limits}`, `domain::stream::tests::invalid_attachment_ids_roll_back_everything` |
| Tier downgrade when a higher tier is exhausted | `domain::quota::tests::preflight::{premium_daily_exhausted_downgrades_to_standard, premium_monthly_exhausted_downgrades, kill_switches_skip_premium_with_reason, disabled_selected_model_downgrades_with_model_disabled, missing_selected_model_starts_at_premium, all_tiers_exhausted_rejects_with_tokens, standard_never_upgrades_when_total_exhausted}`, `domain::stream::tests::premium_exhausted_downgrades_to_standard` |
| Credits and tokens per model and tier | `domain::quota::tests::arith::*`, `domain::quota::tests::settle::{actual_standard_settlement_updates_total_rows, actual_premium_settlement_updates_both_buckets_telemetry_on_total_only}`, `domain::stream::tests::{completed_turn_settles_quota_and_publishes_usage_once, premium_turn_charges_both_buckets}` |

## Settlement & Finalization

| Item | Tests |
|---|---|
| Every terminal outcome settles exactly once (actual / estimated) | `domain::quota::tests::settle::*`, `domain::quota::tests::arith::billing_derivation_table`, `domain::stream::tests::{completed_turn_settles_quota_and_publishes_usage_once, provider_failure_is_terminal_sanitized_error, only_one_running_turn_per_chat}`, `infra::workers::orphan_watchdog::tests::*` |
| Usage published reliably and once per turn | `domain::stream::tests::{completed_turn_settles_quota_and_publishes_usage_once, replay_of_completed_turn_is_side_effect_free}`, `infra::outbox::handlers::usage::tests::usage_handler_outcomes`, `domain::quota::tests::settle::{dedupe_key_is_simple_hex, enqueue_usage_reaches_policy_plugin}` |

## Context Assembly

| Item | Tests |
|---|---|
| System prompt, summary and history assembled and truncated deterministically | `domain::context::tests::*`, `domain::stream::tests::{history_is_included_in_order, thread_summary_applied_is_reported}` |
| Tool availability and guidance in the request | `domain::stream::tests::{web_search_tool_events_counts_and_citations, file_search_and_code_interpreter_tools_from_ready_attachments, no_tools_without_attachments_or_web_search}`, `domain::context::tests::instructions_are_system_prompt_plus_guards`, `infra::llm::responses::tests::responses_body_*` |

## Error Mapping & Sanitization

| Item | Tests |
|---|---|
| Canonical error contract across REST and streaming | `api::handlers::chats::tests::authz_denied_is_403_and_pdp_outage_is_503`, `domain::stream::tests::authz_failures_map_to_403_and_503`, error-reason assertions throughout the router tests, `infra::llm::responses::tests::http_*` |
| Provider error details sanitized | `domain::sanitize::tests::*`, `domain::stream::tests::{provider_failure_is_terminal_sanitized_error, provider_http_500_maps_to_provider_error}` |

## Web Search

| Item | Tests |
|---|---|
| Reported, cited, accounted, quota-limited | `domain::stream::tests::{web_search_tool_events_counts_and_citations, web_search_per_message_limit_fails_turn, web_search_daily_quota_is_checked_only_when_enabled, web_search_kill_switch_rejects}`, `domain::quota::tests::preflight::{daily_web_search_quota_only_when_tool_sent, web_search_kill_switch_rejects_before_cascade}` |

## Cleanup & Recovery

| Item | Tests |
|---|---|
| Chat deletion cleans provider resources | `infra::outbox::handlers::attachment_cleanup::tests::{chat_deletion_cleans_files_and_vector_store_through_the_pipeline, chat_cleanup_waits_for_pending_files_before_the_vector_store, chat_cleanup_continues_after_terminal_file_failure, vector_store_delete_failure_retries_then_rejects_at_max_attempts, chat_cleanup_rejects_live_chats}`, `api::handlers::chats::tests::delete_soft_deletes_marks_attachments_and_enqueues_cleanup` |
| Thread summary generation, failure/retry, mutation invalidation | `domain::summary::tests::*`, `infra::outbox::handlers::thread_summary::tests::*` |

## Authorization

| Item | Tests |
|---|---|
| Scoped to owner; foreign access rejected consistently | isolation tests above, plus `api::handlers::models::tests::models_authorization_errors`, `domain::quota::tests::status::status_denied_and_pdp_down` |

## Live-server smoke checks

Besides the unit/integration tests, the example server built with
`--features mini-chat,static-authn,static-authz,single-tenant,static-credstore` was run against a mock
OpenAI-compatible provider (Responses streaming, Files, Vector Stores) through OAGW, with static-token
users in two tenants: chat CRUD + pagination, streaming + replay, isolation, reactions, quota status,
seeded-quota downgrade and 429, provider failure sanitization, document upload + `file_search`, retry and
delete of turns, chat deletion cleanup (provider file + vector store deletes), thread summary generation on
a small-context model, concurrent sends and deletes, and the served OpenAPI (all 19 operations).
