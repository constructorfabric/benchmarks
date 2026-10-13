//! Upload MIME validation, inference and purpose routing (DESIGN "File Upload", "Purpose routing").

/// XLSX MIME type (code-interpreter only).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Image MIME types.
pub const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Document MIME types accepted for `file_search` (provider-supported retrieval formats).
pub const DOCUMENT_TYPES: [&str; 22] = [
    "text/plain",
    "text/markdown",
    "text/html",
    "text/x-c",
    "text/x-c++",
    "text/x-csharp",
    "text/css",
    "text/x-golang",
    "text/x-java",
    "text/javascript",
    "text/x-php",
    "text/x-python",
    "text/x-ruby",
    "text/x-script.python",
    "text/x-tex",
    "text/x-typescript",
    "application/json",
    "application/pdf",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/x-sh",
];

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Document.
    Document,
    /// Image.
    Image,
}

impl Kind {
    /// Stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }
}

/// Classification of a validated MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// Stored content type (CSV remapped to `text/plain`).
    pub content_type: String,
    /// Kind.
    pub kind: Kind,
    /// Indexed in the chat vector store.
    pub for_file_search: bool,
    /// Available to `code_interpreter`.
    pub for_code_interpreter: bool,
}

fn essence(ct: &str) -> String {
    ct.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

/// Validates and classifies a MIME type; `None` when unsupported.
#[must_use]
pub fn classify(content_type: &str, allow_csv: bool) -> Option<Classified> {
    let mut ct = essence(content_type);
    if ct == "image/jpg" {
        "image/jpeg".clone_into(&mut ct);
    }
    if ct == "text/csv" {
        if !allow_csv {
            return None;
        }
        "text/plain".clone_into(&mut ct);
    }
    if IMAGE_TYPES.contains(&ct.as_str()) {
        return Some(Classified { content_type: ct, kind: Kind::Image, for_file_search: false, for_code_interpreter: false });
    }
    if ct == XLSX {
        return Some(Classified { content_type: ct, kind: Kind::Document, for_file_search: false, for_code_interpreter: true });
    }
    if DOCUMENT_TYPES.contains(&ct.as_str()) {
        return Some(Classified { content_type: ct, kind: Kind::Document, for_file_search: true, for_code_interpreter: false });
    }
    None
}

/// MIME type by filename extension.
#[must_use]
pub fn from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "json" => "application/json",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "hpp" => "text/x-c++",
        "cs" => "text/x-csharp",
        "css" => "text/css",
        "go" => "text/x-golang",
        "java" => "text/x-java",
        "js" | "mjs" => "text/javascript",
        "php" => "text/x-php",
        "py" => "text/x-python",
        "rb" => "text/x-ruby",
        "tex" => "text/x-tex",
        "ts" => "text/x-typescript",
        "sh" => "application/x-sh",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Effective content type of a part: `application/octet-stream` is inferred from the extension.
#[must_use]
pub fn effective_content_type(part_content_type: &str, filename: &str) -> String {
    if essence(part_content_type) == "application/octet-stream" {
        return from_extension(filename).unwrap_or("application/octet-stream").to_owned();
    }
    part_content_type.to_owned()
}

/// Filename default (`upload`) and truncation to 255 characters keeping the extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or("upload");
    if name.chars().count() <= 255 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            let stem: String = stem.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        _ => name.chars().take(255).collect(),
    }
}

#[cfg(test)]
#[path = "mime_tests.rs"]
mod mime_tests;
