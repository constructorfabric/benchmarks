# mini-chat tests

Run everything (SDK + gear, unit + integration):

```sh
cargo test -p cf-gears-mini-chat-sdk -p cf-gears-mini-chat
```

The integration suites (`tests/*.rs`) drive the real router, domain service, Secure ORM
persistence and outbox pipeline in-process (`tests/common/mod.rs`):

- a temporary file-backed SQLite database (WAL) with the gear and outbox migrations applied;
- a mock PDP that mirrors static-authz (`In(owner_tenant_id)`) and can be switched to deny or fail;
- a fake OAGW `ServiceGatewayClientV1` acting as an OpenAI-compatible provider. It scripts
  Responses SSE streams, HTTP errors, gateway timeouts, slow streams, Files and Vector Store calls,
  and records every outbound request so the tests can assert the provider wire format;
- the bundled static model-policy plugin behind a swappable port that records published usage
  events, and a recording audit port with switchable delivery outcomes;
- two users in tenant A (`ALICE`, `BOB`) and one in tenant B (`CAROL`).

`tests/smoke/mock_openai.py` is the standalone mock provider used for the end-to-end smoke run
against the example server (see `specs/001-mini-chat-gear/quickstart.md`).

## Acceptance criteria → tests

Item text from `gears/mini-chat/docs/acceptance-criteria.md`. `unit:` entries are `#[cfg(test)]`
modules under `src/`.

### Principles & Constraints

| Criterion | Tests |
|---|---|
| Tenant and owner isolation enforced on every resource | `authz::foreign_chat_is_not_found_everywhere`, `authz::same_tenant_other_user_cannot_list_or_read_messages`, `attachments::attachments_are_private_to_the_uploader`, `chats::list_chats_default_order_is_most_recent_activity` |
| Context window budget enforced (input message and full request) | `streaming::input_too_long_and_context_budget_exceeded`, `mutations::rejected_mutation_leaves_previous_turn_intact`, unit: `domain::context::max_input_limits_budget_and_input_too_long`, `…summary_dropped_when_it_does_not_fit_and_mandatory_never_truncated` |
| Streaming responses are never buffered before relaying | `streaming::deltas_are_relayed_without_buffering`, `streaming::ping_is_sent_before_first_content` |
| A chat's model is immutable once set | `chats::model_is_immutable_via_patch`, `quota::premium_exhaustion_downgrades_to_standard` (chat model unchanged after downgrade) |
| Quota is checked before any outbound provider call | `streaming::quota_is_checked_before_provider_call`, `quota::all_tiers_exhausted_is_429_tokens_without_provider_call`, `quota::code_interpreter_daily_quota` |

### Chat CRUD

| Criterion | Tests |
|---|---|
| Create / get / list / update / delete lifecycle incl. model and title validation | `chats::create_chat_default_and_explicit_model`, `chats::create_chat_rejects_invalid_or_disabled_model`, `chats::title_validation`, `chats::get_rename_delete_lifecycle` |
| List filtering, ordering, pagination, malformed input | `chats::list_chats_pagination_filter_orderby`, `chats::list_chats_rejects_malformed_odata` |
| Chat ordering reflects most recent activity | `chats::list_chats_default_order_is_most_recent_activity`, `streaming::happy_path_event_order_and_persistence` (`updated_at` bump) |

### Messages API

| Criterion | Tests |
|---|---|
| List messages with filtering, ordering, pagination | `chats::messages_list_contract_and_chronology`, `chats::messages_of_deleted_or_missing_chat_are_404` |
| Message response contract (identity, attachments, reactions always present) | `chats::messages_list_contract_and_chronology`, `mutations::mutation_carries_attachments_and_tool_settings`, `models_reactions::reactions_upsert_and_delete_idempotently` |
| Message count and chronological ordering across turns | `chats::messages_list_contract_and_chronology`, `mutations::retry_latest_turn_replaces_it_with_new_request_id`, `mutations::delete_latest_turn` |

### Streaming: Send Message

| Criterion | Tests |
|---|---|
| Streams a response correlated by a request id | `streaming::happy_path_event_order_and_persistence`, `streaming::request_id_is_generated_when_omitted` |
| Preflight validation (content, attachments, limits) before any provider call | `streaming::preflight_validation_never_calls_provider`, `attachments::too_many_images_in_one_message`, `attachments::attachments_are_private_to_the_uploader`, `attachments::indexing_deadline_returns_uploaded_and_completes_in_background` (not-ready attachment) |
| Assistant message and usage persisted once a stream completes | `streaming::happy_path_event_order_and_persistence`, `quota::credits_are_accounted_per_model_and_tier` |

### SSE Event Contract

| Criterion | Tests |
|---|---|
| Full event contract (start, delta, tool, citations, done, error, ping) and ordering | `streaming::happy_path_event_order_and_persistence`, `streaming::ping_is_sent_before_first_content`, `attachments::tools_and_images_in_provider_request`, `quota::web_search_tool_citations_and_accounting`, unit: `infra::llm::adapters::translates_responses_stream` |
| `done` exposes usage and quota/downgrade outcome without internal ids | `streaming::happy_path_event_order_and_persistence`, `streaming::sse_payloads_never_contain_internal_ids`, `quota::premium_exhaustion_downgrades_to_standard`, `quota::quota_warnings_in_done_event` |
| Error event is terminal and sanitized | `streaming::provider_http_error_is_terminal_sanitized_error_event`, `streaming::failed_event_mid_stream_uses_reported_usage`, unit: `domain::sanitize::scrubs_ids_urls_and_keys`, `infra::llm::adapters::failed_event_is_sanitized` |

### Idempotency & Replay

| Criterion | Tests |
|---|---|
| Replay returns the stored result without side effects | `idempotency::replay_of_completed_turn_is_side_effect_free` |
| Conflicting reuse of a request id rejected consistently | `idempotency::request_id_conflicts_for_failed_cancelled_running_and_deleted`, `mutations::retry_latest_turn_replaces_it_with_new_request_id` |
| Replay is checked before the parallel-turn guard | `idempotency::replay_is_checked_before_parallel_guard` |

### Parallel Turn Enforcement

| Criterion | Tests |
|---|---|
| Only one turn may run per chat | `idempotency::only_one_turn_runs_per_chat_under_concurrency`, `idempotency::request_id_conflicts_for_failed_cancelled_running_and_deleted` |
| A new turn is accepted once the previous one is terminal | `idempotency::request_id_conflicts_for_failed_cancelled_running_and_deleted`, `idempotency::orphan_watchdog_finalizes_stale_running_turn` |

### Turn Mutations

| Criterion | Tests |
|---|---|
| Retry / edit / delete only on the latest terminal turn | `mutations::only_latest_terminal_turn_can_be_mutated`, `mutations::delete_latest_turn`, `mutations::failed_and_cancelled_turns_can_be_retried` |
| Full send pipeline (quota, context budget, attachments) and a new request id | `mutations::retry_latest_turn_replaces_it_with_new_request_id`, `mutations::edit_latest_turn_uses_new_content`, `mutations::rejected_mutation_leaves_previous_turn_intact` |
| Concurrent mutations resolve deterministically | `mutations::concurrent_mutations_resolve_deterministically` |
| Attachment and tool-usage history carried forward | `mutations::mutation_carries_attachments_and_tool_settings` |

### Turn Lifecycle

| Criterion | Tests |
|---|---|
| State machine running → completed / cancelled / failed | `idempotency::turn_status_in_every_state`, `streaming::provider_rate_limit_and_timeout_codes`, `streaming::provider_stream_ending_without_terminal_event_fails`, `streaming::incomplete_response_still_completes` |
| Partial and null content on cancellation or failure | `streaming::client_disconnect_cancels_with_partial_text_and_estimated_settlement`, `streaming::disconnect_before_any_content_leaves_null_content`, `streaming::provider_http_error_is_terminal_sanitized_error_event` |

### Attachments

| Criterion | Tests |
|---|---|
| Upload / get / delete for documents and images with size, type and per-chat limits | `attachments::document_upload_reaches_ready_with_vector_store`, `attachments::image_upload_has_thumbnail_and_no_vector_store`, `attachments::xlsx_goes_to_code_interpreter_and_respects_kill_switch_and_capability`, `attachments::upload_validation_errors`, `attachments::per_chat_document_and_storage_limits`, `attachments::delete_lifecycle_lock_and_idempotency` |
| Asynchronous indexing incl. provider failure and timeout | `attachments::indexing_in_progress_polls_until_completed`, `attachments::indexing_deadline_returns_uploaded_and_completes_in_background`, `attachments::indexing_failure_is_503_and_deletes_provider_file`, `attachments::provider_upload_failure_is_503_and_failed_row` |
| Attachments available to the relevant provider tools | `attachments::tools_and_images_in_provider_request`, `attachments::xlsx_goes_to_code_interpreter_and_respects_kill_switch_and_capability` |
| Cleanup and abandoned-upload recovery under failure | `attachments::upload_reaper_fails_abandoned_uploads`, `cleanup::attachment_cleanup_handler_semantics`, `attachments::delete_lifecycle_lock_and_idempotency` |

### Models API

| Criterion | Tests |
|---|---|
| Read-only list/get of enabled entries, no internal fields | `models_reactions::models_list_and_get_show_enabled_entries_only` |

### Reactions API

| Criterion | Tests |
|---|---|
| Set/remove on assistant messages only, idempotently | `models_reactions::reactions_upsert_and_delete_idempotently`, `models_reactions::reactions_validation` |

### Quota Status API

| Criterion | Tests |
|---|---|
| Accurate and consistent with actual usage | `quota::quota_status_is_consistent_with_usage`, `quota::reserve_is_held_during_the_turn_and_released_at_settlement`, unit: `domain::quota::status_flags` |

### Quota Enforcement

| Criterion | Tests |
|---|---|
| Reserve-before-execute on every provider call | `quota::reserve_is_held_during_the_turn_and_released_at_settlement`, `streaming::quota_is_checked_before_provider_call`, `mutations::rejected_mutation_leaves_previous_turn_intact` |
| Tier downgrade when a higher tier is exhausted | `quota::premium_exhaustion_downgrades_to_standard`, `quota::kill_switches_and_disabled_model_downgrade_with_reason`, unit: `domain::quota::design_example_downgrades_to_standard` |
| Credits and tokens accounted per model and tier | `quota::credits_are_accounted_per_model_and_tier`, unit: `domain::credits::credits_per_component_ceil`, `domain::billing::*` |

### Settlement & Finalization

| Criterion | Tests |
|---|---|
| Every terminal outcome settles quota exactly once (actual vs estimated) | `streaming::happy_path_event_order_and_persistence`, `streaming::provider_http_error_is_terminal_sanitized_error_event`, `streaming::failed_event_mid_stream_uses_reported_usage`, `streaming::client_disconnect_cancels_with_partial_text_and_estimated_settlement`, `idempotency::orphan_watchdog_finalizes_stale_running_turn`, unit: `domain::billing::*` |
| Usage published reliably and exactly once per turn | `quota::usage_publish_is_retried_and_delivered_exactly_once`, `cleanup::usage_handler_outcomes`, `streaming::happy_path_event_order_and_persistence` |

### Context Assembly

| Criterion | Tests |
|---|---|
| System prompt, summary and history assembled and truncated deterministically | `streaming::provider_request_body`, `streaming::history_is_truncated_deterministically_within_budget`, `streaming::thread_summary_is_injected_with_preamble`, unit: `domain::context::*` |
| Tool availability and guidance reflected in the request | `attachments::tools_and_images_in_provider_request`, `quota::web_search_tool_citations_and_accounting`, `mutations::mutation_carries_attachments_and_tool_settings` |

### Error Mapping & Sanitization

| Criterion | Tests |
|---|---|
| All errors map to the canonical contract (REST and streaming) | `authz::invalid_path_and_body_errors`, `authz::pdp_deny_is_403`, `authz::pdp_failure_is_503_with_retry_after`, `streaming::provider_rate_limit_and_timeout_codes`, unit: `api::rest::error::*` |
| Provider error details sanitized | `streaming::provider_http_error_is_terminal_sanitized_error_event`, unit: `domain::sanitize::*` |

### Web Search

| Criterion | Tests |
|---|---|
| Reported, cited, accounted and quota-limited | `quota::web_search_tool_citations_and_accounting`, `quota::web_search_per_message_limit_and_daily_quota_and_kill_switch` |

### Cleanup & Recovery

| Criterion | Tests |
|---|---|
| Chat deletion triggers reliable provider-side cleanup | `cleanup::chat_deletion_cleans_up_provider_files_and_vector_store`, `cleanup::chat_cleanup_retries_failed_deletes_and_stops_at_max_attempts`, `cleanup::chat_cleanup_handler_direct_semantics` |
| Thread summary generation, failure/retry, mutation-driven invalidation | `cleanup::thread_summary_is_triggered_generated_and_used`, `cleanup::thread_summary_failure_cas_and_deleted_frontier`, `cleanup::no_summary_trigger_for_short_chats_or_failed_turns`, `mutations::mutation_invalidates_covering_thread_summary` |

### Authorization

| Criterion | Tests |
|---|---|
| Every operation owner-scoped; foreign access rejected consistently | `authz::foreign_chat_is_not_found_everywhere`, `authz::pdp_deny_is_403`, `authz::pdp_failure_is_503_with_retry_after`, `mutations::only_latest_terminal_turn_can_be_mutated` (foreign requester) |

Audit delivery is covered by `cleanup::audit_handler_delivery_drop_retry_reject`,
`cleanup::audit_gateway_maps_plugin_results` and `cleanup::audit_retries_through_the_outbox_until_delivered`.
The bundled plugins are covered by `cleanup::policy_gateway_serves_static_plugin` and the unit tests in
`src/infra/plugins/`.

## Spec edge cases → tests

| Edge case (spec.md) | Tests |
|---|---|
| Concurrent sends / same request id (unique-index races) | `idempotency::only_one_turn_runs_per_chat_under_concurrency`, `idempotency::request_id_conflicts_for_failed_cancelled_running_and_deleted` |
| Stream without usage; `incomplete` completes the turn | `streaming::incomplete_response_still_completes`, `streaming::provider_stream_ending_without_terminal_event_fails` |
| Chat model removed (400 `INVALID_MODEL`) vs disabled (downgrade `model_disabled`) | `chats::chat_model_removed_from_catalog_is_invalid_model_on_send_and_upload`, `quota::kill_switches_and_disabled_model_downgrade_with_reason` |
| Images on a turn downgraded to a non-vision model | `quota::images_on_turn_downgraded_to_non_vision_model_are_rejected` |
| Non-UUID path (400), malformed JSON (400), schema mismatch (422), non-JSON content type (415) | `authz::invalid_path_and_body_errors` |
| UTC calendar periods; warnings skip limit ≤ 0 | unit: `domain::credits::periods_are_utc_calendar`, `domain::quota::status_skips_periods_without_a_positive_limit` |
| Postgres schema (FR-014) | unit: `infra::db::migrations::m0001_initial::postgres_ddl_uses_native_types` |
