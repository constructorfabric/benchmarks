//! The pure rules of the upload intake and the purpose routing
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake`,
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing`): MIME resolution with the
//! CSV remap and the extension inference, filename normalization, kind classification, purpose
//! flags, the effective per-file limit, the provider-side filename and the storage provider
//! resolution.

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::{ProviderKind, RagConfig};
use crate::domain::attachments::model::AttachmentKind;
use crate::domain::attachments::provider::StorageTarget;
use crate::domain::error::{AttachmentError, DomainError};
use crate::domain::provider_registry::ProviderRegistry;

/// The XLSX MIME type: routed to `code_interpreter` only.
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// The four image MIME types of the allowlist.
pub const IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// The purposes of an upload.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purposes {
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
}

/// Resolve the MIME type of the `file` part: the declared content type without parameters and in
/// lowercase; for `application/octet-stream` inferred from the filename extension (an unknown
/// extension keeps `application/octet-stream`); `text/csv` becomes `text/plain` when
/// `allow_csv`; the result must be in the allowlist.
///
/// # Errors
///
/// `Attachment(UnsupportedContentType)` for a type outside the allowlist (CSV while CSV is
/// disabled included).
pub fn resolve_mime(
    declared: &str,
    filename: &str,
    allow_csv: bool,
) -> Result<String, DomainError> {
    let unsupported = || DomainError::Attachment(AttachmentError::UnsupportedContentType);
    let mut mime = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-mime
    // a generic content type is inferred from the extension; an unknown extension keeps
    // `application/octet-stream`, which the allowlist then rejects
    if mime == "application/octet-stream"
        && let Some((_, ext)) = filename.rsplit_once('.')
        && let Some(inferred) = mime_of_extension(&ext.to_ascii_lowercase())
    {
        inferred.clone_into(&mut mime);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-mime
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-csv
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-csv-remap
    if mime == "text/csv" {
        return if allow_csv {
            Ok("text/plain".to_owned())
        } else {
            Err(unsupported())
        };
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-csv-remap
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-csv
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-unsupported
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-unsupported
    if ALLOWLIST.contains(&mime.as_str()) {
        Ok(mime)
    } else {
        Err(unsupported())
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-return-unsupported
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake:p1:inst-intake-unsupported
}

const ALLOWLIST: [&str; 21] = [
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    XLSX_MIME,
    "text/plain",
    "text/markdown",
    "text/html",
    "application/json",
    "text/x-python",
    "text/x-java-source",
    "text/javascript",
    "text/x-typescript",
    "text/x-rust",
    "text/x-go",
    "text/x-csharp",
    "text/x-ruby",
    "application/sql",
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
];

fn mime_of_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => XLSX_MIME,
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "py" => "text/x-python",
        "java" => "text/x-java-source",
        "js" => "text/javascript",
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

/// The filename of the row: `upload` when the part has none (or an empty one), truncated to 255
/// characters keeping the extension.
#[must_use]
pub fn normalize_filename(raw: Option<&str>) -> String {
    const MAX: usize = 255;
    let Some(name) = raw.filter(|name| !name.is_empty()) else {
        return "upload".to_owned();
    };
    if name.chars().count() <= MAX {
        return name.to_owned();
    }
    let extension = name
        .rfind('.')
        .map(|at| &name[at..])
        .filter(|ext| ext.chars().count() < MAX);
    let extension = extension.unwrap_or("");
    let keep = MAX - extension.chars().count();
    let stem: String = name.chars().take(keep).collect();
    format!("{stem}{extension}")
}

/// `image` for the four image MIME types, `document` for every other supported type.
#[must_use]
pub fn kind_of(mime: &str) -> AttachmentKind {
    if IMAGE_MIME_TYPES.contains(&mime) {
        AttachmentKind::Image
    } else {
        AttachmentKind::Document
    }
}

/// The purpose routing: XLSX is code-interpreter only, other documents are file-search only,
/// images have neither; `for_code_interpreter` is dropped when the `disable_code_interpreter`
/// kill switch is on or the chat model lacks `tool_support.code_interpreter`; a document left
/// without a purpose is rejected. `disable_file_search` and `tool_support.file_search` never
/// change the routing.
///
/// # Errors
///
/// `Attachment(CodeInterpreterUnavailable)` for a document without a remaining purpose.
pub fn route_purposes(
    mime: &str,
    kind: AttachmentKind,
    disable_code_interpreter: bool,
    model_supports_code_interpreter: bool,
) -> Result<Purposes, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-derive
    // XLSX is code-interpreter only, every other document is file-search only, images have
    // neither
    if kind == AttachmentKind::Image {
        return Ok(Purposes {
            for_file_search: false,
            for_code_interpreter: false,
        });
    }
    let mut purposes = Purposes {
        for_file_search: mime != XLSX_MIME,
        for_code_interpreter: mime == XLSX_MIME,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-derive
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-ci-unavailable
    if purposes.for_code_interpreter
        && (disable_code_interpreter || !model_supports_code_interpreter)
    {
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-ci-unavailable
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-ci-drop
        purposes.for_code_interpreter = false;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-ci-drop
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-no-purpose
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-return-ci-unavailable
    if !purposes.for_file_search && !purposes.for_code_interpreter {
        return Err(DomainError::Attachment(
            AttachmentError::CodeInterpreterUnavailable,
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-return-ci-unavailable
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-no-purpose
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-file-search-untouched
    // `disable_file_search` and `tool_support.file_search` are stream-time gates: neither
    // parameter of this function changes `for_file_search`
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-file-search-untouched
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-return
    Ok(purposes)
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing:p1:inst-route-return
}

/// The effective per-file byte limit: `min(kind limit of the rag section in KiB * 1024,
/// model max_file_size_mb * 1048576)`.
#[must_use]
pub fn effective_limit_bytes(
    kind: AttachmentKind,
    rag: &RagConfig,
    model_max_file_size_mb: u32,
) -> u64 {
    let kb = match kind {
        AttachmentKind::Document => rag.uploaded_file_max_size_kb,
        AttachmentKind::Image => rag.uploaded_image_max_size_kb,
    };
    (u64::from(kb) * 1024).min(u64::from(model_max_file_size_mb) * 1_048_576)
}

/// The provider-side filename `{chat_id}_{attachment_id}.{ext}`, `ext` being the extension of
/// `filename`.
#[must_use]
pub fn provider_filename(chat_id: Uuid, attachment_id: Uuid, filename: &str) -> String {
    // the extension is client input: only a short alphanumeric one is carried to the provider
    match filename.rsplit_once('.') {
        Some((_, ext))
            if !ext.is_empty()
                && ext.len() <= 16
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            format!("{chat_id}_{attachment_id}.{ext}")
        }
        _ => format!("{chat_id}_{attachment_id}"),
    }
}

/// The storage provider of a model: the entry named by the `rag_provider` of the model's
/// provider entry, or that entry itself, resolved for `tenant_id` (tenant overrides apply).
///
/// # Errors
///
/// `Internal` when no storage-capable provider resolves.
pub fn resolve_storage(
    registry: &ProviderRegistry,
    model_provider_id: &str,
    tenant_id: Uuid,
) -> Result<StorageTarget, DomainError> {
    let model_provider = registry.resolve(model_provider_id, tenant_id)?;
    let storage = match &model_provider.rag_provider {
        Some(rag) => registry.resolve(rag, tenant_id)?,
        None => model_provider,
    };
    Ok(StorageTarget {
        provider_id: storage.provider_id.clone(),
        upstream_alias: storage.upstream_alias.clone(),
        storage_kind: storage.storage_kind,
        api_version: storage.api_version.clone(),
        storage_backend: storage.storage_backend.clone(),
    })
}

/// The Anthropic Files target of the secondary copy: `Some` only when the model's provider kind
/// is `anthropic_messages`, resolved for `tenant_id`.
#[must_use]
pub fn resolve_secondary(
    registry: &ProviderRegistry,
    model_provider_id: &str,
    tenant_id: Uuid,
) -> Option<StorageTarget> {
    let provider = registry.resolve(model_provider_id, tenant_id).ok()?;
    if provider.kind != ProviderKind::AnthropicMessages {
        return None;
    }
    Some(StorageTarget {
        provider_id: provider.provider_id.clone(),
        upstream_alias: provider.upstream_alias.clone(),
        storage_kind: provider.storage_kind,
        api_version: provider.api_version.clone(),
        storage_backend: provider.storage_backend.clone(),
    })
}

#[cfg(test)]
#[path = "intake_tests.rs"]
mod intake_tests;
