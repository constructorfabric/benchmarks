# Acceptance-Criteria Traceability: Mini Chat

Maps every item of [`acceptance-criteria.md`](./acceptance-criteria.md) (numbered 1–48 in
document order) to the code that implements it and the tests that assert its observable
behaviour.

Conventions:

- **Code** paths are relative to `gears/mini-chat/mini-chat/src/` (gear) or
  `gears/mini-chat/mini-chat-sdk/src/` (marked `sdk:`), written `file:symbol`.
- **Rust tests** are `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` names relative to
  the crate root (`module::test_name`); SDK tests are marked `sdk:`.
- **Black-box tests** are `testing/mini_chat_blackbox/test_blackbox.py` (real
  `cf-gears-example-server` + scripted provider mock), written `test_blackbox.py::test_name`.
- Tests marked **(T24)** were added by the traceability audit to close a gap; every other test
  listed existed before and was read to confirm that it asserts the criterion (not only touches
  the area).

Status: **all 48 items are covered**. No item required a behaviour change during the audit; the
items listed in the audit summary at the end were only partially asserted before the audit and
received new tests. The final whole-branch review changed the behaviour behind items 6, 18, 20,
33 and 48 (see "Final review fixes" at the end).

## Principles & Constraints

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 1 | Tenant and owner isolation enforced on every resource | `domain/authz.rs:Authz::chat_scope` (PDP scope + `ensure_owner`), `Authz::create_scope`, `Authz::quota_scope`; Secure ORM entities `infra/db/entity/*.rs` (tenant/owner columns); `infra/db/repo/chats.rs:load_scoped` | `api::access_tests::foreign_callers_get_404_from_every_chat_operation_and_change_nothing` (T24); `api::handlers::chats::tests::foreign_and_cross_tenant_chats_are_404`; `api::handlers::chats::tests::list_orders_by_activity_and_paginates` (stranger sees only own chat); `api::handlers::messages::tests::messages_list_odata` (foreign list 404); `api::handlers::turns::tests::turn_status_states` (foreign 404); `api::handlers::reactions::tests::reactions_lifecycle` (stranger 404); `domain::attachment::tests::get_and_delete_rules` (other uploader 404); `domain::attachment::tests::upload_validation_errors` (stranger upload 404); `api::handlers::stream::tests::preflight_rejections_open_no_stream` (same-tenant stranger 404); `domain::turn_service::tests::mutation_guards` (other user 404 / foreign requester 403); `domain::quota::tests::status_endpoint_reports_own_usage` (other user's usage invisible); `domain::authz::tests::owner_constraint_added`; `infra::outbox::chat_cleanup::tests::chat_cleanup_does_not_touch_another_tenants_chat` | Gap closed: one table-driven test runs all 14 chat-scoped operations as a same-tenant user and as another tenant and checks nothing changed. |
| 2 | Context window budget enforced, for both the input message and the full assembled request | `domain/stream/setup.rs:check_input_length` (`INPUT_TOO_LONG`), `domain/stream/setup.rs:assemble_context` → `domain/context.rs:assemble` (`ContextBudgetExceeded`); `domain/quota/estimate.rs:estimate_text_tokens` | `api::handlers::stream::tests::input_too_long_counts_utf8_bytes`; `domain::stream::tests::assembled_request_over_the_downgraded_model_budget_is_rejected` (T24); `domain::context::tests::mandatory_over_budget_is_context_budget_exceeded`; `domain::context::tests::max_input_tokens_caps_the_limit`; `domain::context::tests::max_output_ge_context_window_rejected`; `domain::turn_service::tests::post_commit_setup_failure_marks_turn_failed` (retry path) | Gap closed: the send path's assembled-request rejection (400 `CONTEXT_BUDGET_EXCEEDED`, no provider call/turn/reserve) and "budget computed for the effective model after downgrade" had no end-to-end test. |
| 3 | Streaming responses are never buffered before relaying | `infra/llm/openai_responses.rs:Translator` (event-by-event), `domain/stream/provider_task.rs:run`, `domain/stream/relay.rs:with_pings`, `api/sse.rs:into_sse_response` (`X-Accel-Buffering: no`) | `domain::stream::tests::deltas_are_not_buffered`; `infra::llm::openai_responses::tests::events_are_yielded_before_the_provider_finishes`; `domain::stream::tests::send_streams_contract_in_order` (headers); `api::sse::tests::writes_named_events_and_stops_after_the_terminal_one` | |
| 4 | A chat's model is immutable once set | `domain/chat_service.rs:ChatService::rename` (title only), `api/dto/chats.rs` (`UpdateChatReq` has only `title`); downgrade recorded on `messages.model` only (`domain/stream/finalize.rs:assistant_message`) | `api::handlers::chats::tests::get_patch_delete_lifecycle` (PATCH with `model` keeps the model); `domain::stream::tests::downgraded_turn_is_charged_at_the_effective_model_rate` (T24, chat keeps `gpt-premium` after a downgraded turn); `domain::stream::tests::downgrade_reported_in_done_and_message_model` | |
| 5 | Quota is checked before any outbound provider call | `domain/stream/setup.rs:prepare` → `check_preflight` → `domain/quota/preflight.rs:QuotaService::preflight`, then `setup.rs:reserve` before `domain/stream/mod.rs:StreamService::spawn`; mutations: `setup.rs:prepare_mutation` | `api::handlers::stream::tests::spent_quota_rejects_with_429_and_reserves_nothing`; `api::handlers::stream::tests::preflight_rejections_open_no_stream` (tiny limits → 429, no provider call); `domain::turn_service::tests::quota_rejection_leaves_previous_turn_intact`; `domain::quota::tests::web_search_kill_switch_before_quota`; `domain::stream::tests::reserve_is_booked_while_the_provider_call_runs` (T24) | |

## Chat CRUD

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 6 | Create / get / list / update / delete lifecycle, including model and title validation | `api/handlers/chats.rs:{create_chat,get_chat,list_chats,update_chat,delete_chat}`, `domain/chat_service.rs:{ChatService::create,get,list,rename,delete,validate_title}`, `domain/model_service.rs:ModelService::resolve_for_create`, `api/dto/timestamp.rs` (API timestamps at microsecond precision) | `api::handlers::chats::tests::create_defaults_model_and_returns_location`; `…::api_timestamps_are_microsecond_precision_and_stable_across_reads` (final review); `api::dto::timestamp::tests::stored_and_postgres_read_back_values_serialize_identically` (final review); `…::create_validates_title_and_model`; `…::create_validates_title_before_authorization`; `…::create_json_extractor_errors`; `…::get_patch_delete_lifecycle`; `…::deleted_chats_are_not_listed`; `…::delete_marks_attachments_and_enqueues_chat_cleanup`; `domain::chat_service::tests::title_is_trimmed_and_counted_in_chars`; `domain::model_service::tests::default_model_algorithm`; `test_blackbox.py::test_chat_create_list_get_rename_delete` | |
| 7 | List supports filtering, ordering, pagination, with validation of malformed input | `infra/db/repo/chats.rs:list`, `infra/db/repo/keyset.rs`, `infra/db/repo/odata_time.rs`, `api/dto/chats.rs` (`FilterField`) | `api::handlers::chats::tests::list_orders_by_activity_and_paginates`; `…::datetime_filters_compose_and_survive_cursors`; `…::id_filter_accepts_quoted_and_unquoted_uuids`; `…::title_order_pages_through_untitled_chats_exactly_once`; `…::timestamps_within_one_second_order_and_filter_correctly`; `infra::db::repo::odata_time::tests::literals_are_normalised_and_out_of_range_is_an_error` | |
| 8 | Chat ordering reflects most recent activity | `infra/db/repo/chats.rs:list` (default `updated_at desc, id`), `infra/db/repo/chats.rs:touch_updated_at` (send, retry, edit, delete-turn transactions), `chats.rs:rename` | `api::handlers::chats::tests::list_orders_by_activity_and_paginates` (rename moves a chat first); `api::handlers::chats::tests::timestamps_within_one_second_order_and_filter_correctly` (default order); `domain::stream::tests::send_persists_turn_messages_and_settlement` (send bumps `updated_at`); `domain::turn_service::tests::retry_replaces_latest_turn` and `…::delete_removes_latest_turn` (mutations bump it) | |

## Messages API

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 9 | List messages with filtering, ordering, pagination | `api/handlers/messages.rs:list_messages`, `domain/message_service.rs:MessageService::list`, `infra/db/repo/messages.rs`, `api/dto/messages.rs` (`MessageField`) | `api::handlers::messages::tests::messages_list_odata` | |
| 10 | Message response contract: identity, attachments, reaction fields always present | `api/dto/messages.rs:MiniChatMessageDto` (`From<MessageView>`), `domain/message_service.rs:MessageService::list` | `api::handlers::messages::tests::messages_list_contract_and_counts` (`request_id`, `attachments: []`, `my_reaction: null` on every item); `…::message_attachments_listed_without_deleted`; `domain::message_service::tests::zero_token_counts_are_absent_and_a_missing_request_id_is_internal`; `domain::message_service::tests::thumbnail_only_for_ready_images_that_have_one`; `api::handlers::reactions::tests::reactions_lifecycle` (`my_reaction`) | |
| 11 | Message count and chronological ordering tracked correctly across turns | `infra/db/repo/chats.rs:{message_count,message_counts}` (live messages only), `infra/db/repo/messages.rs` (`created_at, id` order) | `api::handlers::messages::tests::messages_list_contract_and_counts`; `api::handlers::chats::tests::message_count_ignores_soft_deleted_messages`; `domain::turn_service::tests::delete_removes_latest_turn` (count 4 → 2 → 0); `domain::turn_service::tests::retry_replaces_latest_turn` (order after retry); `test_blackbox.py::test_stream_message_events_and_persisted_rows` | |

## Streaming: Send Message

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 12 | Send-message endpoint streams a response, correlated by a request id | `api/handlers/stream.rs:stream_message`, `domain/stream/mod.rs:StreamService::send`, `api/dto/stream.rs:StreamMessageRequest` (server UUID v4 when omitted) | `domain::stream::tests::send_streams_contract_in_order`; `domain::stream::tests::server_generates_request_id_when_omitted`; `api::handlers::messages::tests::messages_list_contract_and_counts` (user + assistant share `request_id`); `test_blackbox.py::test_stream_message_events_and_persisted_rows` | |
| 13 | Preflight validation (content, attachments, limits) runs before any provider call | `domain/stream/setup.rs:{validate_request,attachment_facts,check_preflight,check_images,check_input_length}` | `api::handlers::stream::tests::preflight_rejections_open_no_stream`; `…::vision_rejected_after_downgrade`; `…::input_too_long_counts_utf8_bytes`; `domain::stream::setup::tests::request_validation_order_and_limits`; `domain::turn_service::tests::retry_reapplies_image_guards` | |
| 14 | Assistant message and usage persisted once a stream completes | `domain/stream/finalize.rs:{finalize_turn,commit,assistant_message}` | `domain::stream::tests::send_persists_turn_messages_and_settlement`; `test_blackbox.py::test_stream_message_events_and_persisted_rows` | |

## SSE Event Contract

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 15 | Full event contract (start, delta, tool, citations, completion, error, keepalive) and its ordering | `domain/stream/events.rs:StreamEvent`, `domain/stream/provider_task.rs` (tool/citation mapping, `map_citations`), `domain/stream/relay.rs:with_pings` (`ping` before content), `api/sse.rs:into_sse_response` (30 s `:` comment keep-alive after content), `api/sse.rs:into_sse_event` | `domain::stream::tests::send_streams_contract_in_order`; `domain::stream::tests::tool_events_and_citations_contract`; `domain::stream::tests::relay_pings_only_before_content`; `api::sse::tests::comment_keep_alive_every_30_seconds_while_the_upstream_is_silent` (T24 fix round 1); `domain::stream::events::tests::payloads_match_the_wire_contract`; `api::sse::tests::writes_named_events_and_stops_after_the_terminal_one`; `api::routes::tests::operations_match_published_contract` (SSE schemas); `test_blackbox.py::test_stream_message_events_and_persisted_rows` | |
| 16 | Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers | `domain/stream/finalize.rs:done_payload`, `api/dto/stream.rs:DoneData` | `domain::stream::tests::send_streams_contract_in_order` (no `message_id`/`request_id`/`resp_…` in any frame, T24 assertion added); `domain::stream::tests::downgrade_reported_in_done_and_message_model`; `domain::stream::tests::incomplete_is_completed_without_error_code` (`quota_warnings`); `domain::quota::tests::quota_warning_serializes_for_the_done_event` | |
| 17 | Error event is terminal and carries a sanitized message | `domain/stream/relay.rs:with_pings` (ends after terminal), `api/sse.rs:into_sse_response`, `domain/sanitize.rs:sanitize_provider_message`, `infra/llm/errors.rs:from_http` | `domain::stream::tests::provider_http_errors_map_to_sse_codes`; `domain::stream::tests::relay_ends_after_terminal_event`; `api::sse::tests::body_ends_after_the_terminal_event_while_the_upstream_stays_pending`; `infra::llm::errors::tests::upstream_error_body_is_sanitized_and_context_length_detected` | |

## Idempotency & Replay

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 18 | Replaying a known request id returns the stored result without side effects | `domain/stream/setup.rs:check_turn_slot` (run before model resolution: the replay reads nothing from the policy snapshot), `domain/stream/replay.rs:{load,events}` (separate path, no quota/outbox access) | `domain::stream::tests::replay_is_side_effect_free`; `domain::stream::tests::replay_does_not_depend_on_catalog_or_policy_plugin` (final review: model removed from the catalog, policy plugin failing); `domain::stream::tests::null_content_completion_persists_empty_message` (replay of empty answer); `domain::stream::replay::tests::downgrade_is_rebuilt_from_the_stored_models`; `domain::stream::tests::usage_is_published_once_per_turn_and_redelivered_after_a_transient_failure` (T24, replay publishes nothing); `test_blackbox.py::test_replay_of_a_completed_turn_does_not_call_the_provider` | Replay `done` is rebuilt from stored models (documented deviation, DESIGN "Replay done payload immutability invariant — not implemented as written", ADR-0010). |
| 19 | Conflicting reuse of a request id across turn states is rejected consistently | `domain/stream/setup.rs:check_turn_slot` (`RequestIdConflict`), `infra/db/repo/turns.rs:find_by_request` | `domain::stream::tests::request_id_conflicts` (failed, soft-deleted, running); `domain::stream::tests::cancelled_turn_frees_the_chat_but_not_its_request_id` (T24, cancelled) | Gap closed: the `cancelled` row of the idempotency table had no test. |
| 20 | Replay is checked before the parallel-turn guard | `domain/stream/setup.rs:prepare` (`check_turn_slot` right after authorization and the chat lookup: before model resolution, the running-turn check and the insert) | `domain::stream::tests::replay_checked_before_parallel_guard`; `domain::stream::tests::replay_does_not_depend_on_catalog_or_policy_plugin` (final review) | DESIGN "Check Priority Order": idempotency first, no further checks; DESIGN 4135: replay must not depend on catalog changes. |

## Parallel Turn Enforcement

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 21 | Only one turn may run per chat at a time | `domain/stream/setup.rs:check_turn_slot` (`TurnAlreadyRunning`), unique index `UNIQUE(chat_id) WHERE state='running'` (`infra/db/migrations/m0001_initial.rs`) | `domain::stream::tests::concurrent_sends_one_wins_no_reserve_leak`; `domain::stream::tests::replay_checked_before_parallel_guard`; `infra::db::tests::running_turn_unique_per_chat`; `test_blackbox.py::test_parallel_turn_in_one_chat_is_rejected_with_409` | |
| 22 | A new turn is accepted once the previous one reaches a terminal state | same as 21 (the guard only sees `state='running'`) | `domain::stream::tests::cancelled_turn_frees_the_chat_but_not_its_request_id` (T24, after cancel); `domain::stream::tests::request_id_conflicts` (after failed); `infra::workers::orphan_watchdog::tests::watchdog_unblocks_chat` (after orphan timeout); `domain::turn_service::tests::post_commit_setup_failure_marks_turn_failed` (after setup failure); `domain::stream::tests::provider_request_contains_history_and_message` (after completed) | Gap closed: acceptance after a client cancel had no test. |

## Turn Mutations

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 23 | Retry / edit / delete act only on the latest, terminal turn | `domain/turn_service.rs:{TurnService::preview,retry,edit,delete}`, `infra/db/repo/turns.rs:{latest,soft_delete_terminal}` | `domain::turn_service::tests::mutation_guards` (`NOT_LATEST_TURN`, running → 400 `turn_state/STATE`, unknown 404, foreign 403/404, already deleted) | |
| 24 | A mutation goes through the full send pipeline and gets a new request id | `domain/stream/setup.rs:{prepare_mutation,start_replacement,reserve_replacement}`, `domain/turn_service.rs:commit_mutation` | `domain::turn_service::tests::retry_replaces_latest_turn` (new v4 id, reserve fields, audit); `…::edit_uses_new_content_and_keeps_attachments`; `…::quota_rejection_leaves_previous_turn_intact` (quota); `…::post_commit_setup_failure_marks_turn_failed` (context budget); `…::retry_reapplies_image_guards` (attachment/image checks) | |
| 25 | Concurrent mutations resolve deterministically | `domain/turn_service.rs:commit_mutation` (single tx; `GENERATION_IN_PROGRESS` on the running-turn index) | `domain::turn_service::tests::concurrent_mutations_one_wins` | |
| 26 | Mutated turns carry forward attachment and tool-usage history | `domain/turn_service.rs:commit_mutation` (copies live `message_attachments`), `domain/stream/setup.rs:prepare_mutation` (`web_search_enabled` of the replaced turn, images re-sent) | `domain::turn_service::tests::edit_uses_new_content_and_keeps_attachments`; `domain::turn_service::tests::retry_reapplies_image_guards`; `domain::turn_service::tests::retry_and_edit_reuse_the_web_search_flag` (T24) | Gap closed: reuse of `chat_turns.web_search_enabled` by retry/edit (DESIGN 3.7 / "Web Search Configuration") was implemented but untested. |

## Turn Lifecycle

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 27 | Turn state machine (running → completed / cancelled / failed) consistent end to end | `infra/db/repo/turns.rs:{finalize_running,finalize_orphan}` (CAS `WHERE state='running'`), `domain/stream/finalize.rs:finalize_turn`, `domain/turn_service.rs:TurnService::status` | `api::handlers::turns::tests::turn_status_states`; `domain::stream::tests::cas_lost_emits_stream_interrupted`; `domain::stream::tests::finalization_failure_reports_finalization_failed`; `infra::workers::orphan_watchdog::tests::watchdog_finalizes_seeded_stale_turn`; `…::watchdog_skips_recent_progress`; `infra::db::tests::text_enums_round_trip`; `test_blackbox.py::test_turn_status` | |
| 28 | Partial and null-content cases on cancellation or failure handled correctly | `domain/stream/provider_task.rs:{disconnected,finish}`, `domain/stream/finalize.rs:{commit,retry_without_message}` | `domain::stream::tests::disconnect_mid_stream_cancels_turn_and_settles_estimated`; `…::disconnect_before_any_text_persists_no_message`; `…::null_content_completion_persists_empty_message`; `…::message_persistence_failure_reports_error`; `…::web_search_limit_exceeded_mid_turn` (failed turn has no assistant message); `api::handlers::turns::tests::turn_status_states` (cancelled with partial text) | |

## Attachments

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 29 | Upload / get / delete lifecycle with size, type, per-chat limit validation | `api/handlers/attachments.rs:{upload_attachment,get_attachment,delete_attachment}`, `domain/attachment/upload.rs:{upload,size_limit,normalize_filename}`, `domain/attachment/mime.rs`, `domain/attachment/mod.rs:{AttachmentService::get,delete}` | `domain::attachment::tests::upload_document_ready`; `…::upload_image_with_thumbnail`; `…::upload_xlsx_routes_to_code_interpreter`; `…::upload_validation_errors`; `…::per_chat_limits`; `…::upload_concurrency_limit`; `…::get_and_delete_rules`; `…::long_multibyte_filename_truncated_keeping_extension`; `…::provider_mismatch_conflict`; `domain::attachment::mime::tests::*`; `domain::attachment::upload::tests::*`; `domain::attachment::thumbnail::tests::*`; `test_blackbox.py::test_upload_pdf_and_image` | |
| 30 | Asynchronous indexing lifecycle, including provider failure and timeout | `domain/attachment/indexing.rs` (`wait_in_request`, `spawn_background`, `poll_round`, `fail`, vector store creation protocol) | `domain::attachment::tests::upload_document_indexing_in_progress_then_ready`; `…::upload_indexing_failed_returns_503`; `…::background_indexing_timeout_fails_and_enqueues_cleanup`; `…::background_indexing_failure_enqueues_cleanup`; `…::background_indexing_stops_for_deleted_attachment_and_on_shutdown`; `…::transient_status_errors_keep_polling`; `…::vector_store_creation_race_creates_one_store`; `…::vector_store_creation_failure_fails_the_upload`; `…::stale_placeholder_is_reclaimed`; `…::loser_gives_up_when_the_placeholder_never_fills`; `…::lost_compare_and_set_uses_the_chat_store`; `infra::storage::tests::status_mapping` | |
| 31 | Attachments are made available to the relevant provider tools | `domain/stream/setup.rs:{attachment_facts,assemble_context}`, `domain/tools.rs:build_tools`, `domain/stream/knowledge.rs` | `domain::attachment::tests::ready_attachments_reach_provider_tools`; `domain::stream::tests::tool_events_and_citations_contract`; `domain::stream::tests::code_interpreter_limit_exceeded_mid_turn` (CI files in the request); `domain::stream::knowledge::tests::knowledge_search_loop`; `domain::attachment::secondary::tests::anthropic_secondary_copy_on_image_upload` | |
| 32 | Cleanup and abandoned-upload recovery behave correctly under failure | `infra/outbox/attachment_cleanup.rs:AttachmentCleanupHandler`, `infra/workers/upload_reaper.rs:UploadReaper` | `infra::outbox::attachment_cleanup::tests::*` (12 tests: 404 = done, failure counting to `max_attempts`, recovery after a failed attempt, lost CAS, missing S2S, secondary file); `infra::workers::upload_reaper::tests::reaper_fails_seeded_stale_upload`; `domain::attachment::tests::chat_deleted_during_the_upload_is_not_found` | |

## Models API

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 33 | Read-only model list/get reflects only enabled entries, without internal fields | `api/handlers/models.rs:{list_models,get_model}`, `domain/model_service.rs:{ModelService::list_visible,get_visible}`, `api/dto/models.rs:ModelDto` | `api::handlers::models::tests::list_shows_only_enabled_without_internal_fields`; `…::get_returns_projection_with_description`; `…::get_disabled_or_unknown_is_404_model`; `…::policy_snapshot_failure_is_500`; `…::pdp_deny_is_403_and_failure_503` (403 carries the model resource type, final review); `test_blackbox.py::test_models_list_and_get` | |

## Reactions API

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 34 | Set/remove reaction on assistant messages only, idempotently | `api/handlers/reactions.rs:{put_reaction,delete_reaction}`, `domain/reaction_service.rs:{ReactionService::set,remove,assistant_message_scope}` | `api::handlers::reactions::tests::reactions_lifecycle`; `domain::reaction_service::tests::only_like_and_dislike_parse`; `test_blackbox.py::test_reaction_lifecycle` | |

## Quota Status API

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 35 | Quota status reporting is accurate and consistent with actual usage | `api/handlers/quota.rs:get_quota_status`, `domain/quota/status.rs:{QuotaService::status,warnings,tier_statuses,period_status}` | `domain::stream::tests::quota_status_matches_the_settled_turns` (T24); `domain::quota::tests::status_endpoint_reports_own_usage`; `domain::quota::tests::status_and_warnings`; `domain::quota::tests::status_endpoint_requires_quota_permission`; `test_blackbox.py::test_quota_status_reports_usage` | Gap closed: the Rust tests used seeded rows and the black-box test only checked `used > 0`; the new test compares the endpoint with the credits two real turns settled. |

## Quota Enforcement

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 36 | Reserve-before-execute flow enforced on every provider call | `domain/stream/setup.rs:{reserve,reserve_replacement}` → `domain/quota/ledger.rs:QuotaService::reserve` (re-checks limits), `ledger.rs:QuotaService::settle` | `domain::stream::tests::reserve_is_booked_while_the_provider_call_runs` (T24); `domain::quota::tests::reserve_rechecks_limits`; `domain::quota::tests::concurrent_reserves_never_exceed_the_limit`; `domain::quota::tests::reserve_and_settle_lock_rows_in_the_preflight_order`; `domain::stream::tests::concurrent_sends_one_wins_no_reserve_leak`; `domain::turn_service::tests::retry_replaces_latest_turn` (reserve on the replacement turn) | Gap closed: no test observed the booked reserve while the provider call was in flight. |
| 37 | Tier downgrade applied when a higher tier is exhausted | `domain/quota/preflight.rs:{decide,start_tier,candidate,tier_available}` | `domain::quota::tests::design_example_downgrades_premium`; `…::cascade_rules`; `…::cascade_candidate_selection`; `domain::stream::tests::downgrade_reported_in_done_and_message_model`; `api::handlers::stream::tests::vision_rejected_after_downgrade` | |
| 38 | Credits and tokens are accounted correctly per model and tier | `sdk:credits.rs:credits_micro_checked`, `domain/quota/ledger.rs:{settle,actual_charge,credits}`, `domain/stream/finalize.rs:multipliers` (effective model's multipliers) | `sdk:credits::tests::per_component_ceil_div`; `sdk:credits::tests::bounds`; `domain::quota::tests::settle_actual_estimated_released_and_overshoot`; `domain::stream::tests::send_persists_turn_messages_and_settlement` (premium buckets); `domain::stream::tests::downgraded_turn_is_charged_at_the_effective_model_rate` (T24); `domain::quota::tests::surcharges_follow_candidate_tools`; `domain::quota::estimate::tests::estimate_examples` | Gap closed: no end-to-end test checked that a downgraded turn is charged at the standard model's multipliers on `total` only. |

## Settlement & Finalization

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 39 | Every terminal outcome settles quota exactly once, with actual or estimated usage | `domain/stream/finalize.rs:{finalize_turn,commit,settle_input}` (CAS winner only), `domain/quota/billing.rs:derive_billing`, `infra/workers/orphan_watchdog.rs:OrphanWatchdog::commit` | `domain::quota::billing::tests::billing_derivation_table`; `domain::stream::tests::send_persists_turn_messages_and_settlement` (actual); `…::disconnect_mid_stream_cancels_turn_and_settles_estimated`; `…::provider_http_errors_map_to_sse_codes` (estimated); `…::response_failed_with_usage_settles_actual`; `…::web_search_limit_exceeded_mid_turn`; `…::cas_lost_emits_stream_interrupted` (the loser leaves `quota_usage` and the usage queue unchanged); `…::finalization_failure_reports_finalization_failed`; `infra::workers::orphan_watchdog::tests::*`; `domain::quota::tests::release_beyond_the_booked_reserve_clamps_at_zero` | |
| 40 | Usage is published reliably and exactly once per turn | `domain/stream/finalize.rs:usage_event` (enqueued in the settlement tx, `dedupe_key`), `infra/outbox/mod.rs:OutboxEnqueuer`, `infra/outbox/usage.rs:UsageHandler`, `infra/gateways/policy.rs:PolicyGateway::publish_usage` | `domain::stream::tests::usage_is_published_once_per_turn_and_redelivered_after_a_transient_failure` (T24); `domain::stream::tests::send_persists_turn_messages_and_settlement` (one usage payload, `dedupe_key`); `infra::outbox::usage::tests::usage_handler_publishes_and_classifies`; `infra::db::tx::tests::contended_first_attempt_delivers_exactly_one_outbox_message`; `infra::outbox::tests::enqueued_message_is_delivered_after_commit`; `domain::stream::tests::replay_is_side_effect_free`; `infra::gateways::policy::tests::direct_gateway_reads_current_version_and_maps_errors` | Gap closed: nothing checked end to end that a turn's event reaches the policy plugin after a transient publish failure, once per turn. |

## Context Assembly

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 41 | System prompt, thread summary, recent history assembled and truncated deterministically within budget | `domain/context.rs:assemble`, `domain/stream/setup.rs:{assemble_context,history_message}` | `domain::context::tests::keeps_everything_when_it_fits`; `…::drops_oldest_whole_turns_first` (determinism); `…::leading_assistant_is_dropped`; `…::summary_dropped_when_it_alone_does_not_fit`; `…::images_cost_image_token_budget`; `domain::stream::tests::provider_request_contains_history_and_message`; `infra::outbox::thread_summary::tests::long_chat_gets_summary_and_next_turn_uses_it` | |
| 42 | Tool availability and guidance reflected correctly in the assembled request | `domain/tools.rs:build_tools`, `domain/quota/preflight.rs:tool_gates`, `infra/llm/openai_responses.rs:tool` | `domain::tools::tests::tools_and_guards`; `domain::tools::tests::code_interpreter_has_no_guard_and_is_last`; `domain::attachment::tests::ready_attachments_reach_provider_tools` (tools + guards in `instructions`); `domain::quota::tests::surcharges_follow_candidate_tools`; `infra::llm::openai_responses::tests::request_body_shape`; `infra::llm::openai_chat::tests::openai_chat_translation_request_body_drops_built_in_tools` | |

## Error Mapping & Sanitization

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 43 | All errors map to the canonical error contract, consistently across REST and streaming | `api/error.rs:From<DomainError> for CanonicalError`, `domain/error.rs:DomainError`, `domain/stream/finalize.rs:error` + `domain/stream/setup.rs:setup_failure_code` (SSE codes) | `api::error::tests::maps_every_variant`; `…::retry_after_header_only_on_service_unavailable`; `…::internal_hides_diagnostic`; `…::service_unavailable_detail_is_generic`; `…::db_errors_map_to_domain_errors`; `domain::stream::tests::provider_http_errors_map_to_sse_codes`; `api::access_tests::every_operation_asks_the_pdp_and_fails_closed` (T24, 403/503 on every operation); `api::handlers::chats::tests::create_json_extractor_errors` | |
| 44 | Provider-originated error details sanitized before reaching the client | `domain/sanitize.rs:sanitize_provider_message`, `infra/llm/errors.rs:{from_http,error_fields}`, `infra/storage` failed-status mapping | `domain::sanitize::tests::scrubs_every_provider_id_family_and_bearer_tokens`; `…::scrubs_ids_urls_credentials`; `…::short_file_like_words_survive`; `infra::llm::errors::tests::upstream_error_body_is_sanitized_and_context_length_detected`; `domain::stream::tests::provider_http_errors_map_to_sse_codes`; `infra::storage::tests::failed_status_reason_is_sanitized`; `domain::attachment::tests::upload_indexing_failed_returns_503` | |

## Web Search

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 45 | Web search tool use is reported, cited, accounted, and quota-limited | `domain/stream/provider_task.rs:{start_tool,complete_tool,bump_counter}`, `domain/quota/preflight.rs` (daily tool quota, kill switch), `domain/quota/ledger.rs:settle` (`web_search_calls`) | `domain::stream::tests::tool_events_and_citations_contract` (tool events, url citation, counters on turn / usage / `quota_usage`); `…::web_search_limit_exceeded_mid_turn`; `domain::quota::tests::daily_tool_quotas`; `domain::quota::tests::web_search_kill_switch_before_quota`; `api::handlers::stream::tests::preflight_rejections_open_no_stream` (`disable_web_search`); `domain::turn_service::tests::retry_and_edit_reuse_the_web_search_flag` (T24) | |

## Cleanup & Recovery

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 46 | Chat deletion triggers reliable background cleanup of provider-side resources | `domain/chat_service.rs:ChatService::delete` (`mark_attachments_cleanup_pending` + `chat_cleanup` outbox in one tx), `infra/outbox/chat_cleanup.rs:ChatCleanupHandler` | `api::handlers::chats::tests::delete_marks_attachments_and_enqueues_chat_cleanup`; `infra::outbox::chat_cleanup::tests::*` (12 tests: end to end, waits for attachments before the vector store, retry then reject, missing store, placeholders, secondary files, metrics once); `test_blackbox.py::test_delete_chat_deletes_the_vector_store` | |
| 47 | Thread summary generation, failure/retry, and mutation-driven invalidation | `domain/thread_summary.rs:{evaluate_trigger,enqueue_if_needed,fit,parse_summary}`, `infra/outbox/thread_summary.rs:ThreadSummaryHandler`, `domain/turn_service.rs:drop_covering_summary`, `infra/db/repo/thread_summaries.rs` (frontier CAS) | `infra::outbox::thread_summary::tests::long_chat_gets_summary_and_next_turn_uses_it`; `…::existing_summary_is_merged_and_frontier_advanced`; `…::summary_provider_failure_keeps_previous_state`; `…::context_length_error_retries_with_fewer_messages`; `…::frontier_deleted_skips_commit`; `…::missing_s2s_context_defers_without_dead_lettering`; `…::disabled_summary_model_rejected`; `domain::thread_summary::tests::*`; `domain::turn_service::tests::mutation_of_summarized_turn_drops_summary`; `infra::db::repo::thread_summaries::tests::compare_and_set_requires_the_stored_frontier` | |

## Authorization

| # | Criterion | Implementing code | Tests | Notes |
|---|-----------|-------------------|-------|-------|
| 48 | Every operation scoped to its owner; cross-tenant or foreign access rejected consistently | `domain/authz.rs:{Authz,ChatAction}` (per-operation actions of DESIGN 3.8), every handler's service call (`chat_scope` / `create_scope` / `model_permission` / `quota_scope`), `api/error.rs` (403 `AUTHZ_DENIED` — model operations with the model resource type via `DomainError::ModelAccessDenied` — 503 `Retry-After: 5`) | `api::access_tests::every_operation_asks_the_pdp_and_fails_closed` (T24: all 19 operations, action / resource type / `resource.id` per the DESIGN matrix, deny → 403, PDP failure → 503); `api::access_tests::allowed_operations_evaluate_only_their_own_action` (T24 fix round 1: with the PDP allowing, each operation makes exactly one evaluation, its own action; `messages:stream` has no `read`, retry/edit are evaluated once); `api::access_tests::foreign_callers_get_404_from_every_chat_operation_and_change_nothing` (T24); `domain::authz::tests::*` (6 tests); `api::handlers::models::tests::pdp_deny_is_403_and_failure_503`; `api::handlers::chats::tests::create_validates_title_before_authorization`; `domain::quota::tests::status_endpoint_requires_quota_permission` | Gap closed: before the audit only chats, models and quota status had deny/failure tests, and no test checked the action each operation sends to the PDP. |

## Audit summary

| Item | Gap found | New test(s) | Behaviour change |
|------|-----------|-------------|------------------|
| 1, 48 | No operation-wide isolation / PDP fail-closed check | `api::access_tests::every_operation_asks_the_pdp_and_fails_closed`, `api::access_tests::allowed_operations_evaluate_only_their_own_action`, `api::access_tests::foreign_callers_get_404_from_every_chat_operation_and_change_nothing` | none (passed first run) |
| 15 | The 30 s SSE comment keep-alive after content had no test | `api::sse::tests::comment_keep_alive_every_30_seconds_while_the_upstream_is_silent` | none (passed first run) |
| 39 | The CAS loser's "no settlement" was checked on the usage queue only | `quota_usage` unchanged assertion added to `domain::stream::tests::cas_lost_emits_stream_interrupted` | none |
| 2 | Send-path assembled-request budget after downgrade | `domain::stream::tests::assembled_request_over_the_downgraded_model_budget_is_rejected` | none (passed first run) |
| 16 | `done` provider-id leak only checked for some keys | assertion added to `domain::stream::tests::send_streams_contract_in_order` | none |
| 19, 22 | `cancelled` request id conflict; new turn after cancel | `domain::stream::tests::cancelled_turn_frees_the_chat_but_not_its_request_id` | none (passed first run) |
| 26, 45 | Retry/edit reuse of `web_search_enabled` | `domain::turn_service::tests::retry_and_edit_reuse_the_web_search_flag` | none (passed first run) |
| 35 | Status vs real settled usage | `domain::stream::tests::quota_status_matches_the_settled_turns` | none (passed first run) |
| 36 | Reserve booked during the provider call | `domain::stream::tests::reserve_is_booked_while_the_provider_call_runs` | none (passed first run) |
| 4, 38 | Downgraded turn charged at the effective model's rate, chat model unchanged | `domain::stream::tests::downgraded_turn_is_charged_at_the_effective_model_rate` | none (passed first run) |
| 40 | Reliable once-per-turn publication to the policy plugin | `domain::stream::tests::usage_is_published_once_per_turn_and_redelivered_after_a_transient_failure` | none (passed first run) |

Every new test that passed on its first run was checked against a deliberate regression (an
action name changed in `domain/authz.rs`, the mandatory-items budget check disabled in
`domain/context.rs`, retry/edit forced to `web_search_enabled = false` in
`domain/stream/setup.rs`, the SSE keep-alive removed from `api/sse.rs`, an extra `read`
evaluation added to the send path) and failed as expected; the regressions were reverted.

Documentation note: DESIGN 3.8 says the stream service "evaluates `send_message` again" after
the handler; the implementation evaluates `send_message` exactly once per send (the scope is
reused), which `allowed_operations_evaluate_only_their_own_action` pins. Both satisfy the
normative rule (`send_message` only, no `read`).

## Final review fixes

| Item | Finding | Change | Test(s) |
|------|---------|--------|---------|
| 18, 20 | Replay of a completed turn ran after model resolution, so it failed with `INVALID_MODEL` / 500 when the model left the catalog or the policy plugin was down | `domain/stream/setup.rs:prepare` runs `check_turn_slot` before `resolve_chat_model` | `domain::stream::tests::replay_does_not_depend_on_catalog_or_policy_plugin` |
| 6 | Every API timestamp ended in `…001Z` (storage artefact of `infra/db/ts.rs:normalize`); on PostgreSQL a create response could differ by 1 ns from a later read | `api/dto/timestamp.rs`: every DTO timestamp is serialized truncated to microseconds; storage format, cursors and `$filter` binding unchanged | `api::handlers::chats::tests::api_timestamps_are_microsecond_precision_and_stable_across_reads`, `api::dto::timestamp::tests::*`; `api::handlers::turns::tests::turn_status_states` and `api::handlers::reactions::tests::reactions_lifecycle` now compare the API value with the stored value truncated to microseconds |
| 33, 48 | A PDP denial of `list_models` / `get_model` reported the chat resource type | `DomainError::ModelAccessDenied` from `Authz::model_permission`, rendered with `gts.cf.core.mini_chat.model.v1~` | `api::handlers::models::tests::pdp_deny_is_403_and_failure_503`, `domain::authz::tests::deny_is_403`, `api::error::tests` mapping table |
| — (config) | Provider entries / tenant overrides sharing an OAGW alias with different host, port, scheme or auth silently shared one upstream | `config/providers.rs:validate_alias_collisions` (config error naming both entries); `infra/llm/provisioning.rs:warn_on_drift` warns when an existing upstream differs | `config::tests::upstream_alias_collision_with_different_settings_is_rejected`, `config::tests::upstream_alias_shared_with_identical_settings_is_valid`, `infra::llm::provisioning::tests::already_exists_with_different_settings_is_reused_with_a_warning` |
| — (config) | A `url_prefix` without a leading `/`, with a trailing `/` or with route syntax panicked in route registration | `config/mod.rs:validate_url_prefix` | `config::tests::url_prefix_must_be_a_plain_absolute_path` |
| — (operations) | Provider failure causes were dropped | `infra/llm/errors.rs` / `transport.rs` log gateway, body-read and mid-stream transport errors (credentials scrubbed) | `infra::llm::transport::tests::*`, `infra::llm::errors::tests::logged_causes_are_scrubbed_and_bounded` |
