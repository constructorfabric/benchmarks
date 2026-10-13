//! The multipart input of an upload as the domain consumes it
//! (`cpt-cf-mini-chat-algo-attachments-read-multipart`,
//! `cpt-cf-mini-chat-algo-attachments-read-file-content`).

use async_trait::async_trait;
use bytes::Bytes;
use toolkit_macros::domain_model;

use crate::domain::error::{DomainError, MultipartIssue};

/// The headers of the `file` part.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePartHeader {
    /// The part's `Content-Type` as sent (not normalized).
    pub content_type: String,
    /// `filename` (or `filename*`) of the part's `Content-Disposition`, as sent.
    pub filename: Option<String>,
}

/// The raw multipart request, read lazily. Nothing is read before
/// [`MultipartSource::open_file`].
#[async_trait]
pub trait MultipartSource: Send {
    /// Reads up to the headers of the first part named exactly `file`
    /// (case-sensitive); other parts before it are discarded unread, parts after
    /// it are never read.
    ///
    /// # Errors
    ///
    /// `BoundaryRequired` (no `multipart/form-data` boundary), `MissingFile` (the
    /// body ends before a `file` part), `MultipartError` (malformed framing,
    /// truncated body, unparsable part headers), `MissingContentType` (the part
    /// has no or an empty `Content-Type`).
    async fn open_file(&mut self) -> Result<FilePartHeader, MultipartIssue>;

    /// The next chunk of the file content; `None` at its end.
    ///
    /// # Errors
    ///
    /// `MultipartError` when the read fails or the part ends without its closing
    /// boundary.
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, MultipartIssue>;
}

/// Reads the content of the opened `file` part with a byte counter.
///
/// Stops at the first chunk that takes the total above `limit` (the rest of the
/// body is never read, every byte read so far is dropped) and keeps at most
/// `limit` bytes in memory; an empty file is accepted. The chunks are collected
/// as they are (no per-chunk copy, no growth slack) and joined once into a
/// buffer of exactly the content size, so the capacity is the content length; a
/// single chunk is returned as is.
///
/// # Errors
///
/// [`DomainError::FileTooLarge`] above `limit`;
/// [`DomainError::InvalidMultipart`] with `MultipartError` when a read fails.
pub async fn read_file_content(
    source: &mut dyn MultipartSource,
    limit: u64,
) -> Result<Bytes, DomainError> {
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut counted: u64 = 0;
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-count-bytes
    loop {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-if-read-failed
        let chunk = source
            .next_chunk()
            .await
            .map_err(DomainError::InvalidMultipart)?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-if-read-failed
        let Some(chunk) = chunk else { break };
        counted = counted.saturating_add(chunk.len() as u64);
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-if-over-limit
        if counted > limit {
            return Err(DomainError::FileTooLarge);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-if-over-limit
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-keep-chunk
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-keep-chunk
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-count-bytes
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-return-content
    Ok(join(chunks, counted))
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-file-content:p1:inst-return-content
}

/// Joins `chunks` (`total` bytes) into one `Bytes` with exactly `total` capacity.
fn join(mut chunks: Vec<Bytes>, total: u64) -> Bytes {
    if chunks.len() <= 1 {
        return chunks.pop().unwrap_or_default();
    }
    let mut joined = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    for chunk in chunks {
        joined.extend_from_slice(&chunk);
    }
    Bytes::from(joined)
}
