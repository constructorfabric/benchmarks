use super::*;

fn target(code_interpreter: bool, ks: KillSwitches) -> UploadTarget {
    let model: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m",
        "provider_model_id": "m",
        "display_name": "M",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000,
        "max_num_results": 5,
        "general_config": {"max_file_size_mb": 2, "tool_support": {"code_interpreter": code_interpreter}}
    }))
    .expect("entry");
    let now = time::OffsetDateTime::UNIX_EPOCH;
    UploadTarget {
        chat: chat::Model {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            user_id: Uuid::nil(),
            model: "m".to_owned(),
            title: None,
            is_temporary: false,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        },
        model,
        kill_switches: ks,
    }
}

fn classify_default(ct: &str, name: &str, t: &UploadTarget) -> DomainResult<UploadClass> {
    classify(ct, name, true, 25_600, 5_120, t)
}

#[test]
// The exact, case-preserved `.docx` suffix is what is asserted, not a file-type check.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn filename_normalization() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("  ")), "upload");
    assert_eq!(normalize_filename(Some("a.pdf")), "a.pdf");
    let long = format!("{}.docx", "x".repeat(300));
    let n = normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(n.ends_with(".docx"));
    let no_ext = "y".repeat(300);
    assert_eq!(normalize_filename(Some(&no_ext)).chars().count(), 255);
}

#[test]
fn extension_inference() {
    assert_eq!(mime_from_extension("a.PDF"), Some("application/pdf"));
    assert_eq!(mime_from_extension("a.xlsx"), Some(XLSX));
    assert_eq!(mime_from_extension("a.png"), Some("image/png"));
    assert_eq!(mime_from_extension("a.unknown"), None);
    assert_eq!(mime_from_extension("noext"), None);
}

#[test]
fn documents_images_and_xlsx() {
    let t = target(true, KillSwitches::default());
    let d = classify_default("text/plain; charset=utf-8", "a.txt", &t).unwrap();
    assert_eq!(
        (d.kind, d.for_file_search, d.for_code_interpreter),
        ("document", true, false)
    );
    assert_eq!(d.content_type, "text/plain");
    // model cap (2 MiB) below the configured document cap
    assert_eq!(d.limit_bytes, 2 * 1024 * 1024);
    let i = classify_default("image/png", "a.png", &t).unwrap();
    assert_eq!(
        (i.kind, i.for_file_search, i.for_code_interpreter),
        ("image", false, false)
    );
    assert_eq!(i.limit_bytes, 2 * 1024 * 1024);
    let x = classify_default("application/octet-stream", "s.xlsx", &t).unwrap();
    assert_eq!(x.content_type, XLSX);
    assert_eq!(
        (x.kind, x.for_file_search, x.for_code_interpreter),
        ("document", false, true)
    );
}

#[test]
fn image_limit_uses_image_cap() {
    let t = target(true, KillSwitches::default());
    let i = classify("image/jpeg", "a.jpg", true, 25_600, 100, &t).unwrap();
    assert_eq!(i.limit_bytes, 100 * 1024);
}

#[test]
fn csv_handling() {
    let t = target(true, KillSwitches::default());
    assert_eq!(
        classify_default("text/csv", "a.csv", &t)
            .unwrap()
            .content_type,
        "text/plain"
    );
    assert!(matches!(
        classify("text/csv", "a.csv", false, 25_600, 5_120, &t),
        Err(DomainError::UnsupportedContentType(_))
    ));
}

#[test]
fn unsupported_types() {
    let t = target(true, KillSwitches::default());
    for (ct, name) in [
        ("application/zip", "a.zip"),
        ("application/octet-stream", "a.bin"),
        ("video/mp4", "a.mp4"),
    ] {
        assert!(matches!(
            classify_default(ct, name, &t),
            Err(DomainError::UnsupportedContentType(_))
        ));
    }
}

#[test]
fn kill_switches_and_capabilities() {
    let ks = KillSwitches {
        disable_images: true,
        ..KillSwitches::default()
    };
    assert!(matches!(
        classify_default("image/gif", "a.gif", &target(true, ks)),
        Err(DomainError::FeatureDisabled(DisabledFeature::Images))
    ));
    let ks = KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    assert!(matches!(
        classify_default(XLSX, "a.xlsx", &target(true, ks)),
        Err(DomainError::CodeInterpreterUnavailable)
    ));
    assert!(matches!(
        classify_default(XLSX, "a.xlsx", &target(false, KillSwitches::default())),
        Err(DomainError::CodeInterpreterUnavailable)
    ));
}

#[test]
fn provider_filename_convention() {
    let t = target(true, KillSwitches::default());
    let row = |name: &str| attachment::Model {
        id: Uuid::from_u128(2),
        tenant_id: Uuid::nil(),
        chat_id: Uuid::from_u128(1),
        uploaded_by_user_id: Uuid::nil(),
        filename: name.to_owned(),
        content_type: "text/plain".to_owned(),
        size_bytes: 1,
        storage_backend: "openai".to_owned(),
        provider_file_id: None,
        status: "pending".to_owned(),
        error_code: None,
        attachment_kind: "document".to_owned(),
        for_file_search: true,
        for_code_interpreter: false,
        doc_summary: None,
        img_thumbnail: None,
        img_thumbnail_width: None,
        img_thumbnail_height: None,
        summary_model: None,
        summary_updated_at: None,
        cleanup_status: None,
        cleanup_attempts: 0,
        last_cleanup_error: None,
        cleanup_updated_at: None,
        created_at: t.chat.created_at,
        updated_at: t.chat.created_at,
        deleted_at: None,
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        secondary_provider_kind: None,
    };
    let c = Uuid::from_u128(1);
    let a = Uuid::from_u128(2);
    assert_eq!(
        provider_filename(&row("Report.PDF")),
        format!("{c}_{a}.pdf")
    );
    assert_eq!(provider_filename(&row("upload")), format!("{c}_{a}"));
    assert_eq!(provider_filename(&row("weird.na me")), format!("{c}_{a}"));
}
