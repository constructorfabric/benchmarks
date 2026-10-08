//! Mapping of provider annotations to client citations (DESIGN §3.3 `citations`, §4
//! "Citation File ID and Title Resolution").

use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

use super::events::{Citation, Span};
use crate::infra::llm::responses::TextPart;

/// `provider_file_id -> (attachment_id, filename)` of the chat's ready, non-deleted attachments.
pub type FileMap = HashMap<String, (Uuid, String)>;

fn char_slice(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let total = text.chars().count();
    if end > total {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

fn index(v: &Value, k: &str) -> Option<usize> {
    v.get(k)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// Maps annotations of the final answer; unknown or deleted files are omitted.
#[must_use]
pub fn map_citations(parts: &[TextPart], files: &FileMap) -> Vec<Citation> {
    let mut out = Vec::new();
    for part in parts {
        for a in &part.annotations {
            match a.get("type").and_then(Value::as_str) {
                Some("url_citation") => {
                    let url = a
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    if url.is_empty() {
                        continue;
                    }
                    let start = index(a, "start_index");
                    let end = index(a, "end_index");
                    let span = match (start, end) {
                        (Some(s), Some(e)) => Some(Span { start: s, end: e }),
                        _ => None,
                    };
                    let snippet = a
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| span.map(|s| char_slice(&part.text, s.start, s.end)))
                        .unwrap_or_default();
                    let title = a
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .unwrap_or(&url)
                        .to_owned();
                    out.push(Citation {
                        source: "web",
                        title,
                        url: Some(url),
                        attachment_id: None,
                        snippet,
                        span,
                    });
                }
                Some("file_citation") => {
                    let Some(fid) = a.get("file_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some((attachment_id, filename)) = files.get(fid) else {
                        continue;
                    };
                    out.push(Citation {
                        source: "file",
                        title: filename.clone(),
                        url: None,
                        attachment_id: Some(*attachment_id),
                        snippet: String::new(),
                        span: None,
                    });
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn web_citation_uses_range_of_part_text() {
        let parts = vec![TextPart {
            text: "Hello world, growth is 5%.".to_owned(),
            annotations: vec![
                json!({"type": "url_citation", "url": "https://e.x/a", "title": "A", "start_index": 6, "end_index": 11}),
            ],
        }];
        let c = map_citations(&parts, &FileMap::new());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].source, "web");
        assert_eq!(c[0].snippet, "world");
        assert_eq!(c[0].span.map(|s| (s.start, s.end)), Some((6, 11)));
    }

    #[test]
    fn web_citation_out_of_range_gives_empty_snippet() {
        let parts = vec![TextPart {
            text: "short".to_owned(),
            annotations: vec![
                json!({"type": "url_citation", "url": "https://e.x/a", "title": "A", "start_index": 2, "end_index": 99}),
            ],
        }];
        assert_eq!(map_citations(&parts, &FileMap::new())[0].snippet, "");
    }

    #[test]
    fn file_citation_maps_to_attachment_and_drops_unknown() {
        let aid = Uuid::new_v4();
        let mut files = FileMap::new();
        files.insert("file-abc".to_owned(), (aid, "Q3.pdf".to_owned()));
        let parts = vec![TextPart {
            text: "x".to_owned(),
            annotations: vec![
                json!({"type": "file_citation", "file_id": "file-abc", "filename": "f", "index": 0}),
                json!({"type": "file_citation", "file_id": "file-zzz", "index": 0}),
            ],
        }];
        let c = map_citations(&parts, &files);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].attachment_id, Some(aid));
        assert_eq!(c[0].title, "Q3.pdf");
        assert_eq!(c[0].snippet, "");
        assert!(c[0].span.is_none());
    }
}
