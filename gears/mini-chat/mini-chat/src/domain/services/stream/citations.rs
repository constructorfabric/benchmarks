//! Provider annotations → public citations (DESIGN §3.3 "`event: citations`",
//! §4 "Citation File ID and Title Resolution"). Provider file ids never leave
//! this mapping: file citations carry the attachment id and filename.

use std::collections::HashMap;

use uuid::Uuid;

use crate::api::rest::dto::{Citation, CitationSource, TextSpan};
use crate::infra::llm::RawCitation;

fn span((start, end): (u32, u32)) -> TextSpan {
    TextSpan { start, end }
}

/// The public citation of `raw`; `None` for a file citation whose provider file
/// id is not a ready attachment of the chat (unknown or deleted).
#[must_use]
pub(super) fn map_citation(
    raw: RawCitation,
    files: &HashMap<String, (Uuid, String)>,
) -> Option<Citation> {
    match raw {
        RawCitation::Web {
            url,
            title,
            snippet,
            span: range,
        } => Some(Citation {
            source: CitationSource::Web,
            title,
            url: Some(url),
            attachment_id: None,
            snippet,
            span: range.map(span),
            score: None,
        }),
        RawCitation::File {
            provider_file_id,
            span: range,
        } => {
            let (attachment_id, filename) = files.get(&provider_file_id)?;
            Some(Citation {
                source: CitationSource::File,
                title: filename.clone(),
                url: None,
                attachment_id: Some(*attachment_id),
                snippet: String::new(),
                span: range.map(span),
                score: None,
            })
        }
    }
}
