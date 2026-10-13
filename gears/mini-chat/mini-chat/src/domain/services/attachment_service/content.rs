//! Upload content types and filenames (DESIGN section 3.3 "Upload
//! Attachment", PRD supported file types).

use crate::domain::enums::AttachmentKind;

pub(super) const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const OCTET_STREAM: &str = "application/octet-stream";
const CSV: &str = "text/csv";
const TEXT_PLAIN: &str = "text/plain";

/// Supported MIME types and the extension used for the provider-side file.
const SUPPORTED: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (DOCX, "docx"),
    (PPTX, "pptx"),
    (XLSX, "xlsx"),
    (TEXT_PLAIN, "txt"),
    ("text/markdown", "md"),
    ("text/html", "html"),
    ("application/json", "json"),
    ("text/x-python", "py"),
    ("text/x-java", "java"),
    ("text/x-java-source", "java"),
    ("text/javascript", "js"),
    ("application/javascript", "js"),
    ("application/typescript", "ts"),
    ("text/x-typescript", "ts"),
    ("text/x-rust", "rs"),
    ("text/x-go", "go"),
    ("text/x-csharp", "cs"),
    ("text/x-ruby", "rb"),
    ("application/sql", "sql"),
    ("text/x-sql", "sql"),
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

/// Filename extension -> MIME type for `application/octet-stream` parts.
const BY_EXTENSION: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("docx", DOCX),
    ("pptx", PPTX),
    ("xlsx", XLSX),
    ("txt", TEXT_PLAIN),
    ("md", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("json", "application/json"),
    ("py", "text/x-python"),
    ("java", "text/x-java"),
    ("js", "text/javascript"),
    ("ts", "application/typescript"),
    ("rs", "text/x-rust"),
    ("go", "text/x-go"),
    ("cs", "text/x-csharp"),
    ("rb", "text/x-ruby"),
    ("sql", "application/sql"),
    ("csv", CSV),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// Maximum stored filename length (characters).
const MAX_FILENAME_CHARS: usize = 255;
/// Longest extension kept when a filename is truncated.
const MAX_KEPT_EXTENSION_CHARS: usize = 32;
/// Filename of a part without one.
const DEFAULT_FILENAME: &str = "upload";

/// A supported content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContentType {
    pub mime: &'static str,
    pub ext: &'static str,
}

impl ContentType {
    pub fn kind(self) -> AttachmentKind {
        if self.mime.starts_with("image/") {
            AttachmentKind::Image
        } else {
            AttachmentKind::Document
        }
    }

    /// XLSX: code interpreter is its only purpose.
    pub fn code_interpreter_only(self) -> bool {
        self.mime == XLSX
    }
}

/// The supported type of a part: parameters stripped, lower-cased,
/// `application/octet-stream` inferred from the filename extension, `text/csv`
/// served as `text/plain` when CSV uploads are allowed. `None` = unsupported.
pub(super) fn resolve(
    filename: Option<&str>,
    declared: &str,
    allow_csv: bool,
) -> Option<ContentType> {
    let base = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut mime: &str = &base;
    if mime == OCTET_STREAM {
        let ext = extension(filename?)?.to_ascii_lowercase();
        mime = BY_EXTENSION.iter().find(|(e, _)| *e == ext)?.1;
    }
    if mime == CSV {
        if !allow_csv {
            return None;
        }
        mime = TEXT_PLAIN;
    }
    SUPPORTED
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|&(mime, ext)| ContentType { mime, ext })
}

/// The extension after the last `.` (not a leading dot).
fn extension(name: &str) -> Option<&str> {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => Some(&name[i + 1..]),
        _ => None,
    }
}

/// `upload` for a missing or empty name; longer than 255 characters is
/// truncated keeping the extension.
pub(super) fn normalize_filename(name: Option<&str>) -> String {
    let name = name.filter(|n| !n.is_empty()).unwrap_or(DEFAULT_FILENAME);
    if name.chars().count() <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    let suffix = extension(name)
        .map(|e| format!(".{e}"))
        .filter(|s| s.chars().count() <= MAX_KEPT_EXTENSION_CHARS)
        .unwrap_or_default();
    let keep = MAX_FILENAME_CHARS - suffix.chars().count();
    let stem_end = name.len() - suffix.len();
    let mut out: String = name[..stem_end].chars().take(keep).collect();
    out.push_str(&suffix);
    out
}
