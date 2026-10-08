//! Upload MIME allowlist, kind and purpose derivation.

pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    DOCX,
    PPTX,
    XLSX,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-script.python",
    "text/x-java",
    "text/x-java-source",
    "text/javascript",
    "application/javascript",
    "text/x-typescript",
    "application/typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "text/x-sql",
];

/// Strip parameters and normalize case (`Text/Plain; charset=utf-8` ->
/// `text/plain`).
#[must_use]
pub fn base_mime(ct: &str) -> String {
    ct.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// MIME type inferred from a filename extension (for
/// `application/octet-stream` parts).
#[must_use]
pub fn mime_from_filename(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => DOCX,
        "pptx" => PPTX,
        "xlsx" => XLSX,
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/x-typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Validate a MIME type against the allowlist. CSV is accepted as
/// `text/plain` when `allow_csv`. Returns the stored content type.
#[must_use]
pub fn validate_mime(ct: &str, allow_csv: bool) -> Option<String> {
    let base = base_mime(ct);
    if base == "text/csv" {
        return allow_csv.then(|| "text/plain".to_owned());
    }
    if IMAGE_TYPES.contains(&base.as_str()) || DOCUMENT_TYPES.contains(&base.as_str()) {
        Some(base)
    } else if base == "image/jpg" {
        Some("image/jpeg".to_owned())
    } else {
        None
    }
}

#[must_use]
pub fn is_image(ct: &str) -> bool {
    IMAGE_TYPES.contains(&base_mime(ct).as_str())
}

/// Purposes derived from the MIME type: `(for_file_search, for_code_interpreter)`.
#[must_use]
pub fn purposes(ct: &str) -> (bool, bool) {
    let base = base_mime(ct);
    if is_image(&base) {
        (false, false)
    } else if base == XLSX {
        (false, true)
    } else {
        (true, false)
    }
}

/// File extension for provider-side file names.
#[must_use]
pub fn extension_for(filename: &str, ct: &str) -> String {
    if let Some((_, ext)) = filename.rsplit_once('.')
        && !ext.is_empty()
        && ext.len() <= 10
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return ext.to_ascii_lowercase();
    }
    match base_mime(ct).as_str() {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/json" => "json",
        "text/markdown" => "md",
        "text/html" => "html",
        _ => "txt",
    }
    .to_owned()
}

/// Truncate a filename to at most 255 characters, keeping the extension.
#[must_use]
pub fn truncate_filename(name: &str) -> String {
    const MAX: usize = 255;
    if name.chars().count() <= MAX {
        return name.to_owned();
    }
    if let Some((stem, ext)) = name.rsplit_once('.') {
        let ext_len = ext.chars().count() + 1;
        if ext_len < MAX {
            let keep: String = stem.chars().take(MAX - ext_len).collect();
            return format!("{keep}.{ext}");
        }
    }
    name.chars().take(MAX).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        assert_eq!(
            validate_mime("application/pdf", true).as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            validate_mime("text/plain; charset=utf-8", true).as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            validate_mime("text/csv", true).as_deref(),
            Some("text/plain")
        );
        assert_eq!(validate_mime("text/csv", false), None);
        assert_eq!(validate_mime("application/octet-stream", true), None);
        assert_eq!(validate_mime("application/zip", true), None);
        assert_eq!(
            validate_mime("image/png", true).as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn kinds_and_purposes() {
        assert!(is_image("image/gif"));
        assert_eq!(purposes(XLSX), (false, true));
        assert_eq!(purposes("application/pdf"), (true, false));
        assert_eq!(purposes("image/png"), (false, false));
    }

    #[test]
    fn filename_helpers() {
        assert_eq!(mime_from_filename("a.PDF"), Some("application/pdf"));
        assert_eq!(mime_from_filename("noext"), None);
        let long = format!("{}.pdf", "a".repeat(400));
        let t = truncate_filename(&long);
        assert_eq!(t.chars().count(), 255);
        assert!(
            std::path::Path::new(&t)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        );
        assert_eq!(extension_for("x.TXT", "text/plain"), "txt");
    }
}
