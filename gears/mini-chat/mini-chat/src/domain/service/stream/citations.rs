//! Citation mapping (`cpt-cf-mini-chat-algo-streaming-turns-map-citations`).

use std::collections::HashMap;

use serde_json::Value;

use super::ports::CitationMap;
use super::types::{Annotation, CitationItem, CitationSource};

/// The accumulated text of the output parts, by `(item_id, content_index)`.
pub type OutputParts = HashMap<(Option<String>, Option<u32>), String>;

/// Maps the annotations of a completed stream in provider order: web
/// annotations (`url_citation`, `web_search_result_location`) and file
/// annotations (`file_citation`) known to `map`; never a score and never a raw
/// provider file id.
#[must_use]
pub fn map_citations(
    annotations: &[Annotation],
    parts: &OutputParts,
    map: &CitationMap,
) -> Vec<CitationItem> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-for-each-annotation
    annotations
        .iter()
        .filter_map(|annotation| match annotation.value["type"].as_str() {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-if-web
            Some("url_citation" | "web_search_result_location") => {
                Some(web_item(annotation, parts))
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-if-web
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-if-file
            Some("file_citation") => file_item(annotation, map),
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-if-file
            _ => None,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-for-each-annotation
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-return-items
        .collect()
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-return-items
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-add-web-item
fn web_item(annotation: &Annotation, parts: &OutputParts) -> CitationItem {
    let value = &annotation.value;
    let start = value.get("start_index").and_then(Value::as_u64);
    let end = value.get("end_index").and_then(Value::as_u64);
    let snippet = text_of(value, "text")
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            let part = annotation
                .part_text
                .as_ref()
                .or_else(|| parts.get(&(annotation.item_id.clone(), annotation.content_index)));
            match (part, start, end) {
                (Some(part), Some(start), Some(end)) => slice_chars(part, start, end),
                _ => String::new(),
            }
        });
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-no-score
    CitationItem {
        source: CitationSource::Web,
        title: text_of(value, "title").unwrap_or_default(),
        snippet,
        url: text_of(value, "url"),
        attachment_id: None,
        span: start.zip(end),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-no-score
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-add-web-item

/// The characters `start..end` of `text`; an empty string for a range outside
/// the text.
fn slice_chars(text: &str, start: u64, end: u64) -> String {
    let total = text.chars().count() as u64;
    if start > end || end > total {
        return String::new();
    }
    text.chars()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .take(usize::try_from(end - start).unwrap_or(usize::MAX))
        .collect()
}

// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-add-file-item
fn file_item(annotation: &Annotation, map: &CitationMap) -> Option<CitationItem> {
    let file_id = annotation.value.get("file_id")?.as_str()?;
    let target = map.get(file_id)?;
    Some(CitationItem {
        source: CitationSource::File,
        title: target.filename.clone(),
        snippet: String::new(),
        url: None,
        attachment_id: Some(target.attachment_id),
        span: None,
    })
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-map-citations:p1:inst-add-file-item

#[cfg(test)]
#[path = "citations_tests.rs"]
mod tests;
