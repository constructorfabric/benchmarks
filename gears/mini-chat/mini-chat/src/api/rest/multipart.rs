//! The raw multipart request body as a [`MultipartSource`]
//! (`cpt-cf-mini-chat-algo-attachments-read-multipart`): the handler gives the
//! `Content-Type` header and the body stream; nothing is read until the domain
//! asks for the `file` part, so the upload route sets no body limit and a
//! rejected request costs no body read.

use async_trait::async_trait;
use axum::body::Body;
use bytes::Bytes;
use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};

use crate::domain::error::MultipartIssue;
use crate::domain::service::attachments::source::{FilePartHeader, MultipartSource};

/// A multipart request body read with `multer`.
pub struct RawMultipart {
    content_type: Option<String>,
    body: Option<Body>,
    parser: Option<multer::Multipart<'static>>,
    field: Option<multer::Field<'static>>,
}

impl RawMultipart {
    /// Wraps the `Content-Type` header value (if any) and the body.
    #[must_use]
    pub fn new(content_type: Option<&str>, body: Body) -> Self {
        Self {
            content_type: content_type.map(str::to_owned),
            body: Some(body),
            parser: None,
            field: None,
        }
    }

    /// Creates the parser on the first read: the boundary is checked before the
    /// body is touched.
    fn parser(&mut self) -> Result<&mut multer::Multipart<'static>, MultipartIssue> {
        if self.parser.is_none() {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-boundary
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-return-boundary
            let boundary = self
                .content_type
                .as_deref()
                .and_then(|value| multer::parse_boundary(value).ok())
                .filter(|boundary| !boundary.trim().is_empty())
                .ok_or(MultipartIssue::BoundaryRequired)?;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-return-boundary
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-boundary
            let body = self.body.take().ok_or(MultipartIssue::MultipartError)?;
            self.parser = Some(multer::Multipart::new(body.into_data_stream(), boundary));
        }
        self.parser.as_mut().ok_or(MultipartIssue::MultipartError)
    }
}

/// `filename*` (RFC 5987 `UTF-8''pct`) wins over `filename`.
fn filename_of(field: &multer::Field<'_>) -> Option<String> {
    let disposition = field
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())?;
    let mut plain = None;
    let mut extended = None;
    for param in split_params(disposition).into_iter().skip(1) {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => extended = decode_ext_value(value),
            "filename" => plain = Some(unquote(value)),
            _ => {}
        }
    }
    extended.or(plain)
}

/// Splits on `;` outside quotes.
fn split_params(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (at, ch) in header.char_indices() {
        match ch {
            '\\' if quoted => escaped = !escaped,
            '"' if !escaped => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&header[start..at]);
                start = at + 1;
            }
            _ => escaped = false,
        }
        if ch != '\\' {
            escaped = false;
        }
    }
    parts.push(&header[start..]);
    parts
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn decode_ext_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?;
    let encoded = parts.next()?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[async_trait]
impl MultipartSource for RawMultipart {
    async fn open_file(&mut self) -> Result<FilePartHeader, MultipartIssue> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-while-parts
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-file
            let next = self
                .parser()?
                .next_field()
                .await
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-malformed
                .map_err(|_| MultipartIssue::MultipartError)?;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-malformed
            let Some(mut field) = next else {
                return Err(MultipartIssue::MissingFile);
            };
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-file
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-discard-part
            if field.name() != Some("file") {
                while field
                    .chunk()
                    .await
                    .map_err(|_| MultipartIssue::MultipartError)?
                    .is_some()
                {}
                continue;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-discard-part
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-part-type
            let content_type = field
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .ok_or(MultipartIssue::MissingContentType)?;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-if-no-part-type
            let filename = filename_of(&field);
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-return-part
            self.field = Some(field);
            return Ok(FilePartHeader {
                content_type,
                filename,
            });
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-return-part
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-while-parts
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>, MultipartIssue> {
        let Some(field) = self.field.as_mut() else {
            return Err(MultipartIssue::MultipartError);
        };
        field
            .chunk()
            .await
            .map_err(|_| MultipartIssue::MultipartError)
    }
}

#[cfg(test)]
#[path = "multipart_tests.rs"]
mod tests;
