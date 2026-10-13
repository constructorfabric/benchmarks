#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::non_ascii_literal)]
//! Slice 4: citation mapping
//! (`cpt-cf-mini-chat-algo-streaming-turns-map-citations`).

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::ports::CitationTarget;
use crate::domain::service::stream::types::CitationSource;

const FILE_ATTACHMENT: Uuid = Uuid::from_u128(0x1001);

fn map() -> CitationMap {
    let mut map = CitationMap::new();
    map.insert(
        "file-doc1".to_owned(),
        CitationTarget {
            attachment_id: FILE_ATTACHMENT,
            filename: "report.pdf".to_owned(),
        },
    );
    map
}

fn annotation(value: Value) -> Annotation {
    Annotation {
        value,
        item_id: Some("msg_1".to_owned()),
        content_index: Some(0),
        part_text: None,
    }
}

fn parts(text: &str) -> OutputParts {
    let mut parts = OutputParts::new();
    parts.insert((Some("msg_1".to_owned()), Some(0)), text.to_owned());
    parts
}

#[test]
fn a_web_annotation_with_text_and_span_is_a_web_item() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation",
            "url": "https://example.org/a",
            "title": "Example",
            "text": "quoted text",
            "start_index": 4,
            "end_index": 9
        }))],
        &OutputParts::new(),
        &CitationMap::new(),
    );
    assert_eq!(
        items,
        vec![CitationItem {
            source: CitationSource::Web,
            title: "Example".to_owned(),
            snippet: "quoted text".to_owned(),
            url: Some("https://example.org/a".to_owned()),
            attachment_id: None,
            span: Some((4, 9)),
        }]
    );
}

#[test]
fn the_snippet_falls_back_to_the_characters_of_the_output_part() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation", "url": "https://e.org", "title": "E",
            "start_index": 6, "end_index": 11
        }))],
        &parts("Hello world and more"),
        &CitationMap::new(),
    );
    assert_eq!(items[0].snippet, "world");
    assert_eq!(items[0].span, Some((6, 11)));
}

#[test]
fn an_empty_annotation_text_also_falls_back_to_the_part() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation", "url": "https://e.org", "title": "E", "text": "",
            "start_index": 0, "end_index": 5
        }))],
        &parts("Hello world"),
        &CitationMap::new(),
    );
    assert_eq!(items[0].snippet, "Hello");
}

#[test]
fn offsets_are_character_offsets() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation", "url": "https://e.org", "title": "E",
            "start_index": 6, "end_index": 11
        }))],
        &parts("héllo wörld!"),
        &CitationMap::new(),
    );
    assert_eq!(items[0].snippet, "wörld");
}

#[test]
fn a_range_outside_the_text_gives_an_empty_snippet() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation", "url": "https://e.org", "title": "E",
            "start_index": 50, "end_index": 60
        }))],
        &parts("short"),
        &CitationMap::new(),
    );
    assert_eq!(items[0].snippet, "");
    assert_eq!(items[0].span, Some((50, 60)), "the span is still reported");
}

#[test]
fn a_missing_part_gives_an_empty_snippet() {
    let items = map_citations(
        &[annotation(json!({
            "type": "url_citation", "url": "https://e.org", "title": "E",
            "start_index": 0, "end_index": 3
        }))],
        &OutputParts::new(),
        &CitationMap::new(),
    );
    assert_eq!(items[0].snippet, "");
}

#[test]
fn the_span_needs_both_indexes() {
    for value in [
        json!({"type": "url_citation", "url": "u", "title": "t", "start_index": 1}),
        json!({"type": "url_citation", "url": "u", "title": "t", "end_index": 3}),
        json!({"type": "url_citation", "url": "u", "title": "t"}),
    ] {
        let items = map_citations(&[annotation(value)], &parts("abcdef"), &CitationMap::new());
        assert_eq!(items[0].span, None);
    }
}

#[test]
fn annotations_of_the_completed_payload_carry_their_part_text() {
    let mut carried = annotation(json!({
        "type": "url_citation", "url": "https://e.org", "title": "E",
        "start_index": 0, "end_index": 4
    }));
    carried.part_text = Some("Text of the part".to_owned());
    carried.item_id = None;
    let items = map_citations(&[carried], &OutputParts::new(), &CitationMap::new());
    assert_eq!(items[0].snippet, "Text");
}

#[test]
fn anthropic_web_search_result_locations_are_web_items() {
    let items = map_citations(
        &[annotation(json!({
            "type": "web_search_result_location",
            "url": "https://e.org/p", "title": "P", "text": "cited"
        }))],
        &OutputParts::new(),
        &CitationMap::new(),
    );
    assert_eq!(items[0].source, CitationSource::Web);
    assert_eq!(items[0].url.as_deref(), Some("https://e.org/p"));
    assert_eq!(items[0].snippet, "cited");
}

#[test]
fn a_file_annotation_of_a_known_file_is_a_file_item_with_the_attachment_id() {
    let items = map_citations(
        &[annotation(json!({
            "type": "file_citation", "file_id": "file-doc1", "filename": "provider-name.pdf", "index": 3
        }))],
        &OutputParts::new(),
        &map(),
    );
    assert_eq!(
        items,
        vec![CitationItem {
            source: CitationSource::File,
            title: "report.pdf".to_owned(),
            snippet: String::new(),
            url: None,
            attachment_id: Some(FILE_ATTACHMENT),
            span: None,
        }]
    );
}

#[test]
fn a_file_annotation_of_an_unknown_file_is_omitted() {
    let items = map_citations(
        &[annotation(
            json!({"type": "file_citation", "file_id": "file-gone"}),
        )],
        &OutputParts::new(),
        &map(),
    );
    assert!(items.is_empty());
}

#[test]
fn without_a_citation_map_no_file_item_is_produced() {
    let items = map_citations(
        &[annotation(
            json!({"type": "file_citation", "file_id": "file-doc1"}),
        )],
        &OutputParts::new(),
        &CitationMap::new(),
    );
    assert!(items.is_empty());
}

#[test]
fn unknown_annotation_types_are_ignored() {
    let items = map_citations(
        &[
            annotation(json!({"type": "container_file_citation", "file_id": "x"})),
            annotation(json!({"type": "file_path", "file_id": "x"})),
            annotation(json!({"no_type": true})),
        ],
        &OutputParts::new(),
        &map(),
    );
    assert!(items.is_empty());
}

#[test]
fn items_keep_the_provider_order_and_are_not_deduplicated() {
    let web = annotation(json!({"type": "url_citation", "url": "https://e.org", "title": "E"}));
    let file = annotation(json!({"type": "file_citation", "file_id": "file-doc1"}));
    let items = map_citations(&[web.clone(), file, web], &OutputParts::new(), &map());
    let sources: Vec<_> = items.iter().map(|item| item.source).collect();
    assert_eq!(
        sources,
        [
            CitationSource::Web,
            CitationSource::File,
            CitationSource::Web
        ]
    );
}

#[test]
fn no_annotations_no_items() {
    assert!(map_citations(&[], &OutputParts::new(), &map()).is_empty());
}

#[test]
fn an_item_never_carries_the_raw_provider_file_id() {
    let items = map_citations(
        &[annotation(
            json!({"type": "file_citation", "file_id": "file-doc1"}),
        )],
        &OutputParts::new(),
        &map(),
    );
    let debug = format!("{items:?}");
    assert!(!debug.contains("file-doc1"), "{debug}");
}
