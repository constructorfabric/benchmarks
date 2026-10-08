//! Upload MIME allowlist, attachment kind and purpose routing.

/// XLSX MIME type (code interpreter only).
pub const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

const DOCUMENT_TYPES: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    XLSX,
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
    "application/x-javascript",
    "text/x-typescript",
    "application/typescript",
    "application/x-typescript",
    "text/x-rust",
    "text/rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/x-ruby",
    "application/sql",
    "text/x-sql",
];

/// Base MIME type in lowercase without parameters.
#[must_use]
pub fn base_type(ct: &str) -> String {
    ct.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// MIME type inferred from a filename extension.
#[must_use]
pub fn from_extension(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" | "text" | "log" => "text/plain",
        "csv" => "text/csv",
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
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

/// Resolve the effective MIME type of an upload part; `None` when the type
/// is not supported.
#[must_use]
pub fn resolve(part_type: &str, filename: &str, allow_csv: bool) -> Option<String> {
    let mut t = base_type(part_type);
    if t == "application/octet-stream" {
        let inferred = from_extension(filename)?;
        inferred.clone_into(&mut t);
    }
    if t == "image/jpg" {
        "image/jpeg".clone_into(&mut t);
    }
    if t == "text/csv" || t == "application/csv" {
        return allow_csv.then(|| "text/plain".to_owned());
    }
    (IMAGE_TYPES.contains(&t.as_str()) || DOCUMENT_TYPES.contains(&t.as_str())).then_some(t)
}

#[must_use]
pub fn is_image(mime: &str) -> bool {
    IMAGE_TYPES.contains(&mime)
}

/// `(for_file_search, for_code_interpreter)` of a supported MIME type.
#[must_use]
pub fn purposes(mime: &str) -> (bool, bool) {
    if is_image(mime) {
        (false, false)
    } else if mime == XLSX {
        (false, true)
    } else {
        (true, false)
    }
}

/// Default filename (`upload`) and 255-character truncation keeping the
/// extension.
#[must_use]
pub fn normalize_filename(name: Option<&str>) -> String {
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("upload");
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
mod tests {
    use super::*;

    #[test]
    fn resolves_types() {
        assert_eq!(
            resolve("application/pdf", "a.pdf", true).as_deref(),
            Some("application/pdf")
        );
        assert_eq!(
            resolve("text/plain; charset=utf-8", "a", true).as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            resolve("application/octet-stream", "x.XLSX", true).as_deref(),
            Some(XLSX)
        );
        assert_eq!(resolve("application/octet-stream", "x.bin", true), None);
        assert_eq!(
            resolve("text/csv", "a.csv", true).as_deref(),
            Some("text/plain")
        );
        assert_eq!(resolve("text/csv", "a.csv", false), None);
        assert_eq!(resolve("video/mp4", "a.mp4", true), None);
        assert_eq!(
            resolve("image/jpg", "a.jpg", true).as_deref(),
            Some("image/jpeg")
        );
    }

    #[test]
    fn purpose_routing() {
        assert_eq!(purposes("image/png"), (false, false));
        assert_eq!(purposes(XLSX), (false, true));
        assert_eq!(purposes("application/pdf"), (true, false));
    }

    #[test]
    fn filename_rules() {
        assert_eq!(normalize_filename(None), "upload");
        assert_eq!(normalize_filename(Some("  ")), "upload");
        let long = format!("{}.pdf", "a".repeat(300));
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
