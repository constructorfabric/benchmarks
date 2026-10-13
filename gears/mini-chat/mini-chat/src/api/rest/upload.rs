//! Reading of the multipart upload body (DESIGN "Upload Attachment").
//!
//! The body is read as a stream: the `file` part's headers are validated before its content is
//! read, the bytes are counted while they arrive and the read stops as soon as the size limit is
//! exceeded.

use axum::body::Body;
use bytes::Bytes;
use http::HeaderMap;

use crate::domain::attachment::{Admission, AttachmentService, IncomingFile, UploadSession};
use crate::domain::error::{DomainError, MultipartIssue};

/// The name of the multipart field that carries the file.
const FILE_FIELD: &str = "file";

fn invalid(issue: MultipartIssue) -> DomainError {
    DomainError::InvalidMultipart { issue }
}

/// The multipart boundary of the `Content-Type` header.
fn boundary_of(headers: &HeaderMap) -> Result<String, DomainError> {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| multer::parse_boundary(value).ok())
        .ok_or_else(|| invalid(MultipartIssue::BoundaryRequired))
}

/// Reads the `file` part: admits its headers, then buffers its content up to the size limit.
///
/// # Errors
/// `InvalidMultipart` for an unreadable body, a missing `file` field or content type,
/// `UnsupportedContentType`, `FeatureDisabled`, `CodeInterpreterUnavailable` from the admission and
/// `FileTooLarge` when the content exceeds the limit.
pub(super) async fn read_file(
    service: &AttachmentService,
    session: &UploadSession,
    headers: &HeaderMap,
    body: Body,
) -> Result<(Admission, IncomingFile), DomainError> {
    let boundary = boundary_of(headers)?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(field)) if field.name() == Some(FILE_FIELD) => break field,
            Ok(Some(_other)) => {}
            Ok(None) => return Err(invalid(MultipartIssue::MissingFile)),
            Err(_) => return Err(invalid(MultipartIssue::Malformed)),
        }
    };
    let declared = field.content_type().map(ToString::to_string);
    let admission = service.admit(session, field.file_name(), declared.as_deref())?;
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut size: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                size = size.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                if size > admission.max_bytes {
                    return Err(DomainError::FileTooLarge);
                }
                chunks.push(chunk);
            }
            Ok(None) => break,
            Err(_) => return Err(invalid(MultipartIssue::Malformed)),
        }
    }
    Ok((admission, IncomingFile { chunks, size }))
}
