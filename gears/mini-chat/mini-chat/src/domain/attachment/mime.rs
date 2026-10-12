//! Upload content types and filenames (DESIGN "Upload Attachment", "Purpose routing", B.7).
//!
//! The MIME allowlist, the extension inference of `application/octet-stream` parts, the CSV remap
//! and the filename defaults are pure rules of the upload.

use super::model::{AttachmentKind, Purposes};

/// The XLSX content type: the only code-interpreter-only type.
pub const XLSX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Filename of an upload whose `file` part carries none.
pub const DEFAULT_FILENAME: &str = "upload";

/// Longest stored filename, in characters.
pub const MAX_FILENAME_CHARS: usize = 255;

const OCTET_STREAM: &str = "application/octet-stream";
const CSV: &str = "text/csv";
const PLAIN_TEXT: &str = "text/plain";

/// The allowlist: MIME type, canonical file extension and how the type is processed.
const ALLOWED: [(&str, &str, Kind); 24] = [
    ("application/pdf", "pdf", Kind::Search),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
        Kind::Search,
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
        Kind::Search,
    ),
    (XLSX_CONTENT_TYPE, "xlsx", Kind::Spreadsheet),
    (PLAIN_TEXT, "txt", Kind::Search),
    ("text/markdown", "md", Kind::Search),
    ("text/html", "html", Kind::Search),
    ("application/json", "json", Kind::Search),
    ("text/x-python", "py", Kind::Search),
    ("text/x-java", "java", Kind::Search),
    ("text/x-java-source", "java", Kind::Search),
    ("text/javascript", "js", Kind::Search),
    ("application/javascript", "js", Kind::Search),
    ("text/typescript", "ts", Kind::Search),
    ("application/typescript", "ts", Kind::Search),
    ("text/x-rust", "rs", Kind::Search),
    ("text/x-go", "go", Kind::Search),
    ("text/x-csharp", "cs", Kind::Search),
    ("text/x-ruby", "rb", Kind::Search),
    ("application/sql", "sql", Kind::Search),
    ("image/png", "png", Kind::Image),
    ("image/jpeg", "jpg", Kind::Image),
    ("image/webp", "webp", Kind::Image),
    ("image/gif", "gif", Kind::Image),
];

/// File extension to MIME type, for `application/octet-stream` parts.
const EXTENSIONS: [(&str, &str); 28] = [
    ("pdf", "application/pdf"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("xlsx", XLSX_CONTENT_TYPE),
    ("txt", PLAIN_TEXT),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("json", "application/json"),
    ("csv", CSV),
    ("py", "text/x-python"),
    ("java", "text/x-java"),
    ("js", "text/javascript"),
    ("ts", "text/typescript"),
    ("rs", "text/x-rust"),
    ("go", "text/x-go"),
    ("cs", "text/x-csharp"),
    ("rb", "text/x-ruby"),
    ("sql", "application/sql"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("mjs", "text/javascript"),
    ("tsx", "text/typescript"),
    ("jsx", "text/javascript"),
];

/// How an allowed type is processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A document indexed for `file_search`.
    Search,
    /// A spreadsheet for the code interpreter only.
    Spreadsheet,
    /// An image.
    Image,
}

/// An accepted upload type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedType {
    /// The MIME type that is stored (CSV is `text/plain`).
    pub content_type: String,
    /// Document or image.
    pub kind: AttachmentKind,
    /// The tool paths before the kill switches and model capabilities apply.
    pub purposes: Purposes,
    /// The canonical file extension of the type.
    pub extension: &'static str,
}

/// The MIME type of a filename extension, `None` when the extension is unknown or missing.
fn mime_of_extension(filename: &str) -> Option<&'static str> {
    let extension = file_extension(filename)?.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, mime)| *mime)
}

/// The extension of a filename: the text after the last dot, when that dot is not the first
/// character.
#[must_use]
pub fn file_extension(filename: &str) -> Option<&str> {
    let dot = filename.rfind('.')?;
    let extension = &filename[dot + 1..];
    (dot > 0 && !extension.is_empty()).then_some(extension)
}

/// Resolves the declared content type of the `file` part.
///
/// `application/octet-stream` takes the MIME type of the filename extension; `text/csv` is stored
/// as `text/plain` when `allow_csv` is on. `None` means `UNSUPPORTED_CONTENT_TYPE`.
#[must_use]
pub fn resolve_content_type(
    declared: &str,
    filename: &str,
    allow_csv: bool,
) -> Option<ResolvedType> {
    let essence = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut mime = if essence == OCTET_STREAM {
        mime_of_extension(filename)?.to_owned()
    } else {
        essence
    };
    if mime == CSV {
        if !allow_csv {
            return None;
        }
        PLAIN_TEXT.clone_into(&mut mime);
    }
    let (content_type, extension, kind) =
        ALLOWED.iter().find(|(allowed, _, _)| *allowed == mime)?;
    let (attachment_kind, purposes) = match kind {
        Kind::Search => (
            AttachmentKind::Document,
            Purposes {
                file_search: true,
                code_interpreter: false,
            },
        ),
        Kind::Spreadsheet => (
            AttachmentKind::Document,
            Purposes {
                file_search: false,
                code_interpreter: true,
            },
        ),
        Kind::Image => (
            AttachmentKind::Image,
            Purposes {
                file_search: false,
                code_interpreter: false,
            },
        ),
    };
    Some(ResolvedType {
        content_type: (*content_type).to_owned(),
        kind: attachment_kind,
        purposes,
        extension,
    })
}

/// The stored filename: `upload` when missing or blank, without path components and control
/// characters, at most 255 characters with the extension kept.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    let cleaned: String = raw
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return DEFAULT_FILENAME.to_owned();
    }
    truncate_keeping_extension(trimmed, MAX_FILENAME_CHARS)
}

/// Cuts `name` to `limit` characters; the extension (text from the last dot) survives when it is
/// shorter than the limit.
fn truncate_keeping_extension(name: &str, limit: usize) -> String {
    if name.chars().count() <= limit {
        return name.to_owned();
    }
    let extension = file_extension(name)
        .map(|extension| format!(".{extension}"))
        .filter(|extension| extension.chars().count() < limit);
    match extension {
        Some(extension) => {
            let stem: String = name
                .chars()
                .take(limit - extension.chars().count())
                .collect();
            format!("{stem}{extension}")
        }
        None => name.chars().take(limit).collect(),
    }
}
