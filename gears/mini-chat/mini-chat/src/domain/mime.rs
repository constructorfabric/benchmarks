//! Attachment MIME allowlist, extension inference and filename rules (spec §12).

use super::model::AttachmentKind;

pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const OCTET_STREAM: &str = "application/octet-stream";
const CSV: &str = "text/csv";

const MAX_FILENAME_CHARS: usize = 255;

/// Allowlisted content types.
const ALLOWED: &[&str] = &[
    "application/pdf",
    DOCX_MIME,
    PPTX_MIME,
    XLSX_MIME,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "application/typescript",
    "text/x-typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "text/x-sql",
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];

/// Extension (lower case) to content type.
const BY_EXTENSION: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("docx", DOCX_MIME),
    ("pptx", PPTX_MIME),
    ("xlsx", XLSX_MIME),
    ("txt", "text/plain"),
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

/// Why a content type was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MimeError {
    #[error("file part has no content type")]
    MissingContentType,
    #[error("unsupported content type: {0}")]
    Unsupported(String),
}

fn extension_of(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Resolve the stored content type of an uploaded file.
///
/// Parameters (`; charset=...`) are dropped and the type lower-cased;
/// `application/octet-stream` is inferred from the filename extension;
/// `text/csv` becomes `text/plain` when `allow_csv` is set.
///
/// # Errors
///
/// [`MimeError::MissingContentType`] for `None`, [`MimeError::Unsupported`]
/// when the type is not on the allowlist.
pub fn resolve_content_type(
    part_ct: Option<&str>,
    filename: &str,
    allow_csv: bool,
) -> Result<String, MimeError> {
    let raw = part_ct.ok_or(MimeError::MissingContentType)?;
    let mut ct = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if ct.is_empty() {
        return Err(MimeError::MissingContentType);
    }
    if ct == OCTET_STREAM {
        if let Some(inferred) = extension_of(filename)
            .and_then(|e| BY_EXTENSION.iter().find(|(x, _)| *x == e).map(|(_, c)| *c))
        {
            inferred.clone_into(&mut ct);
        }
    }
    if ct == CSV {
        return if allow_csv {
            Ok("text/plain".to_owned())
        } else {
            Err(MimeError::Unsupported(ct))
        };
    }
    if ALLOWED.contains(&ct.as_str()) {
        Ok(ct)
    } else {
        Err(MimeError::Unsupported(ct))
    }
}

/// Image types are images; everything else is a document.
#[must_use]
pub fn attachment_kind(ct: &str) -> AttachmentKind {
    if ct.starts_with("image/") {
        AttachmentKind::Image
    } else {
        AttachmentKind::Document
    }
}

/// `(for_file_search, for_code_interpreter)`: XLSX is code-interpreter only,
/// images have no purpose, other documents are searched.
#[must_use]
pub fn purposes(ct: &str) -> (bool, bool) {
    if ct == XLSX_MIME {
        (false, true)
    } else {
        (attachment_kind(ct) == AttachmentKind::Document, false)
    }
}

/// Filename stored for the upload: default `upload`, at most 255 characters,
/// the extension is kept when the name is truncated.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map_or("", str::trim);
    if name.is_empty() {
        return "upload".to_owned();
    }
    let len = name.chars().count();
    if len <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    let ext_start = name
        .rfind('.')
        .filter(|&i| i > 0 && name[i..].chars().count() < MAX_FILENAME_CHARS);
    match ext_start {
        Some(i) => {
            let ext = &name[i..];
            let keep = MAX_FILENAME_CHARS - ext.chars().count();
            let stem: String = name[..i].chars().take(keep).collect();
            format!("{stem}{ext}")
        }
        None => name.chars().take(MAX_FILENAME_CHARS).collect(),
    }
}

/// File extension (without dot) used for the provider filename; `bin` if unknown.
#[must_use]
pub fn extension_for(ct: &str) -> &'static str {
    match ct {
        "text/x-java-source" => "java",
        "application/javascript" => "js",
        "text/x-typescript" => "ts",
        "text/x-sql" => "sql",
        "image/jpeg" => "jpg",
        "text/html" => "html",
        _ => BY_EXTENSION
            .iter()
            .find(|(_, c)| *c == ct)
            .map_or("bin", |(e, _)| *e),
    }
}

#[cfg(test)]
#[path = "mime_tests.rs"]
mod mime_tests;
