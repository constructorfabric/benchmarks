//! Citation mapping: web annotations with span and snippet rules (annotation text, answer text
//! in the range as character offsets, out-of-range span), file annotations resolved through the
//! provider-file-id map (unknown ids omitted), no score and no provider identifier, nothing for
//! an incomplete response, one payload for all items.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `citations`; definition of done `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use super::citations::{FileCitationMap, FileRef, map_citations};
use super::provider_events::{Annotation, OutputPart, ProviderCompletion};
use super::relay::{CitationItem, CitationSource, TextSpan};

fn completion(parts: Vec<OutputPart>) -> ProviderCompletion {
    ProviderCompletion {
        usage: None,
        response_id: Some("resp_0aB1c2D3e4F5g6H7".to_owned()),
        incomplete_reason: None,
        parts,
        tool_calls: Vec::new(),
    }
}

fn url(url: &str, title: &str, start: Option<usize>, end: Option<usize>) -> Annotation {
    Annotation::Url {
        url: url.to_owned(),
        title: Some(title.to_owned()),
        start_index: start,
        end_index: end,
        text: None,
    }
}

fn part(text: &str, annotations: Vec<Annotation>) -> OutputPart {
    OutputPart {
        text: text.to_owned(),
        annotations,
    }
}

fn items(completion: &ProviderCompletion, files: &FileCitationMap) -> Vec<CitationItem> {
    map_citations(completion, files)
        .expect("a citations payload")
        .items
}

#[test]
fn a_web_annotation_with_a_range_has_a_span_and_the_answer_text_as_snippet() {
    let completion = completion(vec![part(
        "Growth was strong in 2025 according to analysts.",
        vec![url(
            "https://example.com/m",
            "Market Analysis",
            Some(7),
            Some(18),
        )],
    )]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(
        items,
        vec![CitationItem {
            source: CitationSource::Web,
            title: "Market Analysis".to_owned(),
            url: Some("https://example.com/m".to_owned()),
            attachment_id: None,
            snippet: "was strong ".to_owned(),
            span: Some(TextSpan { start: 7, end: 18 }),
        }]
    );
}

#[test]
fn the_annotation_text_wins_over_the_answer_text_as_snippet() {
    let completion = completion(vec![part(
        "abcdefghij",
        vec![Annotation::Url {
            url: "https://example.com/a".to_owned(),
            title: Some("A".to_owned()),
            start_index: Some(0),
            end_index: Some(3),
            text: Some("the provider excerpt".to_owned()),
        }],
    )]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(items[0].snippet, "the provider excerpt");
    assert_eq!(items[0].span, Some(TextSpan { start: 0, end: 3 }));
}

#[test]
fn offsets_are_characters_not_bytes() {
    let completion = completion(vec![part(
        "caf\u{e9} cr\u{e8}me br\u{fb}l\u{e9}e",
        vec![url("https://example.com/c", "C", Some(5), Some(10))],
    )]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(items[0].snippet, "cr\u{e8}me");
}

#[test]
fn an_out_of_range_span_gives_an_empty_snippet_and_keeps_the_span() {
    let completion = completion(vec![part(
        "short",
        vec![url("https://example.com/s", "S", Some(10), Some(20))],
    )]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(items[0].snippet, "");
    assert_eq!(items[0].span, Some(TextSpan { start: 10, end: 20 }));
}

#[test]
fn a_web_annotation_without_both_indexes_has_no_span_and_no_snippet_from_the_text() {
    let completion = completion(vec![part(
        "some answer text",
        vec![
            url("https://example.com/1", "One", Some(1), None),
            url("https://example.com/2", "Two", None, Some(4)),
            url("https://example.com/3", "Three", None, None),
        ],
    )]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(items.len(), 3);
    for item in &items {
        assert_eq!(item.span, None, "{item:?}");
        assert_eq!(item.snippet, "", "{item:?}");
        assert_eq!(item.source, CitationSource::Web);
    }
}

#[test]
fn the_range_is_applied_to_the_part_that_carries_the_annotation() {
    let completion = completion(vec![
        part("first part, no citations", vec![]),
        part(
            "0123456789",
            vec![url("https://example.com/p", "P", Some(2), Some(5))],
        ),
    ]);
    let items = items(&completion, &FileCitationMap::new());
    assert_eq!(items[0].snippet, "234");
}

#[test]
fn a_file_annotation_resolves_through_the_map_to_the_attachment_and_its_filename() {
    let attachment = Uuid::from_u128(0xA77A);
    let mut files = FileCitationMap::new();
    files.insert(
        "file-AbC123".to_owned(),
        FileRef {
            attachment_id: attachment,
            filename: "Q3 Report.pdf".to_owned(),
        },
    );
    let completion = completion(vec![part(
        "see the report",
        vec![Annotation::File {
            file_id: "file-AbC123".to_owned(),
            filename: Some("provider-side-name.pdf".to_owned()),
        }],
    )]);
    let items = items(&completion, &files);
    assert_eq!(
        items,
        vec![CitationItem {
            source: CitationSource::File,
            title: "Q3 Report.pdf".to_owned(),
            url: None,
            attachment_id: Some(attachment),
            snippet: String::new(),
            span: None,
        }]
    );
}

#[test]
fn an_unknown_or_deleted_file_is_omitted_and_an_empty_result_is_none() {
    let completion = completion(vec![part(
        "text",
        vec![Annotation::File {
            file_id: "file-unknown000".to_owned(),
            filename: Some("x.pdf".to_owned()),
        }],
    )]);
    assert_eq!(map_citations(&completion, &FileCitationMap::new()), None);
}

#[test]
fn a_response_without_annotations_has_no_citations() {
    let completion = completion(vec![part("plain", vec![])]);
    assert_eq!(map_citations(&completion, &FileCitationMap::new()), None);
    assert_eq!(
        map_citations(&self::completion(vec![]), &FileCitationMap::new()),
        None
    );
}

#[test]
fn all_items_of_all_parts_are_one_payload_in_order() {
    let mut files = FileCitationMap::new();
    files.insert(
        "file-AbC123".to_owned(),
        FileRef {
            attachment_id: Uuid::from_u128(1),
            filename: "a.pdf".to_owned(),
        },
    );
    let completion = completion(vec![
        part(
            "alpha beta",
            vec![url("https://example.com/1", "One", Some(0), Some(5))],
        ),
        part(
            "gamma",
            vec![
                Annotation::File {
                    file_id: "file-AbC123".to_owned(),
                    filename: None,
                },
                url("https://example.com/2", "Two", Some(0), Some(5)),
            ],
        ),
    ]);
    let payload = map_citations(&completion, &files).unwrap();
    let titles: Vec<&str> = payload
        .items
        .iter()
        .map(|item| item.title.as_str())
        .collect();
    assert_eq!(titles, vec!["One", "a.pdf", "Two"]);
}

#[test]
fn an_incomplete_response_maps_no_citations() {
    let mut incomplete = completion(vec![part(
        "text",
        vec![url("https://example.com/1", "One", Some(0), Some(2))],
    )]);
    incomplete.incomplete_reason = Some("max_output_tokens".to_owned());
    assert_eq!(map_citations(&incomplete, &FileCitationMap::new()), None);
}

#[test]
fn no_provider_identifier_is_exposed() {
    let mut files = FileCitationMap::new();
    files.insert(
        "file-AbC123".to_owned(),
        FileRef {
            attachment_id: Uuid::from_u128(9),
            filename: "a.pdf".to_owned(),
        },
    );
    let completion = completion(vec![part(
        "text",
        vec![
            Annotation::File {
                file_id: "file-AbC123".to_owned(),
                filename: Some("a.pdf".to_owned()),
            },
            url("https://example.com/1", "One", Some(0), Some(2)),
        ],
    )]);
    let payload = format!("{:?}", map_citations(&completion, &files).unwrap());
    for hidden in ["file-AbC123", "resp_0aB1c2D3e4F5g6H7", "score"] {
        assert!(!payload.contains(hidden), "`{hidden}` in {payload}");
    }
}
