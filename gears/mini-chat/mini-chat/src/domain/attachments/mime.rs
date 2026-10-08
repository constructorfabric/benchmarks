//! MIME resolution, purpose routing and filename rules of attachment uploads (DESIGN §3.3
//! "Upload Attachment", §3.6 "File Upload", "Code Interpreter Tool Availability").

pub const PDF: &str = "application/pdf";
pub const TEXT: &str = "text/plain";
pub const MARKDOWN: &str = "text/markdown";
pub const HTML: &str = "text/html";
pub const JSON: &str = "application/json";
pub const CSV: &str = "text/csv";
pub const DOC: &str = "application/msword";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const PNG: &str = "image/png";
pub const JPEG: &str = "image/jpeg";
pub const WEBP: &str = "image/webp";
pub const GIF: &str = "image/gif";
pub const OCTET_STREAM: &str = "application/octet-stream";

/// Maximum stored filename length (characters).
pub const MAX_FILENAME_CHARS: usize = 255;
/// Filename used when the part has none.
pub const DEFAULT_FILENAME: &str = "upload";

/// `attachments.attachment_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Document,
    Image,
}

impl AttachmentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }
}

/// A supported upload type after MIME resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedType {
    /// Content type stored on the row and sent to the provider.
    pub content_type: &'static str,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

/// Lower-cased media type without parameters.
#[must_use]
pub fn essence(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

/// Lower-cased extension of a filename (without the dot).
#[must_use]
pub fn extension(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// MIME type inferred from the filename extension (`application/octet-stream` parts).
#[must_use]
pub fn from_extension(filename: &str) -> Option<&'static str> {
    Some(match extension(filename)?.as_str() {
        "pdf" => PDF,
        "docx" => DOCX,
        "doc" => DOC,
        "txt" => TEXT,
        "md" | "markdown" => MARKDOWN,
        "csv" => CSV,
        "html" | "htm" => HTML,
        "json" => JSON,
        "pptx" => PPTX,
        "xlsx" => XLSX,
        "png" => PNG,
        "jpg" | "jpeg" => JPEG,
        "webp" => WEBP,
        "gif" => GIF,
        _ => return None,
    })
}

/// Effective media type of a part: its own type, or the extension-inferred one for
/// `application/octet-stream` (unknown extensions keep `application/octet-stream`).
#[must_use]
pub fn effective_type(part_content_type: &str, filename: &str) -> String {
    let ct = essence(part_content_type);
    if ct == OCTET_STREAM {
        return from_extension(filename).map_or(ct, str::to_owned);
    }
    ct
}

/// Resolves a media type to a supported upload type (`None` = unsupported).
/// CSV is accepted only when `allow_csv` is set and is stored as `text/plain`.
#[must_use]
pub fn resolve(media_type: &str, allow_csv: bool) -> Option<ResolvedType> {
    let doc = |ct: &'static str| ResolvedType {
        content_type: ct,
        kind: AttachmentKind::Document,
        for_file_search: true,
        for_code_interpreter: false,
    };
    let image = |ct: &'static str| ResolvedType {
        content_type: ct,
        kind: AttachmentKind::Image,
        for_file_search: false,
        for_code_interpreter: false,
    };
    Some(match media_type {
        PDF => doc(PDF),
        TEXT => doc(TEXT),
        MARKDOWN => doc(MARKDOWN),
        HTML => doc(HTML),
        JSON => doc(JSON),
        DOC => doc(DOC),
        DOCX => doc(DOCX),
        PPTX => doc(PPTX),
        CSV if allow_csv => doc(TEXT),
        XLSX => ResolvedType {
            content_type: XLSX,
            kind: AttachmentKind::Document,
            for_file_search: false,
            for_code_interpreter: true,
        },
        PNG => image(PNG),
        JPEG => image(JPEG),
        WEBP => image(WEBP),
        GIF => image(GIF),
        _ => return None,
    })
}

/// File extension used for the provider filename `{chat_id}_{attachment_id}.{ext}`.
#[must_use]
pub fn provider_extension(content_type: &str) -> &'static str {
    match content_type {
        PDF => "pdf",
        MARKDOWN => "md",
        HTML => "html",
        JSON => "json",
        DOC => "doc",
        DOCX => "docx",
        PPTX => "pptx",
        XLSX => "xlsx",
        PNG => "png",
        JPEG => "jpg",
        WEBP => "webp",
        GIF => "gif",
        _ => "txt",
    }
}

/// Filename stored on the row: default `upload`, at most 255 characters keeping the extension.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    let name = raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_FILENAME);
    let total = name.chars().count();
    if total <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() + 1 < MAX_FILENAME_CHARS => {
            let keep = MAX_FILENAME_CHARS - ext.chars().count() - 1;
            let stem: String = stem.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        _ => name.chars().take(MAX_FILENAME_CHARS).collect(),
    }
}
