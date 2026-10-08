# Acceptance coverage — mini-chat E2E suite

This file maps every item of `gears/mini-chat/docs/acceptance-criteria.md` to the tests that
cover it. Each test also has a `# Acceptance: ...` comment naming its item. Test ids are
`file::function`.

## Principles & Constraints

| Acceptance item | Tests |
|---|---|
| Tenant and owner isolation enforced on every resource | `test_authorization.py::test_foreign_access_is_404`, `test_authorization.py::test_list_scoped_to_owner`, `test_authorization.py::test_foreign_attachment_rejected`, `test_authorization.py::test_quota_isolated`, `test_attachments.py::test_get_attachment_scoping`, `test_lifecycle.py::test_unknown_turn`, `test_errors.py::test_404_masking` |
| Context window budget enforced, for both the input message and the full assembled request | `test_context.py::test_input_too_long`, `test_context.py::test_context_budget_exceeded`, `test_context.py::test_budget_truncation_drops_oldest_turns`, `test_mutations.py::test_edit_input_too_long`, `test_mutations.py::test_edit_context_budget_exceeded` |
| Streaming responses are never buffered before relaying | `test_streaming.py::test_no_buffering` |
| A chat's model is immutable once set | `test_chats.py::test_model_immutable_across_turns_retry_and_rename`, `test_chats.py::test_patch_rename_keeps_model`, `test_quota.py::test_premium_exhausted_downgrades` |
| Quota is checked before any outbound provider call | `test_quota.py::test_all_tiers_exhausted`, `test_quota.py::test_monthly_exhaustion_blocks`, `test_quota.py::test_web_search_daily_quota`, `test_mutations.py::test_retry_rejected_by_quota_keeps_previous_turn` |

## Chat CRUD

| Acceptance item | Tests |
|---|---|
| Create / get / list / update / delete lifecycle for chats, including model and title validation | `test_chats.py::test_create_chat_default_model`, `test_chats.py::test_create_chat_with_title_and_model`, `test_chats.py::test_create_chat_null_title`, `test_chats.py::test_create_chat_invalid_title`, `test_chats.py::test_create_chat_title_boundaries`, `test_chats.py::test_create_chat_invalid_model`, `test_chats.py::test_create_chat_title_checked_before_model`, `test_chats.py::test_create_chat_body_errors`, `test_chats.py::test_get_chat`, `test_chats.py::test_get_unknown_chat`, `test_chats.py::test_patch_rename_keeps_model`, `test_chats.py::test_patch_invalid`, `test_chats.py::test_delete_chat_twice`, `test_chats.py::test_deleted_chat_not_listed`, `test_chats.py::test_message_count_after_turn` |
| List endpoint supports filtering, ordering, and pagination, with validation of malformed input | `test_chat_list.py::test_list_default_order_and_shape`, `test_chat_list.py::test_list_pagination_with_cursor`, `test_chat_list.py::test_list_limit_clamp_and_zero`, `test_chat_list.py::test_list_filter_and_orderby`, `test_chat_list.py::test_list_bad_odata`, `test_chat_list.py::test_list_unknown_filter_field_reason`, `test_chat_list.py::test_list_bad_cursor_reason`, `test_chat_list.py::test_list_select_ignored` |
| Chat ordering reflects most recent activity | `test_chat_list.py::test_list_ordering_reflects_activity`, `test_streaming.py::test_send_bumps_chat_updated_at`, `test_mutations.py::test_retry_latest` |

## Messages API

| Acceptance item | Tests |
|---|---|
| List messages with filtering, ordering, and pagination | `test_messages.py::test_list_messages_chronological`, `test_messages.py::test_filter_messages`, `test_messages.py::test_orderby_desc`, `test_messages.py::test_messages_pagination`, `test_messages.py::test_messages_bad_query`, `test_messages.py::test_messages_unknown_chat` |
| Message response contract: identity, attachments, and reaction fields are always consistently present | `test_messages.py::test_message_contract_fields`, `test_messages.py::test_token_fields_omitted_when_zero`, `test_attachments.py::test_image_in_provider_request`, `test_reactions.py::test_set_and_change_reaction` |
| Message count and chronological ordering are tracked correctly across turns | `test_messages.py::test_message_count_two_turns_then_delete`, `test_messages.py::test_list_messages_chronological`, `test_mutations.py::test_retry_latest`, `test_mutations.py::test_delete_latest` |

## Streaming: Send Message

| Acceptance item | Tests |
|---|---|
| Send-message endpoint streams a response, correlated by a request id | `test_streaming.py::test_normal_turn_event_sequence`, `test_streaming.py::test_request_id_generated_and_client_supplied`, `test_streaming.py::test_provider_request_via_oagw`, `test_messages.py::test_message_contract_fields` |
| Preflight validation (content, attachments, limits) runs before any provider call | `test_streaming.py::test_empty_content_rejected_before_provider`, `test_streaming.py::test_stream_bad_targets`, `test_attachments.py::test_not_ready_attachment_rejected_before_provider`, `test_attachments.py::test_invalid_attachment_ids`, `test_attachments.py::test_image_guards`, `test_context.py::test_input_too_long`, `test_context.py::test_context_budget_exceeded`, `test_web_search.py::test_web_search_kill_switch` |
| Assistant message and usage are persisted once a stream completes | `test_streaming.py::test_persistence_on_completion`, `test_lifecycle.py::test_completed_turn_status` |

## SSE Event Contract

| Acceptance item | Tests |
|---|---|
| Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering | `test_streaming.py::test_normal_turn_event_sequence`, `test_sse_contract.py::test_web_search_tool_and_citations`, `test_sse_contract.py::test_code_interpreter_tool_events`, `test_sse_contract.py::test_ping_before_first_delta`, `test_sse_contract.py::test_incomplete_is_done`, `test_sse_contract.py::test_empty_completion`, `test_sse_contract.py::test_unmapped_file_citation_dropped`, `test_sse_contract.py::test_stream_closes_after_terminal`, `test_attachments.py::test_file_search_tool_and_file_citation`, `test_web_search.py::test_web_search_reported_cited_accounted` |
| Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers | `test_sse_contract.py::test_done_payload`, `test_quota.py::test_premium_exhausted_downgrades`, `test_quota.py::test_force_standard_tier_downgrade`, `test_attachments.py::test_file_search_tool_and_file_citation` |
| Error event is terminal and carries a sanitized message | `test_sse_contract.py::test_provider_failed_error_event`, `test_sse_contract.py::test_provider_error_event`, `test_sse_contract.py::test_provider_http_500`, `test_sse_contract.py::test_provider_http_429`, `test_web_search.py::test_web_search_per_turn_limit` |

## Idempotency & Replay

| Acceptance item | Tests |
|---|---|
| Replaying a known request id returns the stored result without side effects (no provider call, no quota change) | `test_idempotency.py::test_replay_completed_turn_has_no_side_effects`, `test_idempotency.py::test_replay_ignores_new_content`, `test_idempotency.py::test_same_request_id_other_chat_is_new_turn`, `test_quota.py::test_premium_exhausted_downgrades` (replay of a downgraded turn) |
| Conflicting reuse of a request id across turn states is rejected consistently | `test_idempotency.py::test_request_id_conflict_failed`, `test_idempotency.py::test_request_id_conflict_running`, `test_idempotency.py::test_request_id_conflict_cancelled`, `test_idempotency.py::test_request_id_conflict_deleted`, `test_mutations.py::test_retry_latest` |
| Replay is checked before the parallel-turn guard | `test_idempotency.py::test_replay_served_while_other_turn_runs`, `test_idempotency.py::test_request_id_conflict_running` |

## Parallel Turn Enforcement

| Acceptance item | Tests |
|---|---|
| Only one turn may run per chat at a time | `test_parallel_turns.py::test_second_turn_rejected_while_running`, `test_parallel_turns.py::test_racing_sends`, `test_parallel_turns.py::test_other_chat_not_blocked` |
| A new turn is accepted once the previous one reaches a terminal state | `test_parallel_turns.py::test_new_turn_after_terminal` |

## Turn Mutations

| Acceptance item | Tests |
|---|---|
| Retry / edit / delete act only on the latest, terminal turn | `test_mutations.py::test_retry_latest`, `test_mutations.py::test_edit_latest`, `test_mutations.py::test_edit_empty_content`, `test_mutations.py::test_delete_latest`, `test_mutations.py::test_mutation_of_non_latest_turn`, `test_mutations.py::test_mutation_of_running_turn`, `test_mutations.py::test_retry_failed_and_cancelled`, `test_mutations.py::test_mutation_not_found` |
| A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id | `test_mutations.py::test_retry_latest`, `test_mutations.py::test_retry_rejected_by_quota_keeps_previous_turn`, `test_mutations.py::test_edit_input_too_long`, `test_mutations.py::test_edit_context_budget_exceeded`, `test_mutations.py::test_mutation_audit_enqueued`, `test_context.py::test_retry_assembles_same_context` |
| Concurrent mutations resolve deterministically | `test_mutations.py::test_concurrent_retries` |
| Mutated turns correctly carry forward attachment and tool-usage history | `test_mutations.py::test_attachments_carried_forward`, `test_mutations.py::test_retry_resends_images`, `test_mutations.py::test_retry_reuses_web_search_flag` |

## Turn Lifecycle

| Acceptance item | Tests |
|---|---|
| Turn state machine (running → completed / cancelled / failed) is consistent end to end | `test_lifecycle.py::test_completed_turn_status`, `test_lifecycle.py::test_running_turn_status`, `test_lifecycle.py::test_failed_turn_status`, `test_lifecycle.py::test_terminal_state_is_immutable`, `test_lifecycle.py::test_unknown_turn`, `test_lifecycle.py::test_orphan_watchdog_finalizes_stale_turn`, `test_errors.py::test_rest_vs_stream_consistency` |
| Partial and null-content cases on cancellation or failure are handled correctly | `test_lifecycle.py::test_cancelled_with_partial_content`, `test_lifecycle.py::test_cancelled_without_content`, `test_lifecycle.py::test_failed_turn_status` |

## Attachments

| Acceptance item | Tests |
|---|---|
| Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation | `test_attachments.py::test_upload_pdf_ready`, `test_attachments.py::test_single_vector_store_per_chat`, `test_attachments.py::test_upload_image_with_thumbnail`, `test_attachments.py::test_upload_xlsx_ready_not_indexed`, `test_attachments.py::test_upload_xlsx_without_code_interpreter`, `test_attachments.py::test_upload_type_validation`, `test_attachments.py::test_upload_multipart_errors`, `test_attachments.py::test_upload_filename_rules`, `test_attachments.py::test_upload_too_large`, `test_attachments.py::test_document_count_limit`, `test_attachments.py::test_storage_limit`, `test_attachments.py::test_upload_unknown_chat`, `test_attachments.py::test_delete_attachment`, `test_attachments.py::test_delete_referenced_attachment_locked`, `test_attachments.py::test_get_attachment_scoping`, `test_attachments.py::test_kill_switch_images_and_code_interpreter` |
| Asynchronous indexing lifecycle, including provider failure and timeout handling | `test_attachments.py::test_provider_upload_failure`, `test_attachments.py::test_indexing_failed`, `test_attachments.py::test_indexing_in_progress_then_ready`, `test_attachments.py::test_not_ready_attachment_rejected_before_provider` |
| Attachments are correctly made available to the relevant provider tools | `test_attachments.py::test_image_in_provider_request`, `test_attachments.py::test_file_search_tool_and_file_citation`, `test_attachments.py::test_code_interpreter_tool_in_request`, `test_sse_contract.py::test_code_interpreter_tool_events` |
| Cleanup and abandoned-upload recovery behave correctly under failure | `test_attachments.py::test_delete_attachment`, `test_attachments.py::test_upload_reaper_marks_abandoned`, `test_attachments.py::test_attachment_cleanup_retries`, `test_attachments.py::test_indexing_failed` |

## Models API

| Acceptance item | Tests |
|---|---|
| Read-only model list/get reflects only enabled catalog entries, without exposing internal fields | `test_models.py::test_list_models`, `test_models.py::test_model_values`, `test_models.py::test_get_hidden_model`, `test_models.py::test_get_each_listed_model`, `test_models.py::test_models_require_auth` |

## Reactions API

| Acceptance item | Tests |
|---|---|
| Set/remove reaction on assistant messages only, idempotently | `test_reactions.py::test_set_and_change_reaction`, `test_reactions.py::test_remove_reaction`, `test_reactions.py::test_invalid_reaction`, `test_reactions.py::test_reaction_on_user_message`, `test_reactions.py::test_reaction_not_found`, `test_authorization.py::test_reactions_are_per_user` |

## Quota Status API

| Acceptance item | Tests |
|---|---|
| Quota status reporting is accurate and consistent with actual usage | `test_quota.py::test_quota_status_fresh_user`, `test_quota.py::test_quota_status_tracks_usage`, `test_quota.py::test_quota_status_warning_and_exhausted`, `test_quota.py::test_reserve_before_execute` (used = spent + reserved while running) |

## Quota Enforcement

| Acceptance item | Tests |
|---|---|
| Reserve-before-execute quota flow enforced on every provider call | `test_quota.py::test_reserve_before_execute`, `test_quota.py::test_premium_reserve_on_both_buckets`, `test_quota.py::test_all_tiers_exhausted`, `test_mutations.py::test_retry_rejected_by_quota_keeps_previous_turn` |
| Tier downgrade applied when a higher tier is exhausted | `test_quota.py::test_premium_exhausted_downgrades`, `test_quota.py::test_force_standard_tier_downgrade` |
| Credits and tokens are accounted correctly per model and tier | `test_settlement.py::test_actual_settlement_standard`, `test_settlement.py::test_actual_settlement_premium`, `test_settlement.py::test_credit_rounding_per_component`, `test_quota.py::test_quota_status_tracks_usage`, `test_quota.py::test_premium_exhausted_downgrades` |

## Settlement & Finalization

| Acceptance item | Tests |
|---|---|
| Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate | `test_settlement.py::test_actual_settlement_standard`, `test_settlement.py::test_actual_settlement_premium`, `test_settlement.py::test_failed_turn_estimated_settlement`, `test_settlement.py::test_cancelled_turn_estimated_settlement`, `test_settlement.py::test_web_search_limit_estimated_settlement`, `test_settlement.py::test_rejected_request_does_not_settle`, `test_lifecycle.py::test_orphan_watchdog_finalizes_stale_turn` |
| Usage is published reliably and exactly once per turn | `test_settlement.py::test_usage_event_enqueued_once`, `test_settlement.py::test_cancelled_turn_estimated_settlement`, `test_streaming.py::test_persistence_on_completion`, `test_idempotency.py::test_one_usage_message_per_turn_after_replays`, `test_idempotency.py::test_replay_completed_turn_has_no_side_effects` |

## Context Assembly

| Acceptance item | Tests |
|---|---|
| System prompt, thread summary, and recent history are assembled and truncated deterministically within budget | `test_context.py::test_history_in_order`, `test_context.py::test_request_parameters`, `test_context.py::test_recent_messages_limit`, `test_context.py::test_budget_truncation_drops_oldest_turns`, `test_context.py::test_retry_assembles_same_context`, `test_thread_summary.py::test_summary_generated_and_applied`, `test_streaming.py::test_provider_request_identity_fields` |
| Tool availability and guidance are reflected correctly in the assembled request | `test_context.py::test_no_tools_by_default`, `test_context.py::test_web_search_tool_and_guard`, `test_attachments.py::test_file_search_tool_and_file_citation`, `test_attachments.py::test_code_interpreter_tool_in_request`, `test_quota.py::test_web_search_daily_quota` (unsupported model: no tool) |

## Error Mapping & Sanitization

| Acceptance item | Tests |
|---|---|
| All errors map to the canonical error contract, consistently across REST and streaming | `test_errors.py::test_problem_envelope`, `test_errors.py::test_invalid_path_params`, `test_errors.py::test_json_extractor_errors_on_stream`, `test_errors.py::test_unauthenticated`, `test_errors.py::test_rest_vs_stream_consistency`, `test_errors.py::test_404_masking`, `test_chats.py::test_create_chat_body_errors` (plus every `assert_problem` call in the suite) |
| Provider-originated error details are sanitized before reaching the client | `test_errors.py::test_sanitized_provider_errors`, `test_sse_contract.py::test_provider_failed_error_event`, `test_sse_contract.py::test_provider_error_event`, `test_sse_contract.py::test_provider_http_500`, `test_attachments.py::test_upload_pdf_ready` (no provider ids in bodies) |

## Web Search

| Acceptance item | Tests |
|---|---|
| Web search tool use is reported, cited, accounted, and quota-limited correctly | `test_web_search.py::test_web_search_reported_cited_accounted`, `test_web_search.py::test_web_search_not_requested`, `test_web_search.py::test_web_search_per_turn_limit`, `test_web_search.py::test_web_search_kill_switch`, `test_sse_contract.py::test_web_search_tool_and_citations`, `test_settlement.py::test_web_search_calls_accounted`, `test_settlement.py::test_web_search_limit_estimated_settlement`, `test_quota.py::test_web_search_daily_quota` |

## Cleanup & Recovery

| Acceptance item | Tests |
|---|---|
| Chat deletion triggers reliable background cleanup of provider-side resources | `test_cleanup.py::test_chat_deletion_cleans_provider_resources`, `test_cleanup.py::test_chat_deletion_cleanup_retries`, `test_cleanup.py::test_chat_deletion_keeps_running_turn`, `test_lifecycle.py::test_orphan_watchdog_finalizes_stale_turn` |
| Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly | `test_thread_summary.py::test_summary_generated_and_applied`, `test_thread_summary.py::test_summary_failure_is_retried`, `test_thread_summary.py::test_summary_invalidated_by_mutation` |

## Authorization

| Acceptance item | Tests |
|---|---|
| Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently | `test_authorization.py::test_foreign_access_is_404` (tenant B and same-tenant user, 14 operations each), `test_authorization.py::test_list_scoped_to_owner`, `test_authorization.py::test_foreign_attachment_rejected`, `test_authorization.py::test_quota_isolated`, `test_authorization.py::test_models_visible_to_other_tenant`, `test_authorization.py::test_unauthenticated_routes`, `test_attachments.py::test_upload_unknown_chat` |

## Suite self-tests (no server)

`test_mock_selftest.py` (mock provider) and `test_helpers_selftest.py` (SSE parser, grammar
validator, credit formulas, Problem assertions, DB helpers, client disconnect) are marked
`noserver` and run without the gear binary.

## Knowledge search (DESIGN `knowledge_search` config)

| Behaviour | Tests |
|---|---|
| `search_knowledge` tool + guard offered only when enabled; search via OAGW, result fed back, answer streamed | `test_knowledge_search.py::test_knowledge_search_loop`, `test_knowledge_search.py::test_knowledge_search_disabled_by_default` |
| Per-message call limit, iteration cap, degraded search failure | `test_knowledge_search.py::test_knowledge_search_call_limit`, `test_knowledge_search.py::test_knowledge_search_iteration_cap`, `test_knowledge_search.py::test_knowledge_search_failure_degrades` |
| Function call for a tool never offered → `unexpected_tool_use` | `test_knowledge_search.py::test_unexpected_tool_use` |
