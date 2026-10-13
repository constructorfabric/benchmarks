//! Upload MIME allowlist (PRD section 13): the stored content type, the attachment kind and the
//! purposes of an uploaded file.

use crate::domain::error::DomainError;
use crate::infra::db::AttachmentKind;

/// The validated type of an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMime {
    /// Canonical MIME type (lowercase, without parameters) stored on the attachment.
    pub content_type: String,
    pub kind: AttachmentKind,
    /// Indexed into the chat's vector store.
    pub for_file_search: bool,
    /// Given to the code interpreter at send time.
    pub for_code_interpreter: bool,
}

/// What the gear does with an allowlisted type.
#[derive(Clone, Copy)]
enum Use {
    /// A document indexed for `file_search`.
    Search,
    /// A document given to the code interpreter only (XLSX).
    CodeInterpreter,
    /// An image (multimodal input, no purpose flag).
    Image,
}

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Allowlisted MIME types: `(type, provider extension, use)`.
const ALLOWED: &[(&str, &str, Use)] = &[
    ("application/pdf", "pdf", Use::Search),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
        Use::Search,
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
        Use::Search,
    ),
    (XLSX, "xlsx", Use::CodeInterpreter),
    ("text/plain", "txt", Use::Search),
    ("text/markdown", "md", Use::Search),
    ("text/html", "html", Use::Search),
    ("application/json", "json", Use::Search),
    ("text/x-python", "py", Use::Search),
    ("text/x-java", "java", Use::Search),
    ("text/x-java-source", "java", Use::Search),
    ("text/javascript", "js", Use::Search),
    ("application/javascript", "js", Use::Search),
    ("application/typescript", "ts", Use::Search),
    ("text/x-typescript", "ts", Use::Search),
    ("text/x-rust", "rs", Use::Search),
    ("text/x-go", "go", Use::Search),
    ("text/x-csharp", "cs", Use::Search),
    ("text/x-ruby", "rb", Use::Search),
    ("application/sql", "sql", Use::Search),
    ("text/x-sql", "sql", Use::Search),
    ("image/png", "png", Use::Image),
    ("image/jpeg", "jpg", Use::Image),
    ("image/webp", "webp", Use::Image),
    ("image/gif", "gif", Use::Image),
];

const CSV: &str = "text/csv";
const OCTET_STREAM: &str = "application/octet-stream";

/// MIME type of a filename extension (lowercase), for `application/octet-stream` parts.
fn type_of_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX,
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "py" => "text/x-python",
        "java" => "text/x-java",
        "js" => "text/javascript",
        "ts" => "application/typescript",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "cs" => "text/x-csharp",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "csv" => CSV,
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

impl ResolvedMime {
    /// Extension of the provider filename (`{chat_id}_{attachment_id}.{ext}`).
    #[must_use]
    pub fn extension(&self) -> &'static str {
        ALLOWED
            .iter()
            .find(|(ct, _, _)| *ct == self.content_type)
            .map_or("bin", |(_, ext, _)| ext)
    }
}

/// Resolves the `file` part's content type (`None`: the part has none). Parameters
/// (`; charset=...`) and case are ignored; `application/octet-stream` is inferred from the
/// filename extension; `text/csv` is stored as `text/plain` when `allow_csv`.
///
/// # Errors
/// `MissingContentType`, `UnsupportedContentType`.
pub fn resolve(
    part_content_type: Option<&str>,
    filename: &str,
    allow_csv: bool,
) -> Result<ResolvedMime, DomainError> {
    let raw = part_content_type.ok_or(DomainError::MissingContentType)?;
    let essence = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut content_type = essence.as_str();
    if content_type == OCTET_STREAM {
        let ext = filename
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        content_type = type_of_extension(&ext).ok_or(DomainError::UnsupportedContentType)?;
    }
    if content_type == CSV {
        if !allow_csv {
            return Err(DomainError::UnsupportedContentType);
        }
        content_type = "text/plain";
    }
    let (ct, _, usage) = ALLOWED
        .iter()
        .find(|(ct, _, _)| *ct == content_type)
        .ok_or(DomainError::UnsupportedContentType)?;
    let (kind, for_file_search, for_code_interpreter) = match usage {
        Use::Search => (AttachmentKind::Document, true, false),
        Use::CodeInterpreter => (AttachmentKind::Document, false, true),
        Use::Image => (AttachmentKind::Image, false, false),
    };
    Ok(ResolvedMime {
        content_type: (*ct).to_owned(),
        kind,
        for_file_search,
        for_code_interpreter,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

    /// `(content type, kind, for_file_search, for_code_interpreter, extension)`.
    fn ok(
        ct: Option<&str>,
        filename: &str,
        allow_csv: bool,
    ) -> (String, &'static str, bool, bool, &'static str) {
        let m =
            resolve(ct, filename, allow_csv).unwrap_or_else(|e| panic!("{ct:?} {filename}: {e}"));
        let ext = m.extension();
        (
            m.content_type,
            m.kind.as_str(),
            m.for_file_search,
            m.for_code_interpreter,
            ext,
        )
    }

    fn doc(ct: &str, ext: &'static str) -> (String, &'static str, bool, bool, &'static str) {
        (ct.to_owned(), "document", true, false, ext)
    }

    fn image(ct: &str, ext: &'static str) -> (String, &'static str, bool, bool, &'static str) {
        (ct.to_owned(), "image", false, false, ext)
    }

    #[test]
    fn allowlisted_types_with_kind_purposes_and_extension() {
        let cases = [
            ("application/pdf", doc("application/pdf", "pdf")),
            (DOCX, doc(DOCX, "docx")),
            (PPTX, doc(PPTX, "pptx")),
            (XLSX, (XLSX.to_owned(), "document", false, true, "xlsx")),
            ("text/plain", doc("text/plain", "txt")),
            ("text/markdown", doc("text/markdown", "md")),
            ("text/html", doc("text/html", "html")),
            ("application/json", doc("application/json", "json")),
            ("text/x-python", doc("text/x-python", "py")),
            ("text/x-java", doc("text/x-java", "java")),
            ("text/x-java-source", doc("text/x-java-source", "java")),
            ("text/javascript", doc("text/javascript", "js")),
            (
                "application/javascript",
                doc("application/javascript", "js"),
            ),
            (
                "application/typescript",
                doc("application/typescript", "ts"),
            ),
            ("text/x-typescript", doc("text/x-typescript", "ts")),
            ("text/x-rust", doc("text/x-rust", "rs")),
            ("text/x-go", doc("text/x-go", "go")),
            ("text/x-csharp", doc("text/x-csharp", "cs")),
            ("text/x-ruby", doc("text/x-ruby", "rb")),
            ("application/sql", doc("application/sql", "sql")),
            ("text/x-sql", doc("text/x-sql", "sql")),
            ("image/png", image("image/png", "png")),
            ("image/jpeg", image("image/jpeg", "jpg")),
            ("image/webp", image("image/webp", "webp")),
            ("image/gif", image("image/gif", "gif")),
        ];
        for (ct, want) in cases {
            assert_eq!(ok(Some(ct), "whatever.bin", false), want, "{ct}");
        }
    }

    #[test]
    fn parameters_and_case_are_ignored() {
        assert_eq!(
            ok(Some("Text/Plain; charset=UTF-8"), "a.txt", false),
            doc("text/plain", "txt")
        );
        assert_eq!(
            ok(Some(" application/PDF ;name=x"), "a.pdf", false),
            doc("application/pdf", "pdf")
        );
    }

    #[test]
    fn csv_is_plain_text_only_when_allowed() {
        assert_eq!(
            ok(Some("text/csv"), "t.csv", true),
            doc("text/plain", "txt")
        );
        assert!(matches!(
            resolve(Some("text/csv"), "t.csv", false),
            Err(DomainError::UnsupportedContentType)
        ));
    }

    #[test]
    fn octet_stream_is_inferred_from_the_extension() {
        let os = Some("application/octet-stream");
        assert_eq!(ok(os, "report.PDF", false), doc("application/pdf", "pdf"));
        assert_eq!(ok(os, "photo.JPG", false), image("image/jpeg", "jpg"));
        assert_eq!(ok(os, "photo.jpeg", false), image("image/jpeg", "jpg"));
        assert_eq!(ok(os, "page.htm", false), doc("text/html", "html"));
        assert_eq!(ok(os, "main.rs", false), doc("text/x-rust", "rs"));
        assert_eq!(ok(os, "q.sql", false), doc("application/sql", "sql"));
        assert_eq!(
            ok(os, "sheet.xlsx", false),
            (XLSX.to_owned(), "document", false, true, "xlsx")
        );
        assert_eq!(ok(os, "t.csv", true), doc("text/plain", "txt"));
        for (filename, allow_csv) in [
            ("t.csv", false),
            ("archive.zip", false),
            ("noextension", false),
            ("trailing.", false),
            ("", false),
        ] {
            assert!(
                matches!(
                    resolve(os, filename, allow_csv),
                    Err(DomainError::UnsupportedContentType)
                ),
                "{filename}"
            );
        }
    }

    #[test]
    fn other_types_are_rejected() {
        for ct in [
            "application/x-msdownload",
            "image/svg+xml",
            "image/jpg",
            "application/zip",
            "text",
            "",
        ] {
            assert!(
                matches!(
                    resolve(Some(ct), "report.pdf", true),
                    Err(DomainError::UnsupportedContentType)
                ),
                "{ct}"
            );
        }
        assert!(matches!(
            resolve(None, "report.pdf", true),
            Err(DomainError::MissingContentType)
        ));
    }
}
