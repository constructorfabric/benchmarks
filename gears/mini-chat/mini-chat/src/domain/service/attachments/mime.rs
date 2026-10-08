//! Upload MIME allow-list, extension inference, kind / purpose routing and filename rules.

/// XLSX MIME type (code-interpreter only).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
/// DOCX MIME type.
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// PPTX MIME type.
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
/// Generic binary type (triggers extension inference).
pub const OCTET_STREAM: &str = "application/octet-stream";

/// Default filename when the multipart part carries none.
pub const DEFAULT_FILENAME: &str = "upload";
/// Maximum stored filename length (characters).
pub const MAX_FILENAME_CHARS: usize = 255;

/// Attachment kind derived from the content type.
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

/// Outcome of the content type validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MimeCheck {
    /// Supported; the canonical content type to store.
    Supported(&'static str),
    /// CSV while `rag.allow_csv_upload` is off, or a type outside the allow-list.
    Unsupported,
}

/// Canonical type + file extension used for the provider filename.
const CANONICAL: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (DOCX, "docx"),
    (PPTX, "pptx"),
    (XLSX, "xlsx"),
    ("text/plain", "txt"),
    ("text/markdown", "md"),
    ("text/html", "html"),
    ("application/json", "json"),
    ("text/x-python", "py"),
    ("text/x-java", "java"),
    ("application/javascript", "js"),
    ("text/javascript", "js"),
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

/// Accepted aliases mapped to their canonical type.
const ALIASES: &[(&str, &str)] = &[
    ("text/x-markdown", "text/markdown"),
    ("application/x-javascript", "application/javascript"),
    ("text/ecmascript", "text/javascript"),
    ("application/ecmascript", "application/javascript"),
    ("text/typescript", "text/x-typescript"),
    ("application/x-typescript", "application/typescript"),
    ("text/x-script.python", "text/x-python"),
    ("application/x-python", "text/x-python"),
    ("application/x-python-code", "text/x-python"),
    ("text/python", "text/x-python"),
    ("text/x-java-source", "text/x-java"),
    ("text/java", "text/x-java"),
    ("text/rust", "text/x-rust"),
    ("text/x-rustsrc", "text/x-rust"),
    ("text/x-golang", "text/x-go"),
    ("text/go", "text/x-go"),
    ("text/x-c#", "text/x-csharp"),
    ("text/csharp", "text/x-csharp"),
    ("application/x-ruby", "text/x-ruby"),
    ("text/ruby", "text/x-ruby"),
    ("application/x-sql", "application/sql"),
    ("text/sql", "text/x-sql"),
    ("image/jpg", "image/jpeg"),
    ("image/pjpeg", "image/jpeg"),
];

/// Filename extension → content type used when the part says `application/octet-stream`.
const EXTENSIONS: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("docx", DOCX),
    ("pptx", PPTX),
    ("xlsx", XLSX),
    ("txt", "text/plain"),
    ("text", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("json", "application/json"),
    ("py", "text/x-python"),
    ("java", "text/x-java"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("cjs", "text/javascript"),
    ("ts", "application/typescript"),
    ("rs", "text/x-rust"),
    ("go", "text/x-go"),
    ("cs", "text/x-csharp"),
    ("rb", "text/x-ruby"),
    ("sql", "application/sql"),
    ("csv", "text/csv"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// Lower-cased essence (`type/subtype`) of a content type header value.
#[must_use]
pub fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Extension of `filename` (lower-cased, without the dot), if any.
#[must_use]
pub fn extension(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.contains(['/', '\\']) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Content type inferred from the filename extension (`None` for unknown extensions).
#[must_use]
pub fn infer_from_extension(filename: &str) -> Option<&'static str> {
    let ext = extension(filename)?;
    EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, ct)| *ct)
}

/// Content type to validate: the part's type, or the extension-inferred one for
/// `application/octet-stream` (unknown extensions keep `application/octet-stream`).
#[must_use]
pub fn effective_content_type(part_content_type: &str, filename: &str) -> String {
    let ct = essence(part_content_type);
    if ct == OCTET_STREAM {
        return infer_from_extension(filename).map_or(ct, str::to_owned);
    }
    ct
}

/// Validates a content type (already reduced with [`essence`]) against the allow-list.
/// CSV is accepted as `text/plain` only when `allow_csv` is on.
#[must_use]
pub fn check(content_type: &str, allow_csv: bool) -> MimeCheck {
    if content_type == "text/csv" || content_type == "application/csv" {
        return if allow_csv {
            MimeCheck::Supported("text/plain")
        } else {
            MimeCheck::Unsupported
        };
    }
    let canonical = ALIASES
        .iter()
        .find(|(a, _)| *a == content_type)
        .map_or(content_type, |(_, c)| *c);
    CANONICAL
        .iter()
        .find(|(c, _)| *c == canonical)
        .map_or(MimeCheck::Unsupported, |(c, _)| MimeCheck::Supported(c))
}

/// Kind of a supported content type.
#[must_use]
pub fn kind_of(content_type: &str) -> AttachmentKind {
    match content_type {
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => AttachmentKind::Image,
        _ => AttachmentKind::Document,
    }
}

/// `(for_file_search, for_code_interpreter)` derived from a supported content type.
#[must_use]
pub fn purposes_of(content_type: &str) -> (bool, bool) {
    match kind_of(content_type) {
        AttachmentKind::Image => (false, false),
        AttachmentKind::Document if content_type == XLSX => (false, true),
        AttachmentKind::Document => (true, false),
    }
}

/// File extension (without dot) used in the provider filename for a canonical type.
#[must_use]
pub fn provider_extension(content_type: &str) -> &'static str {
    CANONICAL
        .iter()
        .find(|(c, _)| *c == content_type)
        .map_or("bin", |(_, e)| *e)
}

/// Normalizes the client filename: default `upload` when missing/blank, at most 255
/// characters, keeping the extension when truncating.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_FILENAME);
    let count = name.chars().count();
    if count <= MAX_FILENAME_CHARS {
        return name.to_owned();
    }
    let ext = name
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
        .map(|(_, ext)| ext)
        .filter(|ext| ext.chars().count() < MAX_FILENAME_CHARS / 2);
    match ext {
        Some(ext) => {
            let keep = MAX_FILENAME_CHARS - ext.chars().count() - 1;
            let stem: String = name.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        None => name.chars().take(MAX_FILENAME_CHARS).collect(),
    }
}

#[cfg(test)]
#[path = "mime_tests.rs"]
mod mime_tests;
