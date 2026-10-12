#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use uuid::Uuid;

use super::*;

fn web(url: Option<&str>, span: Option<(usize, usize)>) -> LlmAnnotation {
    LlmAnnotation {
        kind: LlmAnnotationKind::Url,
        url: url.map(str::to_owned),
        title: Some("Example".to_owned()),
        file_id: None,
        filename: None,
        start: span.map(|(start, _)| start),
        end: span.map(|(_, end)| end),
        snippet: "Paris".to_owned(),
    }
}

fn file(file_id: &str) -> LlmAnnotation {
    LlmAnnotation {
        kind: LlmAnnotationKind::File,
        url: None,
        title: None,
        file_id: Some(file_id.to_owned()),
        filename: Some("provider-name.pdf".to_owned()),
        start: None,
        end: None,
        snippet: String::new(),
    }
}

#[test]
fn web_citations_keep_url_title_snippet_and_span() {
    let citations = map_citations(
        &[
            web(Some("https://example.com/a"), Some((0, 5))),
            web(Some("https://example.com/b"), None),
        ],
        &HashMap::new(),
    );

    assert_eq!(
        citations,
        vec![
            Citation {
                source: CitationSource::Web,
                title: "Example".to_owned(),
                url: Some("https://example.com/a".to_owned()),
                attachment_id: None,
                snippet: "Paris".to_owned(),
                span: Some(TextSpan { start: 0, end: 5 }),
            },
            Citation {
                source: CitationSource::Web,
                title: "Example".to_owned(),
                url: Some("https://example.com/b".to_owned()),
                attachment_id: None,
                snippet: "Paris".to_owned(),
                span: None,
            },
        ]
    );
}

#[test]
fn file_citations_resolve_to_attachments_and_unknown_files_are_omitted() {
    let attachment_id = Uuid::new_v4();
    let files = HashMap::from([(
        "file-known123456789".to_owned(),
        CitedFile {
            attachment_id,
            filename: "Q3 Report.pdf".to_owned(),
        },
    )]);

    let citations = map_citations(
        &[
            file("file-known123456789"),
            file("file-unknown12345678"),
            web(None, None),
        ],
        &files,
    );

    assert_eq!(
        citations,
        vec![Citation {
            source: CitationSource::File,
            title: "Q3 Report.pdf".to_owned(),
            url: None,
            attachment_id: Some(attachment_id),
            snippet: String::new(),
            span: None,
        }]
    );
}

#[test]
fn failure_error_keeps_the_code_and_names_the_retry_delay() {
    let provider = LlmFailure::provider_error("Upstream failure [provider_id]");
    assert_eq!(
        failure_error(&provider),
        (
            "provider_error".to_owned(),
            "Upstream failure [provider_id]".to_owned()
        )
    );

    let mut limited = LlmFailure::new(LlmFailureKind::RateLimited, "Too many requests");
    limited.retry_after_secs = Some(7);
    let (code, message) = failure_error(&limited);
    assert_eq!(code, "rate_limited");
    assert!(message.contains("7 seconds"), "{message}");

    let mut named = LlmFailure::new(LlmFailureKind::RateLimited, "Retry after 7s");
    named.retry_after_secs = Some(7);
    assert_eq!(failure_error(&named).1, "Retry after 7s");

    let timeout = LlmFailure::new(LlmFailureKind::ProviderTimeout, "timed out");
    assert_eq!(failure_error(&timeout).0, "provider_timeout");
}

#[test]
fn completions_count_per_tool() {
    let mut counts = ToolCallCounts::default();

    assert!(count_completion(TOOL_WEB_SEARCH, &mut counts));
    assert!(count_completion(TOOL_WEB_SEARCH, &mut counts));
    assert!(count_completion(TOOL_CODE_INTERPRETER, &mut counts));
    assert!(count_completion(TOOL_FILE_SEARCH, &mut counts));
    assert!(!count_completion("function_call", &mut counts));

    assert_eq!(
        counts,
        ToolCallCounts {
            web_search: 2,
            code_interpreter: 1,
            file_search: 1,
        }
    );
}
