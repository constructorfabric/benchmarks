//! Upload MIME resolution and filename rules (D "Upload Attachment",
//! D "File Upload" kind / purpose table, P§13 allowlist).

use crate::domain::error::DomainError;
use crate::domain::models::AttachmentKind;

/// Validated MIME type of an upload with its kind, purposes and the file
/// extension used for the provider filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMime {
    pub mime: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// Canonical extension of `mime` (no dot).
    pub ext: String,
}

/// Maximum stored filename length (characters, D§3.7 `VARCHAR(255)`).
const MAX_FILENAME_CHARS: usize = 255;
/// Filename of a part without one.
const DEFAULT_FILENAME: &str = "upload";
/// Generic binary type: the MIME type is inferred from the extension.
const OCTET_STREAM: &str = "application/octet-stream";
const CSV: &str = "text/csv";
const PLAIN_TEXT: &str = "text/plain";

/// What an upload is used for.
#[derive(Clone, Copy)]
enum Purpose {
    FileSearch,
    CodeInterpreter,
    Image,
}

/// One allowed type: canonical MIME, extensions (first = canonical),
/// purpose. Aliases map to the canonical MIME of their row.
struct Allowed {
    mime: &'static str,
    aliases: &'static [&'static str],
    exts: &'static [&'static str],
    purpose: Purpose,
}

const fn row(
    mime: &'static str,
    aliases: &'static [&'static str],
    exts: &'static [&'static str],
    purpose: Purpose,
) -> Allowed {
    Allowed {
        mime,
        aliases,
        exts,
        purpose,
    }
}

/// P§13 allowlist (CSV is handled separately: stored as `text/plain`).
const ALLOWLIST: &[Allowed] = &[
    row("application/pdf", &[], &["pdf"], Purpose::FileSearch),
    row(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        &[],
        &["docx"],
        Purpose::FileSearch,
    ),
    row(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        &[],
        &["pptx"],
        Purpose::FileSearch,
    ),
    row(
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        &[],
        &["xlsx"],
        Purpose::CodeInterpreter,
    ),
    row(
        PLAIN_TEXT,
        &[],
        &["txt", "text", "log"],
        Purpose::FileSearch,
    ),
    row(
        "text/markdown",
        &["text/x-markdown"],
        &["md", "markdown"],
        Purpose::FileSearch,
    ),
    row("text/html", &[], &["html", "htm"], Purpose::FileSearch),
    row("application/json", &[], &["json"], Purpose::FileSearch),
    row(
        "text/x-python",
        &["text/x-script.python", "application/x-python"],
        &["py"],
        Purpose::FileSearch,
    ),
    row(
        "text/x-java",
        &["text/x-java-source"],
        &["java"],
        Purpose::FileSearch,
    ),
    row(
        "text/javascript",
        &["application/javascript", "application/x-javascript"],
        &["js", "mjs", "cjs"],
        Purpose::FileSearch,
    ),
    row(
        "application/typescript",
        &["text/x-typescript", "application/x-typescript"],
        &["ts"],
        Purpose::FileSearch,
    ),
    row("text/x-rust", &[], &["rs"], Purpose::FileSearch),
    row(
        "text/x-go",
        &["text/x-golang"],
        &["go"],
        Purpose::FileSearch,
    ),
    row("text/x-csharp", &[], &["cs"], Purpose::FileSearch),
    row(
        "text/x-ruby",
        &["application/x-ruby"],
        &["rb"],
        Purpose::FileSearch,
    ),
    row(
        "application/sql",
        &["text/x-sql", "application/x-sql"],
        &["sql"],
        Purpose::FileSearch,
    ),
    row("image/png", &[], &["png"], Purpose::Image),
    row(
        "image/jpeg",
        &["image/jpg"],
        &["jpg", "jpeg"],
        Purpose::Image,
    ),
    row("image/webp", &[], &["webp"], Purpose::Image),
    row("image/gif", &[], &["gif"], Purpose::Image),
];

fn resolved(a: &Allowed) -> ResolvedMime {
    ResolvedMime {
        mime: a.mime.to_owned(),
        kind: if matches!(a.purpose, Purpose::Image) {
            AttachmentKind::Image
        } else {
            AttachmentKind::Document
        },
        for_file_search: matches!(a.purpose, Purpose::FileSearch),
        for_code_interpreter: matches!(a.purpose, Purpose::CodeInterpreter),
        ext: a.exts[0].to_owned(),
    }
}

/// Lowercase extension of `filename` (no dot), if any.
fn extension(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Resolve the stored MIME type of an upload: parameters and case are
/// ignored; `application/octet-stream` is inferred from the filename
/// extension; `text/csv` is stored as `text/plain` when `allow_csv` is on.
///
/// # Errors
/// `UnsupportedContentType` for a type outside the allowlist (including an
/// octet stream with an unknown extension).
pub fn resolve(
    content_type: &str,
    filename: &str,
    allow_csv: bool,
) -> Result<ResolvedMime, DomainError> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let essence = if essence == OCTET_STREAM {
        let ext = extension(filename).ok_or(DomainError::UnsupportedContentType)?;
        if ext == "csv" {
            CSV.to_owned()
        } else {
            ALLOWLIST
                .iter()
                .find(|a| a.exts.contains(&ext.as_str()))
                .map(|a| a.mime.to_owned())
                .ok_or(DomainError::UnsupportedContentType)?
        }
    } else {
        essence
    };
    if essence == CSV {
        if !allow_csv {
            return Err(DomainError::UnsupportedContentType);
        }
        return ALLOWLIST
            .iter()
            .find(|a| a.mime == PLAIN_TEXT)
            .map(resolved)
            .ok_or(DomainError::UnsupportedContentType);
    }
    ALLOWLIST
        .iter()
        .find(|a| a.mime == essence || a.aliases.contains(&essence.as_str()))
        .map(resolved)
        .ok_or(DomainError::UnsupportedContentType)
}

/// Stored filename of an upload: the last path component (`/` and `\`
/// separators stripped), `upload` when missing or empty, truncated to 255
/// characters keeping the extension.
#[must_use]
pub fn sanitize_filename(raw: Option<&str>) -> String {
    let base = raw
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let base: String = base.chars().filter(|c| !c.is_control()).collect();
    if base.is_empty() || base == "." || base == ".." {
        return DEFAULT_FILENAME.to_owned();
    }
    if base.chars().count() <= MAX_FILENAME_CHARS {
        return base;
    }
    match base.rsplit_once('.') {
        // Keep the extension when it leaves room for a stem.
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() < MAX_FILENAME_CHARS - 1 => {
            let keep = MAX_FILENAME_CHARS - ext.chars().count() - 1;
            let stem: String = stem.chars().take(keep).collect();
            format!("{stem}.{ext}")
        }
        _ => base.chars().take(MAX_FILENAME_CHARS).collect(),
    }
}

#[cfg(test)]
#[path = "mime_tests.rs"]
mod tests;
