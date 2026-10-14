//! REST API of the gear. DTOs live only here (lints DE0201 and DE0202).

pub mod dto;
pub mod error;
pub mod handlers;
pub mod routes;
pub mod stream;

pub use routes::register_routes;

#[cfg(test)]
#[path = "rest/license_gate_tests.rs"]
mod license_gate_tests;

#[cfg(test)]
#[path = "rest/chat_routes_tests.rs"]
mod chat_routes_tests;

#[cfg(test)]
#[path = "rest/chat_create_tests.rs"]
mod chat_create_tests;

#[cfg(test)]
#[path = "rest/chat_read_tests.rs"]
mod chat_read_tests;

#[cfg(test)]
#[path = "rest/chat_list_tests.rs"]
mod chat_list_tests;

#[cfg(test)]
#[path = "rest/chat_page_keys_tests.rs"]
mod chat_page_keys_tests;

#[cfg(test)]
#[path = "rest/chat_update_tests.rs"]
mod chat_update_tests;

#[cfg(test)]
#[path = "rest/chat_delete_tests.rs"]
mod chat_delete_tests;

#[cfg(test)]
#[path = "rest/chat_isolation_tests.rs"]
mod chat_isolation_tests;

#[cfg(test)]
#[path = "rest/chat_dto_tests.rs"]
mod chat_dto_tests;

#[cfg(test)]
#[path = "rest/quota_status_tests.rs"]
mod quota_status_tests;

#[cfg(test)]
#[path = "rest/quota_error_tests.rs"]
mod quota_error_tests;

#[cfg(test)]
#[path = "rest/quota_dto_tests.rs"]
mod quota_dto_tests;

#[cfg(test)]
#[path = "rest/quota_status_gear_tests.rs"]
mod quota_status_gear_tests;

#[cfg(test)]
#[path = "rest/message_list_tests.rs"]
mod message_list_tests;

#[cfg(test)]
#[path = "rest/turn_status_tests.rs"]
mod turn_status_tests;

#[cfg(test)]
#[path = "rest/send_validation_tests.rs"]
mod send_validation_tests;

#[cfg(test)]
#[path = "rest/send_flow_tests.rs"]
mod send_flow_tests;

#[cfg(test)]
#[path = "rest/send_finalization_tests.rs"]
mod send_finalization_tests;

#[cfg(test)]
#[path = "rest/send_error_contract_tests.rs"]
mod send_error_contract_tests;

#[cfg(test)]
#[path = "rest/message_routes_tests.rs"]
mod message_routes_tests;

#[cfg(test)]
#[path = "rest/stream_relay_tests.rs"]
mod stream_relay_tests;

#[cfg(test)]
#[path = "rest/stream_recovery_tests.rs"]
mod stream_recovery_tests;

#[cfg(test)]
#[path = "rest/web_search_turn_tests.rs"]
mod web_search_turn_tests;

#[cfg(test)]
#[path = "rest/message_count_turns_tests.rs"]
mod message_count_turns_tests;

#[cfg(test)]
#[path = "rest/deferred_scope_tests.rs"]
mod deferred_scope_tests;

// Attachments and file search (FEATURE 0005), slice S1-upload-lifecycle: tests of the upload
// and get routes.
#[cfg(test)]
#[path = "rest/attachment_error_tests.rs"]
mod attachment_error_tests;

#[cfg(test)]
#[path = "rest/attachment_dto_tests.rs"]
mod attachment_dto_tests;

#[cfg(test)]
#[path = "rest/attachment_upload_tests.rs"]
mod attachment_upload_tests;

#[cfg(test)]
#[path = "rest/attachment_intake_tests.rs"]
mod attachment_intake_tests;

#[cfg(test)]
#[path = "rest/attachment_limits_tests.rs"]
mod attachment_limits_tests;

#[cfg(test)]
#[path = "rest/attachment_indexing_tests.rs"]
mod attachment_indexing_tests;

#[cfg(test)]
#[path = "rest/attachment_image_tests.rs"]
mod attachment_image_tests;

#[cfg(test)]
#[path = "rest/attachment_get_tests.rs"]
mod attachment_get_tests;

#[cfg(test)]
#[path = "rest/attachment_permit_tests.rs"]
mod attachment_permit_tests;

#[cfg(test)]
#[path = "rest/attachment_vector_store_tests.rs"]
mod attachment_vector_store_tests;

// Attachments and file search (FEATURE 0005), slice S2-deletion-cleanup-tools: tests of the
// delete route and of the tool contributions over the send route.
#[cfg(test)]
#[path = "rest/attachment_delete_tests.rs"]
mod attachment_delete_tests;

#[cfg(test)]
#[path = "rest/attachment_tools_sse_tests.rs"]
mod attachment_tools_sse_tests;

#[cfg(test)]
#[path = "rest/knowledge_loop_tests.rs"]
mod knowledge_loop_tests;

#[cfg(test)]
#[path = "rest/attachment_deferred_scope_tests.rs"]
mod attachment_deferred_scope_tests;

// Turn mutations and reactions (FEATURE 0006), slice S1-all: the retry, edit and delete
// endpoints, the reaction endpoints and the contracts around them.
#[cfg(test)]
#[path = "rest/turn_retry_tests.rs"]
mod turn_retry_tests;

#[cfg(test)]
#[path = "rest/turn_edit_tests.rs"]
mod turn_edit_tests;

#[cfg(test)]
#[path = "rest/turn_delete_tests.rs"]
mod turn_delete_tests;

#[cfg(test)]
#[path = "rest/turn_setup_failure_tests.rs"]
mod turn_setup_failure_tests;

#[cfg(test)]
#[path = "rest/turn_carry_forward_tests.rs"]
mod turn_carry_forward_tests;

#[cfg(test)]
#[path = "rest/turn_concurrency_tests.rs"]
mod turn_concurrency_tests;

#[cfg(test)]
#[path = "rest/reaction_tests.rs"]
mod reaction_tests;

#[cfg(test)]
#[path = "rest/mutation_error_tests.rs"]
mod mutation_error_tests;
