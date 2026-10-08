//! Upload MIME allowlist, extension inference and filename normalization.

/// XLSX MIME type (code interpreter only).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Image MIME types.
pub const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Document MIME types routed to `file_search`.
pub const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-script.python",
    "application/x-python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "text/x-typescript",
    "application/typescript",
    "application/x-typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-golang",
    "text/x-csharp",
    "text/x-ruby",
    "application/x-ruby",
    "application/sql",
    "text/x-sql",
];

/// Lower-case MIME type without parameters; `image/jpg` becomes
/// `image/jpeg`.
#[must_use]
pub fn normalize(content_type: &str) -> String {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/jpg" | "image/pjpeg" => "image/jpeg".to_owned(),
        _ => base,
    }
}

/// MIME type of a filename extension.
#[must_use]
pub fn from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/x-typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Kind and purposes of an allowed MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MimeClass {
    Image,
    /// Indexed in the vector store.
    Document,
    /// Code interpreter only (XLSX).
    CodeFile,
}

/// Classify a normalized MIME type (`text/csv` must already be remapped).
#[must_use]
pub fn classify(mime: &str) -> Option<MimeClass> {
    if IMAGE_TYPES.contains(&mime) {
        Some(MimeClass::Image)
    } else if mime == XLSX {
        Some(MimeClass::CodeFile)
    } else if DOCUMENT_TYPES.contains(&mime) {
        Some(MimeClass::Document)
    } else {
        None
    }
}

/// Default filename and truncation to 255 characters keeping the
/// extension.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    let name = raw
        .map(|n| {
            // strip any path component
            n.rsplit(['/', '\\']).next().unwrap_or(n).trim().to_owned()
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "upload".to_owned());
    if name.chars().count() <= 255 {
        return name;
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() && ext.chars().count() < 32 => {
            let keep = 255 - ext.chars().count() - 1;
            let stem: String = stem.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        _ => name.chars().take(255).collect(),
    }
}

/// Extension (with dot) of a filename, if any.
#[must_use]
pub fn extension(filename: &str) -> Option<String> {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 16 && e.chars().all(|c| c.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_normalization_and_inference() {
        assert_eq!(normalize("Text/Plain; charset=utf-8"), "text/plain");
        assert_eq!(normalize("image/jpg"), "image/jpeg");
        assert_eq!(from_extension("a.PDF"), Some("application/pdf"));
        assert_eq!(from_extension("sheet.xlsx"), Some(XLSX));
        assert_eq!(from_extension("noext"), None);
        assert_eq!(from_extension("x.bin"), None);
        assert_eq!(classify("image/png"), Some(MimeClass::Image));
        assert_eq!(classify(XLSX), Some(MimeClass::CodeFile));
        assert_eq!(classify("application/pdf"), Some(MimeClass::Document));
        assert_eq!(classify("application/octet-stream"), None);
    }

    #[test]
    fn filename_defaults_and_truncation() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("  ")), "upload");
        assert_eq!(normalize_filename(Some("dir/a.txt")), "a.txt");
        let long = format!("{}.pdf", "x".repeat(300));
        let n = normalize_filename(Some(&long));
        assert_eq!(n.chars().count(), 255);
        assert_eq!(
            std::path::Path::new(&n)
                .extension()
                .and_then(|e| e.to_str()),
            Some("pdf")
        );
    }
}
