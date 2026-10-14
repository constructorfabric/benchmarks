//! Citation mapping (slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`,
//! algorithm `cpt-cf-mini-chat-algo-message-streaming-citations`): provider annotations of a
//! normally completed response to the `citations` payload.

use std::collections::BTreeMap;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::streaming::provider_events::{Annotation, ProviderCompletion};
use crate::domain::streaming::relay::{CitationItem, CitationSource, CitationsPayload, TextSpan};

/// A file the request could cite: the internal attachment and its filename.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub attachment_id: Uuid,
    pub filename: String,
}

/// The map `provider_file_id` to [`FileRef`] built before the provider call (only live
/// attachments are in it).
pub type FileCitationMap = BTreeMap<String, FileRef>;

/// Map the annotations of a completed response. `None` for a provider `incomplete` completion
/// and when no item remains. A web annotation gives `source` `web` with `url`, `title`, `span`
/// (only with both indexes) and `snippet` (the annotation text, else the answer text of the
/// range as character offsets into the part that carries the annotation, empty when out of
/// range); a file annotation resolves through `files` (omitted when unknown) to `source` `file`
/// with `attachment_id`, the filename as `title`, an empty `snippet` and no `span`.
#[must_use]
pub fn map_citations(
    completion: &ProviderCompletion,
    files: &FileCitationMap,
) -> Option<CitationsPayload> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-not-completed
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return-none-incomplete
    // a provider `incomplete` completion maps no citations
    if completion.incomplete_reason.is_some() {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return-none-incomplete
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-not-completed
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-each
    let mut items = Vec::new();
    for part in &completion.parts {
        for annotation in &part.annotations {
            match annotation {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-web
                Annotation::Url {
                    url,
                    title,
                    start_index,
                    end_index,
                    text,
                } => {
                    let span = match (start_index, end_index) {
                        (Some(start), Some(end)) => Some(TextSpan {
                            start: *start,
                            end: *end,
                        }),
                        _ => None,
                    };
                    let snippet = text.clone().unwrap_or_else(|| {
                        span.map(|span| snippet_of(&part.text, span))
                            .unwrap_or_default()
                    });
                    items.push(CitationItem {
                        source: CitationSource::Web,
                        title: title.clone().unwrap_or_default(),
                        url: Some(url.clone()),
                        attachment_id: None,
                        snippet,
                        span,
                    });
                }
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-web
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-file
                Annotation::File { file_id, .. } => {
                    // an unknown id or a soft-deleted attachment is not in the map: omitted
                    if let Some(file) = files.get(file_id) {
                        items.push(CitationItem {
                            source: CitationSource::File,
                            title: file.filename.clone(),
                            url: None,
                            attachment_id: Some(file.attachment_id),
                            snippet: String::new(),
                            span: None,
                        });
                    }
                } // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-file
            }
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-each
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-no-provider-ids
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-empty
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return-none
    // an item holds no score, provider file id, vector store id or provider response id
    if items.is_empty() {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return-none
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-empty
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-no-provider-ids
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-not-persisted
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return
    // nothing is persisted: a replay sends no citations; one payload holds all items
    Some(CitationsPayload { items })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-citations:p1:inst-cit-not-persisted
}

/// The text of `part` in `span` as character offsets; empty when the span is out of range.
fn snippet_of(part: &str, span: TextSpan) -> String {
    let length = part.chars().count();
    if span.start > span.end || span.end > length {
        return String::new();
    }
    part.chars()
        .skip(span.start)
        .take(span.end - span.start)
        .collect()
}
