//! The upload allowlist (PRD "document file types", DESIGN 3.6 "File
//! Upload"): which content types are accepted, how `application/octet-stream`
//! is resolved from the file extension, and what each accepted type is for
//! (`file_search`, the code interpreter, or model input as an image).

use crate::domain::messages::AttachmentKind;

/// Content type of a spreadsheet: code interpreter only.
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

const OCTET_STREAM: &str = "application/octet-stream";
const CSV: &str = "text/csv";
const PLAIN: &str = "text/plain";

/// Source code the provider's `file_search` does not list as a file type
/// (Rust, Go, SQL). Stored and forwarded as `text/plain`, like CSV, with the
/// provider file name ending in `.txt` so the provider indexes it as text.
const PLAIN_TEXT_SOURCE: &[&str] = &[
    "text/x-rust",
    "text/rust",
    "text/x-go",
    "text/x-golang",
    "application/sql",
    "text/x-sql",
];

/// Accepted types with the extension the provider file name carries.
const ALLOWED: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    (XLSX, "xlsx"),
    ("application/msword", "doc"),
    (PLAIN, "txt"),
    ("text/markdown", "md"),
    ("text/html", "html"),
    ("application/json", "json"),
    ("text/x-python", "py"),
    ("text/x-script.python", "py"),
    ("text/javascript", "js"),
    ("application/javascript", "js"),
    ("application/typescript", "ts"),
    ("text/x-typescript", "ts"),
    ("text/x-java", "java"),
    ("text/x-c", "c"),
    ("text/x-c++", "cpp"),
    ("text/x-csharp", "cs"),
    ("text/x-php", "php"),
    ("text/x-ruby", "rb"),
    ("text/x-tex", "tex"),
    ("text/css", "css"),
    ("application/x-sh", "sh"),
    ("text/x-shellscript", "sh"),
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

/// Content type of a file extension (for `application/octet-stream` parts).
const BY_EXTENSION: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("xlsx", XLSX),
    ("doc", "application/msword"),
    ("txt", PLAIN),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("json", "application/json"),
    ("csv", CSV),
    ("py", "text/x-python"),
    ("js", "text/javascript"),
    ("ts", "application/typescript"),
    ("java", "text/x-java"),
    ("c", "text/x-c"),
    ("cpp", "text/x-c++"),
    ("cs", "text/x-csharp"),
    ("php", "text/x-php"),
    ("rb", "text/x-ruby"),
    ("tex", "text/x-tex"),
    ("css", "text/css"),
    ("sh", "application/x-sh"),
    ("rs", "text/x-rust"),
    ("go", "text/x-go"),
    ("sql", "application/sql"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// What an accepted upload is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileKind {
    /// Content type stored on the row and forwarded to the provider (CSV and
    /// Rust, Go and SQL source are stored and forwarded as `text/plain`).
    pub content_type: String,
    /// Document or image.
    pub kind: AttachmentKind,
    /// Added to the chat vector store.
    pub for_file_search: bool,
    /// Offered to the code interpreter only.
    pub for_code_interpreter: bool,
    /// Extension of the provider file name.
    pub extension: &'static str,
}

/// The extension of a file name (lower case, without the dot).
fn extension_of(filename: &str) -> Option<String> {
    let (_, extension) = filename.rsplit_once('.')?;
    (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// The content type after `application/octet-stream` was replaced by the type
/// of the file extension; an unknown extension keeps the octet stream.
fn effective_type(declared: &str, filename: &str) -> String {
    let declared = declared.trim().to_ascii_lowercase();
    if declared != OCTET_STREAM {
        return declared;
    }
    extension_of(filename)
        .and_then(|extension| {
            BY_EXTENSION
                .iter()
                .find(|(known, _)| *known == extension)
                .map(|(_, content_type)| (*content_type).to_owned())
        })
        .unwrap_or(declared)
}

/// Classifies an upload; `None` is an unsupported content type.
pub fn classify(declared: &str, filename: &str, allow_csv: bool) -> Option<FileKind> {
    let mut content_type = effective_type(declared, filename);
    let csv = content_type == CSV;
    if csv {
        if !allow_csv {
            return None;
        }
        // Stored and forwarded as plain text; the provider file keeps `.csv`
        // so the file is recognisable.
        PLAIN.clone_into(&mut content_type);
    }
    if PLAIN_TEXT_SOURCE.contains(&content_type.as_str()) {
        PLAIN.clone_into(&mut content_type);
    }
    let (_, extension) = ALLOWED.iter().find(|(known, _)| *known == content_type)?;
    let image = content_type.starts_with("image/");
    let spreadsheet = content_type == XLSX;
    Some(FileKind {
        content_type,
        kind: if image {
            AttachmentKind::Image
        } else {
            AttachmentKind::Document
        },
        for_file_search: !image && !spreadsheet,
        for_code_interpreter: spreadsheet,
        extension: if csv { "csv" } else { extension },
    })
}

/// A file name of at most `max` characters; a longer one is cut in front of
/// its extension, so the extension survives.
pub fn truncate_filename(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_owned();
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty() && extension.chars().count() < max);
    match extension {
        Some(extension) => {
            let keep = max - extension.chars().count() - 1;
            let stem: String = name.chars().take(keep).collect();
            format!("{stem}.{extension}")
        }
        None => name.chars().take(max).collect(),
    }
}

#[cfg(test)]
#[path = "mime_tests.rs"]
mod tests;
