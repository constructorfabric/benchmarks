//! File classification and purpose routing
//! (`cpt-cf-mini-chat-algo-attachments-classify-file`), the filename rule of
//! `cpt-cf-mini-chat-algo-attachments-read-multipart` and the effective size
//! limit of `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`.

use super::model::{AttachmentKind, Classification, ClassifyRules};
use crate::domain::error::{DisabledFeature, DomainError};

/// Maximum file name length in Unicode scalar values.
pub const FILENAME_MAX_CHARS: usize = 255;

/// File name used when the part has none (or an empty one).
pub const DEFAULT_FILENAME: &str = "upload";

/// One row of the MIME allowlist.
struct Entry {
    /// Accepted declared values, the stored form first.
    accepted: &'static [&'static str],
    /// Extensions (with the dot) an `application/octet-stream` upload is inferred from.
    octet_extensions: &'static [&'static str],
    /// Provider extension of the stored type.
    provider_extension: &'static str,
    kind: AttachmentKind,
}

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const OCTET_STREAM: &str = "application/octet-stream";

const fn entry(
    accepted: &'static [&'static str],
    octet_extensions: &'static [&'static str],
    provider_extension: &'static str,
    kind: AttachmentKind,
) -> Entry {
    Entry {
        accepted,
        octet_extensions,
        provider_extension,
        kind,
    }
}

const DOCUMENT: AttachmentKind = AttachmentKind::Document;
const IMAGE: AttachmentKind = AttachmentKind::Image;

static ALLOWLIST: &[Entry] = &[
    entry(&["application/pdf"], &[".pdf"], "pdf", DOCUMENT),
    entry(
        &["application/vnd.openxmlformats-officedocument.wordprocessingml.document"],
        &[".docx"],
        "docx",
        DOCUMENT,
    ),
    entry(
        &["application/vnd.openxmlformats-officedocument.presentationml.presentation"],
        &[".pptx"],
        "pptx",
        DOCUMENT,
    ),
    entry(&[XLSX], &[".xlsx"], "xlsx", DOCUMENT),
    entry(&["text/plain"], &[".txt"], "txt", DOCUMENT),
    entry(&["text/csv"], &[".csv"], "txt", DOCUMENT),
    entry(
        &["text/markdown", "text/x-markdown"],
        &[".md", ".markdown"],
        "md",
        DOCUMENT,
    ),
    entry(&["text/html"], &[".html", ".htm"], "html", DOCUMENT),
    entry(&["application/json"], &[".json"], "json", DOCUMENT),
    entry(
        &["text/x-python", "text/x-script.python"],
        &[".py"],
        "py",
        DOCUMENT,
    ),
    entry(
        &["text/x-java", "text/x-java-source"],
        &[".java"],
        "java",
        DOCUMENT,
    ),
    entry(
        &["text/javascript", "application/javascript"],
        &[".js", ".mjs"],
        "js",
        DOCUMENT,
    ),
    entry(
        &["application/typescript", "text/x-typescript"],
        &[".ts"],
        "ts",
        DOCUMENT,
    ),
    entry(&["text/x-rust"], &[".rs"], "rs", DOCUMENT),
    entry(&["text/x-golang", "text/x-go"], &[".go"], "go", DOCUMENT),
    entry(&["text/x-csharp"], &[".cs"], "cs", DOCUMENT),
    entry(&["text/x-ruby"], &[".rb"], "rb", DOCUMENT),
    entry(
        &["application/sql", "text/x-sql"],
        &[".sql"],
        "sql",
        DOCUMENT,
    ),
    entry(&["image/png"], &[".png"], "png", IMAGE),
    entry(
        &["image/jpeg", "image/jpg"],
        &[".jpg", ".jpeg"],
        "jpg",
        IMAGE,
    ),
    entry(&["image/webp"], &[".webp"], "webp", IMAGE),
    entry(&["image/gif"], &[".gif"], "gif", IMAGE),
];

/// The extension of `filename` with its dot and lower-cased (`""` without one).
fn extension_of(filename: &str) -> String {
    filename
        .rfind('.')
        .map(|at| filename[at..].to_ascii_lowercase())
        .unwrap_or_default()
}

/// Classifies a file by its declared content type and file name.
///
/// The declared type is lower-cased, its parameters after `;` are dropped and it
/// is trimmed; `application/octet-stream` is replaced by the type of the file
/// name extension (case-insensitive, an unknown or missing extension keeps it);
/// `text/csv` becomes `text/plain` when `allow_csv_upload`; aliases become the
/// stored form of the MIME allowlist (§1.4 of the FEATURE).
///
/// # Errors
///
/// [`DomainError::UnsupportedContentType`] for a type outside the allowlist (and
/// for CSV when it is not allowed); [`DomainError::FeatureDisabled`] with
/// `Images` for an image under `disable_images`;
/// [`DomainError::CodeInterpreterUnavailable`] for a spreadsheet when the code
/// interpreter is unavailable.
#[allow(clippy::trivially_copy_pass_by_ref)] // the rules are a borrowed view of the settings
pub fn classify_file(
    declared_content_type: &str,
    filename: &str,
    rules: &ClassifyRules,
) -> Result<Classification, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-normalize-type
    let mut declared = declared_content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-normalize-type
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-infer-from-extension
    if declared == OCTET_STREAM {
        let extension = extension_of(filename);
        if let Some(found) = ALLOWLIST
            .iter()
            .find(|entry| entry.octet_extensions.contains(&extension.as_str()))
        {
            found.accepted[0].clone_into(&mut declared);
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-infer-from-extension
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-csv
    if declared == "text/csv" {
        if !rules.allow_csv_upload {
            return Err(DomainError::UnsupportedContentType);
        }
        "text/plain".clone_into(&mut declared);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-csv
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-check-allowlist
    let entry = ALLOWLIST
        .iter()
        .find(|entry| entry.accepted.contains(&declared.as_str()))
        .ok_or(DomainError::UnsupportedContentType)?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-check-allowlist
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-kind
    let stored = entry.accepted[0];
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-kind
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-images-kill-switch
    if entry.kind == AttachmentKind::Image && rules.disable_images {
        return Err(DomainError::FeatureDisabled(DisabledFeature::Images));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-images-kill-switch
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-purposes
    let (for_file_search, mut for_code_interpreter) = match entry.kind {
        AttachmentKind::Image => (false, false),
        AttachmentKind::Document if stored == XLSX => (false, true),
        AttachmentKind::Document => (true, false),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-purposes
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-filter-code-interpreter
    if for_code_interpreter && !rules.code_interpreter_available {
        for_code_interpreter = false;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-filter-code-interpreter
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-return-ci-unavailable
    if entry.kind == AttachmentKind::Document && !for_file_search && !for_code_interpreter {
        return Err(DomainError::CodeInterpreterUnavailable);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-return-ci-unavailable
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-return-classification
    Ok(Classification {
        content_type: stored.to_owned(),
        kind: entry.kind,
        for_file_search,
        for_code_interpreter,
        extension: entry.provider_extension,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-classify-file:p1:inst-return-classification
}

/// The file name of a part: the raw name, or `upload` when absent or empty;
/// longer than 255 characters (Unicode scalar values) it is cut to 255 keeping
/// the extension (the text from the last `.` when that is not the first
/// character).
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-filename
    let raw = raw.unwrap_or_default();
    if raw.is_empty() {
        return DEFAULT_FILENAME.to_owned();
    }
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= FILENAME_MAX_CHARS {
        return raw.to_owned();
    }
    let dot = chars.iter().rposition(|c| *c == '.').filter(|at| *at > 0);
    match dot {
        Some(at) if chars.len() - at < FILENAME_MAX_CHARS => {
            let extension = &chars[at..];
            let keep = FILENAME_MAX_CHARS - extension.len();
            chars[..keep].iter().chain(extension.iter()).collect()
        }
        _ => chars[..FILENAME_MAX_CHARS].iter().collect(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-read-multipart:p1:inst-filename
}

/// The effective size limit in bytes of a kind: `configured_kb` x 1024, lowered
/// to `model_max_file_size_mb` x 1 048 576 when that is greater than 0 and
/// smaller.
#[must_use]
pub fn size_limit_bytes(configured_kb: u32, model_max_file_size_mb: u32) -> u64 {
    let configured = u64::from(configured_kb) * 1024;
    if model_max_file_size_mb > 0 {
        configured.min(u64::from(model_max_file_size_mb) * 1_048_576)
    } else {
        configured
    }
}
