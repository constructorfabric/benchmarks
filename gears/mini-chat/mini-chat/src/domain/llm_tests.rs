use super::{
    LlmEvent, LlmFailure, LlmFailureKind, LlmRequestType, LlmTool, LlmUsage, feature_label,
    user_field,
};

fn file_search() -> LlmTool {
    LlmTool::FileSearch {
        vector_store_id: "vs_1".to_owned(),
        max_num_results: 5,
    }
}

fn web_search() -> LlmTool {
    LlmTool::WebSearch {
        search_context_size: "low".to_owned(),
    }
}

fn code_interpreter() -> LlmTool {
    LlmTool::CodeInterpreter { file_ids: vec![] }
}

#[test]
fn user_field_is_64_hex_characters_for_uuids() {
    let user = user_field(
        "a0000000-0000-4000-8000-00000000000a",
        "A1000000-0000-4000-8000-0000000000A1",
    );
    assert_eq!(
        user,
        "a000000000004000800000000000000aa10000000000400080000000000000a1"
    );
    assert_eq!(user.len(), 64);
}

#[test]
fn user_field_falls_back_for_non_uuid_ids() {
    assert_eq!(user_field("tenant-x", "user-y"), "tenant-x:user-y");
    assert_eq!(
        user_field("a0000000-0000-4000-8000-00000000000a", "system"),
        "a0000000-0000-4000-8000-00000000000a:system"
    );
}

#[test]
fn feature_label_lists_tools_in_fixed_order() {
    assert_eq!(feature_label(&[]), "none");
    assert_eq!(feature_label(&[web_search()]), "web_search");
    assert_eq!(
        feature_label(&[web_search(), file_search()]),
        "file_search+web_search"
    );
    assert_eq!(
        feature_label(&[code_interpreter(), file_search()]),
        "file_search+code_interpreter"
    );
    assert_eq!(
        feature_label(&[
            code_interpreter(),
            web_search(),
            file_search(),
            web_search()
        ]),
        "file_search+web_search+code_interpreter"
    );
}

#[test]
fn request_type_and_failure_codes() {
    assert_eq!(LlmRequestType::Chat.as_str(), "chat");
    assert_eq!(LlmRequestType::Summary.as_str(), "summary");
    assert_eq!(LlmRequestType::DocSummary.as_str(), "doc_summary");
    assert_eq!(LlmFailureKind::ProviderError.code(), "provider_error");
    assert_eq!(LlmFailureKind::ProviderTimeout.code(), "provider_timeout");
    assert_eq!(LlmFailureKind::RateLimited.code(), "rate_limited");
    let failure = LlmFailure::provider_error("boom");
    assert_eq!(failure.to_string(), "provider_error: boom");
    assert!(failure.request_sent);
    assert!(LlmFailure::new(LlmFailureKind::RateLimited, "slow").request_sent);
}

#[test]
fn not_sent_failure_is_a_provider_error_marked_not_sent() {
    let failure = LlmFailure::not_sent("could not build");
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert_eq!(failure.message_sanitized, "could not build");
    assert!(!failure.request_sent);
    assert_eq!(failure.usage, None);
}

#[test]
fn terminal_events_and_known_usage() {
    assert!(LlmEvent::Failed(LlmFailure::provider_error("x")).is_terminal());
    assert!(
        LlmEvent::Completed {
            response_id: None,
            usage: None,
            incomplete_reason: None,
            output_text: String::new(),
        }
        .is_terminal()
    );
    assert!(!LlmEvent::TextDelta("a".to_owned()).is_terminal());
    assert!(!LlmUsage::default().is_known());
    assert!(
        LlmUsage {
            output_tokens: 1,
            ..LlmUsage::default()
        }
        .is_known()
    );
}
