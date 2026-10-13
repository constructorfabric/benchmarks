# Mini Chat: acceptance-criteria traceability

This document maps every line of
[`gears/mini-chat/docs/acceptance-criteria.md`](../../../gears/mini-chat/docs/acceptance-criteria.md)
to the modules that implement it and to the tests that check it.

Conventions:

- **Modules** are paths under `gears/mini-chat/mini-chat/src/`. The SDK is `gears/mini-chat/mini-chat-sdk/src/`.
- **Unit** tests are given by their full test path inside the crate. The crate is `mini_chat` unless the name starts with `mini_chat_sdk::`. Run them with `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk <name>`.
- **E2E** tests are given as `file::test`. Files are under `testing/e2e/suites/mini_chat_dev/`. Run them with `cd testing/e2e && python3 -m pytest suites/mini_chat_dev`, after building the server (see the suite README).
- Items marked **(T19)** are tests added during the Task 19 sweep to close a gap. Gaps and fixes are listed at the end.

---

## Principles & Constraints

### Tenant and owner isolation enforced on every resource
- Modules:
  - `domain/authz.rs`: `PolicyEnforcerAuthz`. Every chat, quota and model check goes through the PEP. Chat scopes get `ensure_owner` as defence in depth.
  - `domain/ports.rs`: `AuthzPort`, `ChatAction`.
  - `infra/db/repos/*.rs`: Secure ORM scoped queries.
  - `domain/services/*`: each service evaluates its `ChatAction` before touching a child resource.
- Unit:
  - `domain::authz::authz_tests::chat_scope_contains_tenant_and_owner_predicates`
  - `domain::authz::authz_tests::chat_scope_does_not_cover_another_owner`
  - `domain::authz::authz_tests::every_chat_action_is_sent_by_name_and_owner_scoped`
  - `domain::authz::authz_tests::quota_scope_uses_the_user_quota_type_and_owner_filter`
  - `domain::services::chat_service::chat_service_tests::get_foreign_owner_is_not_found`
  - `domain::services::message_service::message_service_tests::messages_list_hides_deleted_messages_and_foreign_chats`
  - `domain::services::reaction_service::reaction_service_tests::reaction_on_missing_deleted_or_foreign_message_is_not_found`
  - `domain::services::attachment_service::attachment_service_tests::upload_to_unknown_or_foreign_chat_is_404`
  - `domain::services::attachment_service::attachment_service_tests::get_other_uploader_404`
  - `domain::services::attachment_service::attachment_service_tests::delete_other_uploader_or_unknown_is_404`
  - `domain::services::turn_service::turn_service_tests::turn_status_never_reads_another_chats_turn`
  - `domain::services::stream::stream_service_tests::foreign_or_not_ready_attachment_rolls_back_reserve`
  - `domain::services::stream::stream_service_tests::mutation_tests::unknown_turn_and_foreign_chat_are_404`
  - `domain::services::quota_service::quota_service_tests::tool_quota_reads_only_the_callers_rows`
  - `infra::db::repos::repos_tests::find_scoped_hides_deleted_and_out_of_scope_chats`
  - `infra::db::migrations_tests::message_attachment_composite_fk_rejects_cross_chat`
- E2E:
  - `test_authorization.py::test_foreign_caller_gets_404_on_every_chat_endpoint` (T19; covers other tenant and same-tenant other user)
  - `test_authorization.py::test_foreign_attachment_id_cannot_be_referenced_from_own_chat` (T19)
  - `test_authorization.py::test_quota_status_of_other_tenant_is_unaffected_by_usage` (T19)
  - `test_chats.py::test_other_tenant_or_user_gets_404`
  - `test_quota_status.py::test_used_credits_reflect_seeded_usage`

### Context window budget enforced, for both the input message and the full assembled request
- Modules:
  - `domain/estimation.rs`: `estimate_text_tokens`, `effective_input_budget`, `token_budget`.
  - `domain/context.rs`: `assemble_context`, which checks the mandatory items.
  - `domain/services/stream/setup.rs`: `check_effective_model` raises `INPUT_TOO_LONG`; `prepare_request` builds the budget on the effective model.
  - `api/rest/error.rs`: maps both errors to 400 `out_of_range`.
- Unit:
  - `domain::services::stream::stream_service_tests::input_too_long`
  - `domain::services::stream::stream_service_tests::context_budget_exceeded_400`
  - `domain::services::stream::stream_service_tests::mutation_tests::edit_input_too_long_rejected_before_mutation`
  - `domain::estimation::estimation_tests::token_budget_rejects_output_filling_the_window`
  - `domain::estimation::estimation_tests::token_budget_rejects_deductions_reaching_the_limit`
  - `domain::estimation::estimation_tests::token_budget_subtracts_surcharges_and_fixed_overhead`
  - `domain::estimation::estimation_tests::effective_input_budget_caps_by_max_input_when_set`
  - `domain::context::context_tests::mandatory_over_budget_rejects`
  - `api::rest::error::error_tests::out_of_range_errors`
- E2E:
  - `test_principles.py::test_context_budget_is_enforced_for_message_and_assembled_request` (T19; `INPUT_TOO_LONG` and `CONTEXT_BUDGET_EXCEEDED`, both with no provider call, no turn and no reserve)
  - `test_principles.py::test_message_just_within_the_budget_is_sent` (T19)
  - `test_context.py::test_long_message_on_small_model_is_input_too_long`

### Streaming responses are never buffered before relaying
- Modules:
  - `infra/llm/sse.rs`: incremental frame parser.
  - `infra/llm/providers/openai_responses.rs`: one `LlmEvent` per frame.
  - `domain/services/stream/provider_task.rs` and `domain/services/stream/relay.rs`: forward each event on a bounded channel, sized by `streaming.sse_channel_capacity`.
  - `api/rest/sse.rs` and `api/rest/handlers/stream.rs`: write each event to the response as it arrives.
- Unit:
  - `domain::services::stream::stream_service_tests::disconnect_cancels_and_settles_estimated` (the delta is received while the provider stream is still open)
  - `domain::services::stream::stream_service_tests::tool_done_updates_turn_counts_while_running`
  - `infra::llm::sse::sse_tests::parses_split_frames`
  - `infra::llm::providers::openai_responses::openai_responses_tests::cancellation_ends_stream_and_drops_body`
  - `config::config_tests::channel_capacity_range`
- E2E:
  - `test_principles.py::test_stream_is_relayed_without_buffering` (T19). The mock sends events 500 ms apart. The first delta must reach the client at least 2.4 gaps (3 gaps with a 20 % tolerance) before `done`, and each delta must arrive about one gap after the previous one.
  - `test_sse_contract.py::test_pings_are_sent_while_the_model_is_silent`

### A chat's model is immutable once set
- Modules:
  - `domain/services/chat_service.rs`: `update_title` writes only the title.
  - `api/rest/dto.rs`: `UpdateChatReq` has only `title`.
  - `domain/services/quota_service.rs`: the effective model is decided per turn and recorded on the message, never on the chat.
- Unit:
  - `domain::services::chat_service::chat_service_tests::update_title_ignores_other_fields_and_bumps_updated_at`
  - `domain::services::stream::stream_service_tests::downgrade_is_reported_in_done`
- E2E:
  - `test_principles.py::test_chat_model_is_immutable` (T19). PATCH with `model` is ignored, and PATCH with `model` alone is 422. `done.selected_model` and `effective_model` are both the chat model, and so is the provider `model`.
  - `test_principles.py::test_downgrade_changes_the_effective_model_only` (T19)
  - `test_chats.py::test_crud_round_trip`

### Quota is checked before any outbound provider call
- Modules:
  - `domain/services/quota_service.rs`: `preflight` and `reserve_in_tx`.
  - `domain/services/stream/mod.rs`: preflight and reserve run before `provider_task` is spawned.
  - `domain/services/turn_service.rs`: mutations go through the same pipeline.
- Unit:
  - `domain::services::stream::stream_service_tests::quota_exhausted_429_no_provider_call`
  - `domain::services::stream::stream_service_tests::mutation_tests::quota_rejection_leaves_previous_turn_intact`
  - `domain::services::quota_service::quota_service_tests::reject_when_all_tiers_exhausted_tokens_scope`
  - `domain::services::quota_service::quota_service_tests::web_search_kill_switch_before_cascade`
- E2E:
  - `test_principles.py::test_exhausted_quota_is_429_before_any_provider_call` (T19). The mock receives no `/responses` call, and no turn or reserve is created.
  - `test_turn_mutations.py::test_retry_quota_rejection_keeps_the_previous_turn`
  - `test_web_search.py::test_daily_web_search_quota_is_429`

## Chat CRUD

### Create / get / list / update / delete lifecycle for chats, including model and title validation
- Modules:
  - `api/rest/handlers/chats.rs`, `api/rest/routes.rs`, `api/rest/dto.rs`
  - `domain/services/chat_service.rs`, `domain/services/model_service.rs`
  - `infra/db/repos/chat_repo.rs`
- Unit:
  - `domain::services::chat_service::chat_service_tests::create_defaults_model_and_trims_title`
  - `domain::services::chat_service::chat_service_tests::create_rejects_blank_and_long_title_before_model_lookup`
  - `domain::services::chat_service::chat_service_tests::create_rejects_disabled_model`
  - `domain::services::chat_service::chat_service_tests::create_without_enabled_model_is_invalid_model`
  - `domain::services::chat_service::chat_service_tests::delete_twice_is_not_found`
  - `domain::services::chat_service::chat_service_tests::delete_sets_attachment_cleanup_pending_and_enqueues_chat_cleanup`
  - `domain::services::chat_service::chat_service_tests::chat_dto_omits_absent_title`
  - `mini_chat_sdk::models::models_tests::default_model_prefers_is_default_then_first_enabled`
- E2E:
  - `test_chats.py::test_crud_round_trip`
  - `test_chats.py::test_untitled_chat_omits_title_and_explicit_model_is_kept`
  - `test_chats.py::test_invalid_title_is_400`
  - `test_chats.py::test_invalid_model_is_400`
  - `test_chats.py::test_patch_without_title_is_422`
  - `test_chats.py::test_json_body_without_json_content_type_is_415`
  - `test_chats.py::test_malformed_json_is_400`
  - `test_chats.py::test_non_uuid_path_is_400`

### List endpoint supports filtering, ordering, and pagination, with validation of malformed input
- Modules: `domain/services/chat_service.rs` (`list`, OData), `infra/db/repos/chat_repo.rs`
- Unit:
  - `domain::services::chat_service::chat_service_tests::list_orders_by_updated_at_desc_and_paginates`
  - `domain::services::chat_service::chat_service_tests::list_default_page_is_twenty_and_ties_break_by_id_desc`
  - `domain::services::chat_service::chat_service_tests::list_filter_contains_title`
  - `domain::services::chat_service::chat_service_tests::list_limit_over_100_clamped`
  - `domain::services::chat_service::chat_service_tests::list_unknown_field_is_invalid_filter`
  - `domain::services::chat_service::chat_service_tests::list_orderby_title_asc_with_untitled_chats`
  - `domain::services::chat_service::chat_service_tests::default_order_pages_every_chat_once_including_untitled`
  - `api::rest::error::error_tests::odata_query_errors_keep_the_odata_resource_type`
- E2E: `test_chats.py::test_list_pagination_filter_and_limits`, `test_chats.py::test_bad_list_query_is_400_odata`

### Chat ordering reflects most recent activity
- Modules:
  - `domain/services/chat_service.rs`: rename bumps `updated_at`.
  - `domain/services/stream/mod.rs` and `domain/services/turn_service.rs`: `touch_updated_at` on send and on mutation.
  - `infra/db/repos/chat_repo.rs`
- Unit:
  - `domain::services::chat_service::chat_service_tests::rename_moves_chat_to_the_top_of_the_list`
  - `domain::services::stream::stream_service_tests::chat_updated_at_bumped_on_send`
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_copies_web_search_flag_and_bumps_chat`
  - `infra::db::repos::repos_tests::touch_updated_at_sets_the_timestamp_within_the_tenant`
- E2E:
  - `test_chats.py::test_ordering_after_rename`
  - `test_stream.py::test_chat_updated_at_is_bumped`
  - `test_stream.py::test_send_moves_the_chat_to_the_top_of_the_list` (T19)

## Messages API

### List messages with filtering, ordering, and pagination
- Modules: `api/rest/handlers/messages.rs`, `domain/services/message_service.rs`, `infra/db/repos/message_repo.rs`
- Unit:
  - `domain::services::message_service::message_service_tests::messages_filter_by_role`
  - `domain::services::message_service::message_service_tests::messages_list_chronological_same_second`
  - `domain::services::message_service::message_service_tests::messages_list_default_limit_is_twenty_and_ties_break_by_id_asc`
- E2E:
  - `test_messages.py::test_empty_list_shape`
  - `test_messages.py::test_select_is_accepted_and_ignored`
  - `test_messages.py::test_bad_query_is_400`
  - `test_messages.py::test_messages_are_chronological_and_paginate_within_one_second`

### Message response contract: identity, attachments, and reaction fields are always consistently present
- Modules: `api/rest/dto.rs` (`MiniChatMessageDto`), `domain/services/message_service.rs`
- Unit:
  - `domain::services::message_service::message_service_tests::message_dto_always_has_attachments_and_my_reaction`
  - `domain::services::message_service::message_service_tests::my_reaction_is_null_for_non_assistant_messages`
  - `domain::services::message_service::message_service_tests::message_without_request_id_is_internal`
  - `infra::db::repos::repos_tests::unknown_stored_reaction_is_internal`
- E2E:
  - `test_messages.py::test_message_dto_fields`
  - `test_attachments.py::test_message_with_document_uses_file_search` (attachment summary in the list)
  - `test_reactions.py::test_set_replace_list_and_remove` (`my_reaction`)

### Message count and chronological ordering are tracked correctly across turns
- Modules: `infra/db/repos/chat_repo.rs` (live-message count), `infra/db/repos/message_repo.rs`
- Unit:
  - `domain::services::chat_service::chat_service_tests::message_count_counts_live_messages_only`
  - `domain::services::stream::stream_service_tests::new_turn_allowed_after_terminal_and_history_is_sent`
  - `infra::db::migrations_tests::db_ts_sorts_lexicographically`
  - `domain::time::tests::db_ts_forces_micros_plus_one_nanosecond`
- E2E:
  - `test_stream.py::test_second_turn_sends_history` (`message_count` 4, chronological roles)
  - `test_messages.py::test_messages_are_chronological_and_paginate_within_one_second`
  - `test_turn_mutations.py::test_delete_soft_deletes_and_status_404`

## Streaming: Send Message

### Send-message endpoint streams a response, correlated by a request id
- Modules: `api/rest/handlers/stream.rs`, `domain/services/stream/mod.rs`, `domain/services/stream/events.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::happy_path_event_order_and_persistence`
  - `domain::services::stream::stream_service_tests::omitted_request_id_is_generated`
  - `api::rest::sse::sse_tests::stream_message_request_defaults`
- E2E:
  - `test_stream.py::test_default_script_streams_and_persists`
  - `test_stream.py::test_turn_status_after_stream`
  - `test_idempotency.py::test_omitted_request_id_is_generated`

### Preflight validation (content, attachments, limits) runs before any provider call
- Modules: `domain/services/stream/setup.rs` (`turn_facts`, `check_effective_model`), `domain/services/stream/mod.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::empty_content_rejected_before_authz`
  - `domain::services::stream::stream_service_tests::duplicate_attachment_ids_invalid_attachment`
  - `domain::services::stream::stream_service_tests::too_many_attachment_ids_invalid_attachment`
  - `domain::services::stream::stream_service_tests::too_many_images`
  - `domain::services::stream::stream_service_tests::foreign_or_not_ready_attachment_rolls_back_reserve`
  - `domain::services::stream::stream_service_tests::vision_rejected_after_downgrade`
  - `domain::services::stream::stream_service_tests::disable_images_rejects`
  - `domain::services::stream::stream_service_tests::web_search_kill_switch_400_no_stream`
  - `domain::services::stream::stream_service_tests::removed_chat_model_is_invalid_model`
- E2E:
  - `test_errors.py::test_empty_content_is_400`
  - `test_errors.py::test_missing_content_is_422_and_malformed_json_400`
  - `test_errors.py::test_non_uuid_attachment_id_is_422`
  - `test_errors.py::test_duplicate_and_unknown_attachment_ids_are_400`
  - `test_context.py::test_long_message_on_small_model_is_input_too_long`
  - `test_authorization.py::test_foreign_attachment_id_cannot_be_referenced_from_own_chat` (T19)

### Assistant message and usage are persisted once a stream completes
- Modules: `domain/services/finalization_service.rs`, `infra/db/repos/message_repo.rs`, `infra/db/repos/turn_repo.rs`
- Unit:
  - `domain::services::finalization_service::finalization_service_tests::completed_persists_message_settles_and_enqueues_once`
  - `domain::services::finalization_service::finalization_service_tests::completed_message_failure_finalizes_failed_message_persistence`
  - `domain::services::stream::stream_service_tests::message_persistence_failure_is_reported`
  - `infra::db::repos::repos_tests::insert_message_persists_every_field`
- E2E:
  - `test_stream.py::test_default_script_streams_and_persists`
  - `test_stream.py::test_usage_event_is_published_exactly_once_per_turn` (T19)

## SSE Event Contract

### Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering
- Modules:
  - `domain/services/stream/events.rs`
  - `domain/services/stream/relay.rs`: one terminal event; pings only before the first content.
  - `api/rest/sse.rs`
  - `infra/llm/providers/openai_responses.rs`
- Unit:
  - `api::rest::sse::sse_tests::events_render_with_contract_names_and_payloads`
  - `api::rest::sse::sse_tests::sse_event_schema_is_a_oneof_of_named_events`
  - `api::rest::sse::sse_tests::optional_fields_are_omitted`
  - `domain::services::stream::stream_service_tests::happy_path_event_order_and_persistence`
  - `domain::services::stream::stream_service_tests::pings_only_before_first_content`
  - `domain::services::stream::stream_service_tests::relay_stops_after_terminal`
  - `domain::services::stream::stream_service_tests::relay_appends_stream_interrupted_without_terminal`
  - `domain::services::stream::stream_service_tests::empty_deltas_skipped_and_second_terminal_ignored`
  - `domain::services::stream::stream_service_tests::file_citations_map_to_attachments`
  - `domain::services::stream::stream_service_tests::incomplete_response_sends_no_citations`
  - `domain::services::stream::stream_service_tests::reasoning_delta_is_relayed_but_not_persisted`
  - `domain::services::stream::stream_service_tests::summary_applied_reported_in_stream_started`
  - `infra::llm::providers::openai_responses::openai_responses_tests::web_search_tool_events`
  - `infra::llm::providers::openai_responses::openai_responses_tests::file_search_events_and_files_searched`
  - `infra::llm::providers::openai_responses::openai_responses_tests::web_citation_snippet_from_range`
- E2E:
  - `test_sse_contract.py::test_done_payload_shape`
  - `test_sse_contract.py::test_tool_events_are_relayed`
  - `test_sse_contract.py::test_pings_are_sent_while_the_model_is_silent`
  - `test_sse_contract.py::test_openapi_declares_the_stream_operation`
  - `test_web_search.py::test_web_citations_are_mapped`
  - `test_stream.py::test_default_script_streams_and_persists`

### Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers
- Modules: `domain/services/stream/events.rs` (`DoneData`), `domain/services/quota_service.rs` (warnings, decision), `domain/sanitize.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::downgrade_is_reported_in_done`
  - `domain::services::stream::stream_service_tests::replay_of_downgraded_turn_rebuilds_decision_without_reason`
  - `domain::services::quota_service::quota_service_tests::status_and_warnings_flags`
  - `api::rest::dto::dto_tests::quota_decision_maps_to_lowercase_values`
  - `api::rest::dto::dto_tests::quota_warning_omits_absent_next_reset`
- E2E:
  - `test_sse_contract.py::test_done_payload_shape`
  - `test_stream.py::test_default_script_streams_and_persists` (no `resp_` in the raw stream)
  - `test_principles.py::test_downgrade_changes_the_effective_model_only` (T19; `quota_decision` downgrade, `downgrade_from`)

### Error event is terminal and carries a sanitized message
- Modules: `domain/services/stream/relay.rs`, `domain/sanitize.rs`, `domain/error.rs` (`sse_code`)
- Unit:
  - `domain::services::stream::stream_service_tests::provider_failed_event_is_terminal_error_sanitized`
  - `domain::services::stream::stream_service_tests::provider_open_error_is_sse_error`
  - `domain::services::stream::stream_service_tests::stream_without_terminal_is_provider_error`
  - `api::rest::sse::sse_tests::error_event_is_code_and_message`
  - `domain::error::error_tests::sse_codes_are_always_documented_streaming_codes`
- E2E:
  - `test_sse_contract.py::test_provider_failure_is_terminal_error_event`
  - `test_errors.py::test_provider_error_message_is_sanitized`
  - `test_errors.py::test_stream_without_terminal_is_provider_error`

## Idempotency & Replay

### Replaying a known request id returns the stored result without side effects (no provider call, no quota change)
- Modules: `domain/services/stream/replay.rs`, `domain/services/stream/mod.rs`
- Unit: `domain::services::stream::stream_service_tests::replay_is_side_effect_free`
- E2E:
  - `test_idempotency.py::test_completed_request_id_is_replayed_without_side_effects`
  - `test_stream.py::test_usage_event_is_published_exactly_once_per_turn` (T19; a replay publishes no usage event)

### Conflicting reuse of a request id across turn states is rejected consistently
- Modules: `domain/services/stream/mod.rs`, `infra/db/repos/turn_repo.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::request_id_conflict_for_failed_cancelled_running_and_deleted_turns`
  - `domain::services::stream::stream_service_tests::commit_unique_violation_on_same_request_id_is_request_id_conflict`
  - `infra::db::migrations_tests::request_id_unique_per_chat`
- E2E:
  - `test_idempotency.py::test_failed_turn_request_id_is_409_conflict`
  - `test_idempotency.py::test_other_states_are_409_conflict`
  - `test_turn_mutations.py::test_retry_streams_a_new_turn_and_replaces_the_old` (the replaced id gets 409)

### Replay is checked before the parallel-turn guard
- Modules: `domain/services/stream/mod.rs`
- Unit: `domain::services::stream::stream_service_tests::replay_checked_before_parallel_guard`
- E2E: `test_idempotency.py::test_replay_is_checked_before_the_parallel_turn_guard` (T19)

## Parallel Turn Enforcement

### Only one turn may run per chat at a time
- Modules: `domain/services/stream/mod.rs`, `infra/db/migrations/m0001_initial.rs` (unique running index)
- Unit:
  - `domain::services::stream::stream_service_tests::parallel_guard_rejects_second_running`
  - `domain::services::stream::stream_service_tests::concurrent_sends_one_wins`
  - `domain::services::stream::stream_service_tests::commit_unique_violation_on_running_index_is_turn_already_running`
  - `infra::db::migrations_tests::one_running_turn_per_chat`
- E2E: `test_parallel.py::test_second_send_while_running_is_409_then_allowed`, `test_parallel.py::test_concurrent_sends_one_wins`

### A new turn is accepted once the previous one reaches a terminal state
- Unit: `domain::services::stream::stream_service_tests::new_turn_allowed_after_terminal_and_history_is_sent`
- E2E:
  - `test_parallel.py::test_second_send_while_running_is_409_then_allowed`
  - `test_stream.py::test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer` (T19; a new turn right after a cancel)

## Turn Mutations

### Retry / edit / delete act only on the latest, terminal turn
- Modules: `domain/services/turn_service.rs`, `api/rest/handlers/turns.rs`, `infra/db/repos/turn_repo.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::mutation_tests::mutation_of_non_latest_is_409`
  - `domain::services::stream::stream_service_tests::mutation_tests::mutation_of_deleted_turn_is_409_not_latest`
  - `domain::services::stream::stream_service_tests::mutation_tests::running_turn_is_400_turn_state`
  - `domain::services::stream::stream_service_tests::mutation_tests::other_requester_403`
  - `domain::services::turn_service::turn_service_tests::latest_turn_is_ordered_by_started_at_then_id`
  - `domain::services::turn_service::turn_service_tests::delete_preview_rejections_change_nothing`
- E2E:
  - `test_turn_mutations.py::test_retry_of_deleted_turn_is_409_not_latest`
  - `test_turn_mutations.py::test_mutation_of_non_latest_turn_is_409`
  - `test_turn_mutations.py::test_running_turn_is_400_turn_state`
  - `test_turn_mutations.py::test_other_requester_is_403`
  - `test_turn_mutations.py::test_delete_soft_deletes_and_status_404`

### A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id
- Modules: `domain/services/turn_service.rs` (preview), `domain/services/stream/mod.rs` (mutation start)
- Unit:
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_replaces_turn_and_streams_new_request_id`
  - `domain::services::stream::stream_service_tests::mutation_tests::reserve_fills_preflight_once_and_rechecks_quota`
  - `domain::services::stream::stream_service_tests::mutation_tests::quota_rejection_leaves_previous_turn_intact`
  - `domain::services::stream::stream_service_tests::mutation_tests::edit_input_too_long_rejected_before_mutation`
  - `domain::services::stream::stream_service_tests::mutation_tests::edit_empty_content_rejected_after_preview`
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_resends_images_with_guards`
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_preflight_ignores_the_replaced_turn_usage`
  - `domain::services::stream::stream_service_tests::mutation_tests::setup_failure_marks_new_turn_failed`
- E2E:
  - `test_turn_mutations.py::test_retry_streams_a_new_turn_and_replaces_the_old`
  - `test_turn_mutations.py::test_retry_quota_rejection_keeps_the_previous_turn`
  - `test_turn_mutations.py::test_edit_streams_with_new_content`
  - `test_turn_mutations.py::test_edit_validation`

### Concurrent mutations resolve deterministically
- Modules: `domain/services/turn_service.rs` (guarded commit), `infra/db/repos/turn_repo.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::mutation_tests::concurrent_retries_one_wins_generation_in_progress`
  - `domain::services::stream::stream_service_tests::mutation_tests::concurrent_retry_race_has_one_winner`
  - `domain::services::stream::stream_service_tests::mutation_tests::running_turn_insert_race_is_generation_in_progress`
  - `domain::services::stream::stream_service_tests::mutation_tests::newer_turn_after_preview_is_not_latest`
  - `domain::services::turn_service::turn_service_tests::stale_delete_commit_loses_guarded_update`
- E2E: `test_turn_mutations.py::test_concurrent_retries_have_exactly_one_winner` (T19)

### Mutated turns correctly carry forward attachment and tool-usage history
- Unit:
  - `domain::services::stream::stream_service_tests::mutation_tests::edit_uses_new_content_and_copies_attachments`
  - `domain::services::stream::stream_service_tests::mutation_tests::deleted_attachment_not_copied`
  - `domain::services::stream::stream_service_tests::mutation_tests::attachment_deleted_after_preview_is_not_copied`
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_copies_web_search_flag_and_bumps_chat`
  - `domain::services::stream::stream_service_tests::mutation_tests::retry_resends_images_with_guards`
- E2E: `test_turn_mutations.py::test_retry_streams_a_new_turn_and_replaces_the_old`, `test_turn_mutations.py::test_edit_streams_with_new_content`

## Turn Lifecycle

### Turn state machine (running → completed / cancelled / failed) is consistent end to end
- Modules:
  - `domain/services/finalization_service.rs`: CAS finalization.
  - `domain/services/turn_service.rs`: status mapping.
  - `infra/workers/orphan_watchdog.rs`
  - `infra/db/migrations/m0001_initial.rs`: CHECK constraints.
- Unit:
  - `domain::services::finalization_service::finalization_service_tests::second_finalize_is_noop`
  - `domain::services::finalization_service::finalization_service_tests::finalize_unstarted_is_plain_cas`
  - `domain::services::finalization_service::finalization_service_tests::incomplete_is_completed_without_error_code`
  - `domain::services::turn_service::turn_service_tests::turn_status_maps_states_and_hides_deleted`
  - `domain::services::stream::stream_service_tests::cas_lost_relays_stream_interrupted`
  - `domain::enums::enums_tests::terminal_states`
  - `infra::db::migrations_tests::check_constraints_reject_invalid_values`
  - `infra::workers::workers_tests::watchdog_finalizes_stale_running_turn`
- E2E:
  - `test_reactions.py::test_turn_status`
  - `test_stream.py::test_turn_status_after_stream`
  - `test_stream.py::test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer` (T19)
  - `test_stream.py::test_provider_failure_after_partial_text_persists_no_answer` (T19)
  - `test_recovery.py::test_crash_restart_recovers_orphan_turn_and_abandoned_uploads`

### Partial and null-content cases on cancellation or failure are handled correctly
- Modules: `domain/services/finalization_service.rs`, `domain/services/stream/provider_task.rs`
- Unit:
  - `domain::services::finalization_service::finalization_service_tests::cancelled_persists_partial_and_aborted_estimated`
  - `domain::services::finalization_service::finalization_service_tests::cancelled_empty_text_has_no_message`
  - `domain::services::finalization_service::finalization_service_tests::cancelled_message_failure_still_cancels_without_message`
  - `domain::services::stream::stream_service_tests::disconnect_cancels_and_settles_estimated`
  - `domain::services::stream::stream_service_tests::dropped_receiver_alone_is_a_disconnect`
- E2E:
  - `test_stream.py::test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer` (T19)
  - `test_stream.py::test_client_disconnect_before_any_text_cancels_without_a_message` (T19)
  - `test_stream.py::test_provider_failure_after_partial_text_persists_no_answer` (T19)

## Attachments

### Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation
- Modules:
  - `api/rest/handlers/attachments.rs`: streaming multipart, size limit.
  - `domain/services/attachment_service.rs`, `domain/services/attachment_service/content.rs`
  - `infra/db/repos/attachment_repo.rs`, `infra/thumbnail.rs`
- Unit:
  - `domain::services::attachment_service::attachment_service_tests::document_upload_indexes_and_is_ready`
  - `domain::services::attachment_service::attachment_service_tests::image_upload_has_thumbnail_and_no_vector_store`
  - `domain::services::attachment_service::attachment_service_tests::unsupported_type_400`
  - `domain::services::attachment_service::attachment_service_tests::oversize_file_is_too_large`
  - `domain::services::attachment_service::attachment_service_tests::document_limit_429`
  - `domain::services::attachment_service::attachment_service_tests::storage_limit_counts_images_and_excludes_failed`
  - `domain::services::attachment_service::attachment_service_tests::delete_referenced_is_locked`
  - `domain::services::attachment_service::attachment_service_tests::delete_twice_is_204_single_outbox_row`
  - `domain::services::attachment_service::attachment_service_tests::get_returns_own_attachment`
  - `api::rest::handlers::attachments::attachments_tests::oversize_stream_is_aborted_without_reading_the_rest`
  - `api::rest::handlers::attachments::attachments_tests::multipart_errors`
  - `infra::thumbnail::thumbnail_tests::fits_inside_preserving_aspect`
- E2E:
  - `test_attachments.py::test_pdf_upload_is_indexed_and_ready`
  - `test_attachments.py::test_png_upload_has_webp_thumbnail`
  - `test_attachments.py::test_xlsx_upload_is_for_code_interpreter`
  - `test_attachments.py::test_unsupported_type_is_400`
  - `test_attachments.py::test_image_over_limit_is_file_too_large`
  - `test_attachments.py::test_multipart_errors_are_400`
  - `test_attachments.py::test_upload_to_unknown_chat_or_removed_model`
  - `test_attachments.py::test_get_unknown_or_foreign_is_404`
  - `test_attachments.py::test_delete_locked_then_unreferenced`

### Asynchronous indexing lifecycle, including provider failure and timeout handling
- Modules: `domain/services/attachment_service/indexing.rs`, `infra/llm/storage/mod.rs`
- Unit:
  - `domain::services::attachment_service::attachment_service_tests::document_polls_until_completed`
  - `domain::services::attachment_service::attachment_service_tests::indexing_failure_marks_failed_and_503`
  - `domain::services::attachment_service::attachment_service_tests::indexing_timeout_returns_uploaded_then_background_ready`
  - `domain::services::attachment_service::attachment_service_tests::background_times_out_after_ten_minutes`
  - `domain::services::attachment_service::attachment_service_tests::background_failure_enqueues_cleanup`
  - `domain::services::attachment_service::attachment_service_tests::provider_upload_failure_marks_failed_and_503`
  - `domain::services::attachment_service::attachment_service_tests::upload_concurrency_limit_is_503`
  - `infra::llm::storage::storage_tests::status_codes_classify_transient_and_failed`
- E2E: `test_attachments.py::test_indexing_failure_is_503_and_row_failed`, `test_attachments.py::test_slow_indexing_returns_uploaded_then_ready`

### Attachments are correctly made available to the relevant provider tools
- Modules: `domain/tools.rs` (`select_tools`, `tool_guards`), `domain/estimation.rs` (`tool_gates`), `domain/services/stream/setup.rs`
- Unit:
  - `domain::tools::tools_tests::select_tools_shapes`
  - `domain::estimation::estimation_tests::tool_gates_follow_support_switches_and_chat_state`
  - `domain::services::stream::stream_service_tests::valid_attachments_are_linked_to_the_user_message`
  - `domain::services::stream::stream_service_tests::image_is_sent_as_input_image`
  - `domain::services::quota_service::quota_service_tests::code_interpreter_daily_quota_requires_ready_xlsx`
- E2E:
  - `test_attachments.py::test_message_with_document_uses_file_search`
  - `test_attachments.py::test_image_turn_sends_input_image`
  - `test_attachments.py::test_xlsx_enables_code_interpreter`

### Cleanup and abandoned-upload recovery behave correctly under failure
- Modules: `infra/outbox/attachment_cleanup.rs`, `infra/workers/upload_reaper.rs`, `domain/services/attachment_service.rs`
- Unit:
  - `infra::outbox::handlers_tests::attachment_cleanup_deletes_file_and_marks_done`
  - `infra::outbox::handlers_tests::attachment_cleanup_404_is_success`
  - `infra::outbox::handlers_tests::attachment_cleanup_failure_increments_then_fails_at_max`
  - `infra::outbox::handlers_tests::attachment_cleanup_db_error_retries_without_counting`
  - `infra::outbox::handlers_tests::attachment_cleanup_skips_when_chat_deleted`
  - `infra::workers::workers_tests::reaper_fails_stale_pending`
  - `infra::workers::workers_tests::reaper_enqueues_cleanup_when_provider_file_present`
  - `infra::workers::workers_tests::reaper_skips_rows_owned_by_chat_cleanup`
  - `domain::services::attachment_service::attachment_service_tests::chat_deleted_during_upload_deletes_provider_file_and_fails`
  - `domain::services::attachment_service::attachment_service_tests::stale_placeholder_is_reclaimed`
- E2E:
  - `test_cleanup.py::test_attachment_delete_cleans_provider_file`
  - `test_recovery.py::test_reaper_fails_abandoned_uploads_after_restart`
  - `test_recovery.py::test_crash_restart_recovers_orphan_turn_and_abandoned_uploads`

## Models API

### Read-only model list/get reflects only enabled catalog entries, without exposing internal fields
- Modules: `api/rest/handlers/models.rs`, `domain/services/model_service.rs`, `api/rest/dto.rs` (`ModelDto`)
- Unit:
  - `domain::services::model_service::model_service_tests::list_returns_only_enabled_in_catalog_order`
  - `domain::services::model_service::model_service_tests::get_disabled_is_not_found_model`
  - `domain::services::model_service::model_service_tests::dto_hides_internal_fields`
  - `domain::services::model_service::model_service_tests::denied_caller_never_reads_the_catalog`
  - `domain::authz::authz_tests::model_access_is_a_decision_only_check`
- E2E:
  - `test_00_boot.py::test_models_listed`
  - `test_00_boot.py::test_unknown_model_is_404_problem`
  - `test_00_boot.py::test_model_list_and_get_expose_only_public_catalog_fields` (T19)

## Reactions API

### Set/remove reaction on assistant messages only, idempotently
- Modules: `api/rest/handlers/reactions.rs`, `domain/services/reaction_service.rs`, `infra/db/repos/reaction_repo.rs`
- Unit:
  - `domain::services::reaction_service::reaction_service_tests::reaction_upsert_replaces`
  - `domain::services::reaction_service::reaction_service_tests::reaction_remove_is_idempotent_and_only_touches_the_caller`
  - `domain::services::reaction_service::reaction_service_tests::reaction_on_user_message_is_failed_precondition`
  - `domain::services::reaction_service::reaction_service_tests::reaction_invalid_value_checked_before_authz`
  - `infra::db::migrations_tests::reaction_unique_per_user_and_value_checked`
- E2E:
  - `test_reactions.py::test_set_replace_list_and_remove`
  - `test_reactions.py::test_reaction_on_user_message_is_failed_precondition`
  - `test_reactions.py::test_invalid_reaction_value_is_400_before_lookup`
  - `test_reactions.py::test_missing_message_is_404_message`
  - `test_reactions.py::test_missing_chat_is_404_chat`

## Quota Status API

### Quota status reporting is accurate and consistent with actual usage
- Modules: `api/rest/handlers/quota.rs`, `domain/services/quota_service.rs` (`status`)
- Unit:
  - `domain::services::quota_service::quota_service_tests::status_reports_caller_usage_with_breakdown`
  - `domain::services::quota_service::quota_service_tests::remaining_percentage_floors_and_clamps`
  - `domain::services::quota_service::quota_service_tests::status_and_warnings_flags`
  - `domain::services::quota_service::quota_service_tests::status_requires_quota_authorization`
  - `api::rest::dto::dto_tests::quota_status_response_serializes_contract_shape`
- E2E:
  - `test_quota_status.py::test_shape_two_tiers_two_periods`
  - `test_quota_status.py::test_used_credits_reflect_seeded_usage`
  - `test_quota_status.py::test_settlement_charges_actual_credits_per_model_and_tier` (T19)
  - `test_authorization.py::test_quota_status_of_other_tenant_is_unaffected_by_usage` (T19)

## Quota Enforcement

### Reserve-before-execute quota flow enforced on every provider call
- Modules: `domain/services/quota_service.rs` (`preflight`, `reserve_in_tx`), `domain/services/stream/mod.rs`, `domain/services/turn_service.rs`
- Unit:
  - `domain::services::quota_service::quota_service_tests::reserve_increments_total_and_premium_rows`
  - `domain::services::quota_service::quota_service_tests::reserve_recheck_rejects_concurrent_over_limit`
  - `domain::services::quota_service::quota_service_tests::reserve_recheck_covers_premium_bucket`
  - `domain::services::quota_service::quota_service_tests::in_flight_reserves_count_against_the_limit`
  - `domain::services::quota_service::quota_service_tests::preflight_counts_images_prior_context_and_tool_surcharges`
  - `domain::services::stream::stream_service_tests::quota_exhausted_429_no_provider_call`
  - `domain::services::stream::stream_service_tests::mutation_tests::reserve_fills_preflight_once_and_rechecks_quota`
- E2E:
  - `test_principles.py::test_exhausted_quota_is_429_before_any_provider_call` (T19)
  - `test_turn_mutations.py::test_retry_quota_rejection_keeps_the_previous_turn`
  - `test_stream.py::test_default_script_streams_and_persists` (the reserve is released after settlement)

### Tier downgrade applied when a higher tier is exhausted
- Modules: `domain/services/quota_service.rs` (cascade, kill switches)
- Unit:
  - `domain::services::quota_service::quota_service_tests::downgrade_when_premium_daily_exhausted`
  - `domain::services::quota_service::quota_service_tests::downgrade_when_monthly_exhausted`
  - `domain::services::quota_service::quota_service_tests::standard_never_upgrades`
  - `domain::services::quota_service::quota_service_tests::disable_premium_tier_reason`
  - `domain::services::quota_service::quota_service_tests::force_standard_tier_reason`
  - `domain::services::quota_service::quota_service_tests::design_example_premium_reserve_does_not_fit_premium_daily`
  - `domain::services::stream::stream_service_tests::downgrade_is_reported_in_done`
  - `domain::services::stream::stream_service_tests::vision_rejected_after_downgrade`
- E2E: `test_principles.py::test_downgrade_changes_the_effective_model_only` (T19)

### Credits and tokens are accounted correctly per model and tier
- Modules: `domain/credits.rs`, `domain/services/quota_service.rs` (`settle_in_tx`), `infra/db/repos/quota_repo.rs`
- Unit:
  - `domain::credits::credits_tests::per_component_ceil`
  - `domain::credits::credits_tests::section_5_10_example`
  - `domain::services::quota_service::quota_service_tests::settle_premium_updates_both_buckets_and_calls`
  - `domain::services::quota_service::quota_service_tests::standard_reserve_creates_no_premium_row`
  - `domain::services::quota_service::quota_service_tests::candidate_reserve_uses_own_budgets`
  - `domain::services::quota_service::quota_service_tests::settle_actual_within_tolerance`
  - `domain::services::quota_service::quota_service_tests::settle_overshoot_capped_at_reserve`
- E2E:
  - `test_quota_status.py::test_settlement_charges_actual_credits_per_model_and_tier` (T19; premium 3x/15x charges both buckets, standard 1x/3x charges only `total`)
  - `test_stream.py::test_usage_event_is_published_exactly_once_per_turn` (T19; `actual_credits_micro`)

## Settlement & Finalization

### Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate
- Modules: `domain/billing.rs` (`derive_billing`), `domain/services/finalization_service.rs`, `domain/services/quota_service.rs`
- Unit:
  - `domain::billing::billing_tests::completed_is_always_actual`
  - `domain::billing::billing_tests::cancelled_is_aborted_estimated`
  - `domain::billing::billing_tests::provider_errors_with_usage_are_actual`
  - `domain::billing::billing_tests::provider_error_without_usage_is_estimated`
  - `domain::billing::billing_tests::pre_provider_codes_are_released_even_with_usage`
  - `domain::billing::billing_tests::orphan_timeout_is_aborted_estimated`
  - `domain::services::finalization_service::finalization_service_tests::second_finalize_is_noop`
  - `domain::services::finalization_service::finalization_service_tests::failed_with_usage_settles_actual`
  - `domain::services::finalization_service::finalization_service_tests::failed_without_usage_settles_estimated`
  - `domain::services::finalization_service::finalization_service_tests::pre_provider_failure_releases_reserve`
  - `domain::services::finalization_service::finalization_service_tests::orphan_without_reserve_skips_settlement_but_enqueues_usage`
  - `domain::services::quota_service::quota_service_tests::settle_estimated_formula`
  - `domain::services::quota_service::quota_service_tests::settle_released_zero_charge`
  - `infra::workers::workers_tests::watchdog_finalizes_stale_running_turn`
- E2E:
  - `test_stream.py::test_default_script_streams_and_persists`
  - `test_stream.py::test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer` (T19; reserve released after the cancel)
  - `test_quota_status.py::test_settlement_charges_actual_credits_per_model_and_tier` (T19)

### Usage is published reliably and exactly once per turn
- Modules: `infra/outbox/mod.rs` (enqueue in the finalization transaction), `infra/outbox/usage_handler.rs`, `infra/plugins/policy_gateway.rs`
- Unit:
  - `domain::services::finalization_service::finalization_service_tests::completed_persists_message_settles_and_enqueues_once`
  - `domain::services::finalization_service::finalization_service_tests::second_finalize_is_noop`
  - `infra::outbox::outbox_tests::each_queue_delivers_its_payload_type_after_commit`
  - `infra::outbox::handlers_tests::usage_handler_permanent_rejects_transient_retries`
  - `infra::plugins::gateways_tests::policy_gateway_publish_usage_without_plugin_is_transient`
  - `mini_chat_sdk::usage::usage_tests::user_usage_event_has_null_usage_when_unknown`
- E2E: `test_stream.py::test_usage_event_is_published_exactly_once_per_turn` (T19)

## Context Assembly

### System prompt, thread summary, and recent history are assembled and truncated deterministically within budget
- Modules: `domain/context.rs`, `domain/services/stream/setup.rs` (`prepare_request`), `infra/db/repos/message_repo.rs` (`recent_for_context`)
- Unit:
  - `domain::context::context_tests::deterministic_for_same_input`
  - `domain::context::context_tests::oldest_history_dropped_first_whole_turns`
  - `domain::context::context_tests::newest_first_stops_at_first_misfit`
  - `domain::context::context_tests::summary_dropped_when_not_fitting`
  - `domain::context::context_tests::summary_sent_as_user_message_with_preamble`
  - `domain::context::context_tests::system_and_user_never_truncated`
  - `infra::db::repos::repos_tests::recent_for_context_skips_compressed_messages`
  - `infra::db::repos::repos_tests::snapshot_boundary_and_recent_for_context_window`
- E2E:
  - `test_context.py::test_history_is_truncated_by_whole_turns`
  - `test_stream.py::test_second_turn_sends_history`
  - `test_thread_summary.py::test_summary_is_generated_and_applied_to_the_next_turn`

### Tool availability and guidance are reflected correctly in the assembled request
- Modules: `domain/tools.rs`, `domain/context.rs` (guards after the system prompt), `domain/estimation.rs` (`tool_gates`, surcharges)
- Unit:
  - `domain::tools::tools_tests::tool_guards_follow_sent_tools`
  - `domain::tools::tools_tests::select_tools_shapes`
  - `domain::context::context_tests::guards_appended_after_system_prompt`
  - `domain::estimation::estimation_tests::tool_surcharges_count_only_gated_tools`
  - `domain::services::stream::stream_service_tests::web_search_tool_included_only_when_supported`
  - `infra::llm::providers::openai_responses::openai_responses_tests::builds_request_body`
- E2E:
  - `test_web_search.py::test_enabled_web_search_sends_tool_and_guard`
  - `test_web_search.py::test_disabled_or_unsupported_web_search_sends_no_tool`
  - `test_attachments.py::test_message_with_document_uses_file_search`
  - `test_attachments.py::test_xlsx_enables_code_interpreter`

## Error Mapping & Sanitization

### All errors map to the canonical error contract, consistently across REST and streaming
- Modules: `api/rest/error.rs`, `domain/error.rs` (`sse_code`), `api/rest/sse.rs`
- Unit:
  - every test in `api::rest::error::error_tests`, for example `api::rest::error::error_tests::not_found_names_the_missing_resource_type`, `api::rest::error::error_tests::authz_unavailable_is_503_with_retry_after_5`, `api::rest::error::error_tests::quota_exceeded_names_the_scope`
  - `domain::error::error_tests::sse_codes_are_always_documented_streaming_codes`
  - `domain::authz::authz_tests::pdp_outage_is_503_with_retry_after_on_every_operation` (T19)
- E2E:
  - `test_errors.py::test_provider_429_is_rate_limited_event`
  - `test_errors.py::test_provider_http_500_is_provider_error`
  - `test_errors.py::test_unknown_and_foreign_chat_are_404`
  - `test_errors.py::test_unauthenticated_is_401`
  - `test_authorization.py::test_unauthenticated_is_401_on_every_endpoint` (T19)
  - `assert_problem` (canonical Problem check) in every e2e file

### Provider-originated error details are sanitized before reaching the client
- Modules: `domain/sanitize.rs`, `infra/llm/providers/openai_responses.rs`
- Unit:
  - every test in `domain::sanitize::sanitize_tests`, for example `domain::sanitize::sanitize_tests::response_id_is_replaced`, `domain::sanitize::sanitize_tests::secret_key_is_replaced`
  - `infra::llm::providers::openai_responses::openai_responses_tests::http_error_body_is_parsed_and_sanitized`
  - `infra::llm::providers::openai_responses::openai_responses_tests::response_failed_keeps_usage_and_sanitizes`
  - `domain::error::error_tests::display_never_contains_provider_detail_for_pre_stream_variants`
  - `api::rest::error::error_tests::internal_errors_do_not_leak_their_message`
- E2E:
  - `test_errors.py::test_provider_error_message_is_sanitized`
  - `test_attachments.py::test_pdf_upload_is_indexed_and_ready` (no provider ids in bodies)

## Web Search

### Web search tool use is reported, cited, accounted, and quota-limited correctly
- Modules: `domain/services/stream/provider_task.rs` (call limit), `domain/services/quota_service.rs` (daily quota, settlement counts), `domain/tools.rs`
- Unit:
  - `domain::services::stream::stream_service_tests::web_search_limit_exceeded_fails_turn`
  - `domain::services::stream::stream_service_tests::tool_done_updates_turn_counts_while_running`
  - `domain::services::quota_service::quota_service_tests::daily_web_search_quota_allows_below_limit`
  - `domain::services::quota_service::quota_service_tests::daily_web_search_quota_only_when_tool_sent`
  - `domain::services::quota_service::quota_service_tests::daily_web_search_quota_follows_the_effective_model`
  - `domain::services::quota_service::quota_service_tests::settle_premium_updates_both_buckets_and_calls`
  - `infra::llm::providers::openai_responses::openai_responses_tests::web_search_tool_events`
- E2E:
  - `test_web_search.py::test_enabled_web_search_sends_tool_and_guard`
  - `test_web_search.py::test_web_citations_are_mapped`
  - `test_web_search.py::test_daily_web_search_quota_is_429`
  - `test_web_search.py::test_web_search_call_limit_fails_turn`
  - `test_web_search.py::test_completed_web_search_calls_are_accounted` (T19)
  - `test_sse_contract.py::test_tool_events_are_relayed`

## Cleanup & Recovery

### Chat deletion triggers reliable background cleanup of provider-side resources
- Modules: `domain/services/chat_service.rs` (`delete`), `infra/outbox/chat_cleanup.rs`, `infra/db/repos/vector_store_repo.rs`
- Unit:
  - `domain::services::chat_service::chat_service_tests::delete_sets_attachment_cleanup_pending_and_enqueues_chat_cleanup`
  - `infra::outbox::handlers_tests::chat_cleanup_deletes_files_then_vector_store_and_row`
  - `infra::outbox::handlers_tests::chat_cleanup_keeps_vs_row_on_failure_and_retries`
  - `infra::outbox::handlers_tests::chat_cleanup_deletes_vector_store_despite_failed_attachment`
  - `infra::outbox::handlers_tests::chat_cleanup_placeholder_deleted_or_retried_when_replaced`
  - `infra::outbox::handlers_tests::chat_cleanup_rejects_active_chat`
  - `infra::outbox::handlers_tests::cleanup_identity_not_ready_retries_without_counting`
- E2E: `test_cleanup.py::test_chat_delete_cleans_files_then_vector_store`

### Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly
- Modules: `domain/services/thread_summary_service.rs`, `infra/outbox/thread_summary.rs`, `domain/services/finalization_service.rs` (trigger), `domain/services/turn_service.rs` (invalidation)
- Unit:
  - `domain::services::thread_summary_service::thread_summary_service_tests::summarizes_exact_range_and_marks_compressed`
  - `domain::services::thread_summary_service::thread_summary_service_tests::provider_error_retries_and_keeps_state`
  - `domain::services::thread_summary_service::thread_summary_service_tests::empty_summary_retries_then_rejects_at_max`
  - `domain::services::thread_summary_service::thread_summary_service_tests::context_length_error_drops_oldest_and_retries_twice`
  - `domain::services::thread_summary_service::thread_summary_service_tests::cas_conflict_finishes_without_commit`
  - `domain::services::thread_summary_service::thread_summary_service_tests::frontier_deleted_skips`
  - `domain::services::finalization_service::finalization_service_tests::summary_enqueued_when_trigger_and_prior_message`
  - `domain::services::turn_service::turn_service_tests::mutation_deletes_covering_summary_and_clears_compressed`
  - `domain::services::turn_service::turn_service_tests::mutation_keeps_summary_that_does_not_cover_the_turn`
- E2E: `test_thread_summary.py::test_summary_is_generated_and_applied_to_the_next_turn`

## Authorization

### Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently
- Modules:
  - `domain/authz.rs`: PEP; fail-closed mapping, with PDP outage → 503 `Retry-After: 5`, logged at `error`.
  - `domain/ports.rs` (`ChatAction::ALL`), `api/rest/error.rs`, `domain/services/*`
- Unit:
  - `domain::authz::authz_tests::every_chat_action_is_sent_by_name_and_owner_scoped`
  - `domain::authz::authz_tests::chat_action_names_match_the_design_matrix`
  - `domain::authz::authz_tests::create_passes_owner_properties_without_resource_id`
  - `domain::authz::authz_tests::chat_scope_denied_is_authz_denied`
  - `domain::authz::authz_tests::chat_scope_without_constraints_fails_closed_as_denied`
  - `domain::authz::authz_tests::chat_scope_evaluation_failure_is_authz_unavailable`
  - `domain::authz::authz_tests::model_access_denied_and_unavailable_are_mapped`
  - `domain::authz::authz_tests::quota_scope_denied_is_authz_denied`
  - `domain::authz::authz_tests::pdp_outage_is_503_with_retry_after_on_every_operation` (T19; failing fake PDP → HTTP response 503 + `Retry-After: 5` for all 16 chat actions, model list/read and quota)
  - `domain::authz::authz_tests::pdp_outage_is_logged_at_error_and_denial_at_warn` (T19)
  - `domain::services::chat_service::chat_service_tests::denied_caller_gets_authz_error`
  - `domain::services::model_service::model_service_tests::denied_caller_never_reads_the_catalog`
  - `domain::services::quota_service::quota_service_tests::status_requires_quota_authorization`
  - `api::rest::error::error_tests::authz_denied_is_403_with_reason`
  - `api::rest::error::error_tests::not_requester_is_403_authz_denied`
- E2E:
  - `test_authorization.py::test_foreign_caller_gets_404_on_every_chat_endpoint` (T19). Covers 15 operations: get, patch and delete chat; list messages; stream, including replaying the owner's request id; turn status; retry, edit and delete turn; upload, get and delete attachment; put and delete reaction. Each runs for tenant B and for the same-tenant reviewer. The test checks 404 (never 403), that the DB is unchanged and that nothing reached the provider.
  - `test_authorization.py::test_unauthenticated_is_401_on_every_endpoint` (T19)
  - `test_chats.py::test_other_tenant_or_user_gets_404`
  - `test_turn_mutations.py::test_unknown_turn_and_foreign_chat_are_404`
  - `test_reactions.py::test_set_replace_list_and_remove`
  - `test_attachments.py::test_get_unknown_or_foreign_is_404`
  - `test_00_boot.py::test_models_visible_to_other_tenant`

---

## Gaps found in the Task 19 sweep and how they were closed

| # | Checklist item | Gap | Closed by |
|---|---|---|---|
| 1 | Tenant/owner isolation; Authorization | Foreign 404 was checked e2e only for chat get/patch/delete, messages, single reviewer probes (retry, delete turn, turn status, get attachment, put reaction) and B on stream. Not covered: edit turn, upload and delete attachment, delete reaction, replay of the owner's request id by a foreign caller, referencing a foreign attachment id from one's own chat, and the "nothing changed / nothing sent" checks. | `test_authorization.py` (4 tests, 24 cases) |
| 2 | Authorization (fail-closed) | PDP outage → 503 was tested in two halves. PEP → `AuthzUnavailable` was tested only for chat `read` and model `read` (`chat_scope_evaluation_failure_is_authz_unavailable`, `model_access_denied_and_unavailable_are_mapped`). `DomainError` → Problem was tested only on the body's `context.retry_after_seconds` (`authz_unavailable_is_503_with_retry_after_5`). Not tested: the `Retry-After` HTTP header on the real response, the other 15 chat actions, model `list`, and the quota check. **Bug:** DESIGN section 3.8 says the outage cause is logged at `error`, but the code logged it at `warn`. | `pdp_outage_is_503_with_retry_after_on_every_operation`; `pdp_outage_is_logged_at_error_and_denial_at_warn` (RED, then fixed in `domain/authz.rs`) |
| 3 | No buffering | No end-to-end check through OAGW, the gear and api_gateway. | `test_principles.py::test_stream_is_relayed_without_buffering` |
| 4 | Quota before outbound | Credit-quota exhaustion had an e2e only for retry (`test_turn_mutations.py::test_retry_quota_rejection_keeps_the_previous_turn`), not for `messages:stream`. The existing `test_web_search.py::test_daily_web_search_quota_is_429` covers the separate web-search call quota, not tokens or credits. | `test_principles.py::test_exhausted_quota_is_429_before_any_provider_call` |
| 5 | Context budget | `CONTEXT_BUDGET_EXCEEDED` was unit-only. | `test_principles.py::test_context_budget_is_enforced_for_message_and_assembled_request`, `test_message_just_within_the_budget_is_sent` |
| 6 | Model immutable; Tier downgrade; done exposes downgrade | Downgrade was unit-only. The "selected model stays, effective model changes" behaviour was not checked e2e. | `test_principles.py::test_chat_model_is_immutable`, `test_downgrade_changes_the_effective_model_only` |
| 7 | Chat ordering reflects activity | E2E covered rename only; a send only checked `updated_at`. | `test_stream.py::test_send_moves_the_chat_to_the_top_of_the_list` |
| 8 | Replay before parallel guard | Unit only. | `test_idempotency.py::test_replay_is_checked_before_the_parallel_turn_guard` |
| 9 | Concurrent mutations | Unit only. | `test_turn_mutations.py::test_concurrent_retries_have_exactly_one_winner` |
| 10 | Partial/null content; state machine | Cancellation on client disconnect was unit only. | `test_stream.py::test_client_disconnect_cancels_the_turn_and_persists_the_partial_answer`, `test_client_disconnect_before_any_text_cancels_without_a_message`, `test_provider_failure_after_partial_text_persists_no_answer` |
| 11 | Usage published exactly once | E2E checked only system (summary) usage events. | `test_stream.py::test_usage_event_is_published_exactly_once_per_turn` |
| 12 | Credits per model/tier; quota status consistent | E2E used only seeded usage. | `test_quota_status.py::test_settlement_charges_actual_credits_per_model_and_tier` |
| 13 | Web search accounted | `web_search_calls` accounting was unit only. | `test_web_search.py::test_completed_web_search_calls_are_accounted` |
| 14 | Models API | E2E did not check `GET /models/{id}` success, the field set or catalog order. | `test_00_boot.py::test_model_list_and_get_expose_only_public_catalog_fields` |

Restart over an existing DB (Review Focus 5) is already covered by `test_recovery.py::test_crash_restart_recovers_orphan_turn_and_abandoned_uploads`. That test SIGKILLs the server, restarts it over the same DB, and checks that models are listed, that chats survive and that the orphan turn is recovered.
