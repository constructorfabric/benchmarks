#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Test fixtures take owned `Value`s for readability and the assertion-heavy default
// tests are intentionally linear.
#![allow(clippy::needless_pass_by_value, clippy::cognitive_complexity)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`.

use serde_json::{Value, json};

use super::{
    AuthConfig, ConfigError, ProviderKind, ProviderMap, ProviderRegistry, StorageKind,
    validate_providers,
};

const OPENAI_KEY_SENTINEL: &str = "SENTINEL-AUTH-VALUE-7731";

fn provider() -> Value {
    json!({ "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" })
}

fn merge(mut base: Value, extra: Value) -> Value {
    let target = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        target.insert(k.clone(), v.clone());
    }
    base
}

fn parse(map: &Value) -> ProviderMap {
    serde_json::from_value(map.clone()).expect("provider map parses")
}

fn validate(map: &Value) -> Result<ProviderRegistry, ConfigError> {
    validate_providers(Some(&parse(map)))
}

fn ok(map: &Value) -> ProviderRegistry {
    validate(map).unwrap_or_else(|e| panic!("expected valid providers, got: {e}"))
}

fn err(map: &Value) -> String {
    validate(map)
        .expect_err("expected a validation error")
        .to_string()
}

fn one(entry: Value) -> Value {
    json!({ "p": entry })
}

// ---- defaults ----------------------------------------------------------------

#[test]
fn absent_providers_use_the_single_default_openai_entry() {
    let registry = validate_providers(None).expect("default entry is valid");

    assert_eq!(registry.providers.len(), 1);
    let p = &registry.providers[0];
    assert_eq!(p.id, "openai");
    assert_eq!(p.kind, ProviderKind::OpenaiResponses);
    assert_eq!(p.host, "api.openai.com");
    assert_eq!(p.port, 443);
    assert!(!p.use_http);
    assert_eq!(p.storage_kind, StorageKind::Openai);
    assert_eq!(p.api_path, "/v1/responses");
    assert_eq!(p.upstream_alias, "api.openai.com");
    assert!(
        p.auth_plugin_type.is_some(),
        "API-key auth plugin is configured"
    );
    assert!(
        p.auth_config
            .keys()
            .any(|k| p.auth_config.get(k) == Some("cred://openai-key")),
        "auth is the API key from cred://openai-key"
    );
    assert_eq!(registry.targets.len(), 1);
}

#[test]
fn minimal_entry_gets_documented_defaults() {
    let registry = ok(&one(provider()));

    let p = &registry.providers[0];
    assert_eq!(p.id, "p");
    assert_eq!(p.port, 443);
    assert!(!p.use_http);
    assert_eq!(p.api_path, "/v1/responses");
    assert_eq!(
        p.upstream_alias, "api.example.com",
        "alias defaults to host"
    );
    assert_eq!(
        p.storage_backend, "p",
        "storage_backend defaults to the provider id"
    );
    assert!(p.auth_plugin_type.is_none());
    assert!(p.auth_config.is_empty());
    assert!(p.rag_provider.is_none());
    assert!(p.tenant_overrides.is_empty());
}

#[test]
fn use_http_defaults_port_to_80_and_explicit_port_wins() {
    let http = ok(&one(merge(provider(), json!({ "use_http": true }))));
    assert_eq!(http.providers[0].port, 80);
    assert!(http.providers[0].use_http);

    let explicit = ok(&one(merge(
        provider(),
        json!({ "use_http": true, "port": 8080 }),
    )));
    assert_eq!(explicit.providers[0].port, 8080);

    let https = ok(&one(merge(provider(), json!({ "port": 8443 }))));
    assert_eq!(https.providers[0].port, 8443);
}

#[test]
fn explicit_alias_of_an_ip_literal_api_path_and_storage_backend_are_kept() {
    let registry = ok(&one(merge(
        provider(),
        json!({
            "host": "10.0.0.7",
            "upstream_alias": "Custom-Alias.",
            "api_path": "/openai/v1/responses",
            "storage_backend": "my-backend"
        }),
    )));

    let p = &registry.providers[0];
    assert_eq!(p.upstream_alias, "custom-alias", "normalized like OAGW");
    assert_eq!(p.api_path, "/openai/v1/responses");
    assert_eq!(p.storage_backend, "my-backend");
}

#[test]
fn empty_upstream_alias_falls_back_to_the_host() {
    let registry = ok(&one(merge(provider(), json!({ "upstream_alias": "" }))));
    assert_eq!(registry.providers[0].upstream_alias, "api.example.com");
}

#[test]
fn all_four_provider_kinds_are_accepted() {
    for kind in [
        "openai_responses",
        "openai_chat_completions",
        "vllm_responses",
        "anthropic_messages",
    ] {
        ok(&one(merge(provider(), json!({ "kind": kind }))));
    }
}

// ---- port --------------------------------------------------------------------

#[test]
fn port_zero_is_rejected_naming_the_key() {
    let message = err(&one(merge(provider(), json!({ "port": 0 }))));
    assert!(message.contains("providers.p.port"), "got: {message}");
}

// ---- host --------------------------------------------------------------------

#[test]
fn empty_host_is_rejected_naming_the_key() {
    let message = err(&one(merge(provider(), json!({ "host": "" }))));
    assert!(message.contains("providers.p.host"), "got: {message}");
}

#[test]
fn host_charset_accepts_letters_digits_dot_dash_underscore_colon_and_brackets() {
    for host in [
        "api.openai.com",
        "my_host-1.example.com:8443",
        "[::1]:8080",
        "HOST9",
    ] {
        ok(&one(merge(provider(), json!({ "host": host }))));
    }
}

#[test]
fn host_with_other_characters_is_rejected() {
    for host in [
        "a/b",
        "a?b",
        "a#b",
        "user@host",
        "a b",
        "host\n",
        "h;x",
        "h%2f",
    ] {
        let message = err(&one(merge(provider(), json!({ "host": host }))));
        assert!(
            message.contains("providers.p.host"),
            "host {host:?}: {message}"
        );
    }
}

#[test]
fn host_is_expanded_from_the_environment_before_the_charset_check() {
    temp_env::with_vars(
        [
            ("MC_TEST_HOST_OK", Some("azure.example.com")),
            ("MC_TEST_HOST_BAD", Some("bad/host")),
        ],
        || {
            let registry = ok(&one(merge(
                provider(),
                json!({ "host": "${MC_TEST_HOST_OK}" }),
            )));
            assert_eq!(registry.providers[0].host, "azure.example.com");
            assert_eq!(registry.providers[0].upstream_alias, "azure.example.com");

            let message = err(&one(merge(
                provider(),
                json!({ "host": "${MC_TEST_HOST_BAD}" }),
            )));
            assert!(message.contains("providers.p.host"), "got: {message}");
        },
    );
}

#[test]
fn unset_environment_variable_in_host_is_an_error_naming_the_key() {
    temp_env::with_var_unset("MC_TEST_HOST_UNSET", || {
        let message = err(&one(merge(
            provider(),
            json!({ "host": "${MC_TEST_HOST_UNSET}" }),
        )));
        assert!(message.contains("providers.p.host"), "got: {message}");
    });
}

// ---- auth_config -------------------------------------------------------------

#[test]
fn auth_config_values_are_expanded_and_never_debug_printed() {
    temp_env::with_var("MC_TEST_SECRET_REF", Some(OPENAI_KEY_SENTINEL), || {
        let registry = ok(&one(merge(
            provider(),
            json!({
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": { "header": "api-key", "prefix": "", "secret_ref": "${MC_TEST_SECRET_REF}" }
            }),
        )));

        let p = &registry.providers[0];
        assert_eq!(p.auth_config.get("secret_ref"), Some(OPENAI_KEY_SENTINEL));
        assert_eq!(p.auth_config.get("header"), Some("api-key"));
        assert_eq!(p.auth_config.get("prefix"), Some(""));
        assert!(!format!("{registry:?}").contains(OPENAI_KEY_SENTINEL));
        assert!(!format!("{:?}", p.auth_config).contains(OPENAI_KEY_SENTINEL));
        assert!(!format!("{:#?}", registry.targets).contains(OPENAI_KEY_SENTINEL));
    });
}

#[test]
fn unset_environment_variable_in_auth_config_is_an_error_without_values() {
    temp_env::with_var_unset("MC_TEST_AUTH_UNSET", || {
        let message = err(&one(merge(
            provider(),
            json!({ "auth_config": { "header": "api-key", "secret_ref": "${MC_TEST_AUTH_UNSET}" } }),
        )));
        assert!(
            message.contains("providers.p.auth_config"),
            "got: {message}"
        );
    });
}

// ---- storage kind / api_version ---------------------------------------------

#[test]
fn azure_storage_requires_a_non_blank_api_version() {
    let azure = merge(provider(), json!({ "storage_kind": "azure" }));

    let missing = err(&one(azure.clone()));
    assert!(
        missing.contains("providers.p.api_version"),
        "got: {missing}"
    );

    for blank in ["", "   "] {
        let message = err(&one(merge(azure.clone(), json!({ "api_version": blank }))));
        assert!(
            message.contains("providers.p.api_version"),
            "blank {blank:?}: {message}"
        );
    }

    let registry = ok(&one(merge(
        azure,
        json!({ "api_version": "2025-03-01-preview" }),
    )));
    assert_eq!(registry.providers[0].storage_kind, StorageKind::Azure);
    assert_eq!(
        registry.providers[0].api_version.as_deref(),
        Some("2025-03-01-preview")
    );
}

#[test]
fn azure_api_version_allows_only_letters_digits_dot_and_dash() {
    let azure = merge(provider(), json!({ "storage_kind": "azure" }));

    for good in ["2025-03-01-preview", "v1.0", "2024.10"] {
        ok(&one(merge(azure.clone(), json!({ "api_version": good }))));
    }
    for bad in ["2025/03", "a b", "v1?x=1", "v_1", "2025&x"] {
        let message = err(&one(merge(azure.clone(), json!({ "api_version": bad }))));
        assert!(
            message.contains("providers.p.api_version"),
            "version {bad:?}: {message}"
        );
    }
}

#[test]
fn openai_storage_does_not_require_api_version() {
    let registry = ok(&one(provider()));
    assert_eq!(registry.providers[0].api_version, None);
}

// ---- rag_provider ------------------------------------------------------------

#[test]
fn dangling_rag_provider_names_both_ids() {
    let message = err(&json!({
        "claude": merge(provider(), json!({ "kind": "anthropic_messages", "rag_provider": "missing_rag" })),
    }));

    assert!(message.contains("claude"), "got: {message}");
    assert!(message.contains("missing_rag"), "got: {message}");
}

#[test]
fn rag_provider_naming_an_existing_entry_is_valid() {
    let registry = ok(&json!({
        "claude": merge(provider(), json!({
            "kind": "anthropic_messages", "host": "api.anthropic.com", "rag_provider": "openai_rag"
        })),
        "openai_rag": merge(provider(), json!({ "host": "api.openai.com" })),
    }));

    assert_eq!(
        registry.providers[0].rag_provider.as_deref(),
        Some("openai_rag")
    );
}

// ---- tenant overrides --------------------------------------------------------

#[test]
fn tenant_override_must_set_host_or_upstream_alias() {
    let message = err(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": {} } }),
    )));
    assert!(
        message.contains("providers.p.tenant_overrides.00000000-0000-0000-0000-0000000000a1"),
        "got: {message}"
    );

    let only_auth = err(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "auth_config": { "secret_ref": "x" } } } }),
    )));
    assert!(
        only_auth.contains("providers.p.tenant_overrides.00000000-0000-0000-0000-0000000000a1"),
        "got: {only_auth}"
    );

    ok(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "host": "t1.example.com" } } }),
    )));
    ok(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "upstream_alias": "t1-alias" } } }),
    )));
}

#[test]
fn override_with_only_host_inherits_auth_and_defaults_alias_to_its_host() {
    let registry = ok(&one(merge(
        provider(),
        json!({
            "port": 8443,
            "use_http": false,
            "api_path": "/openai/v1/responses",
            "auth_plugin_type": "plugin.apikey",
            "auth_config": { "header": "api-key", "secret_ref": "root-key" },
            "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "host": "t1.example.com" } }
        }),
    )));

    let o = &registry.providers[0].tenant_overrides[0];
    assert_eq!(o.tenant_id, "00000000-0000-0000-0000-0000000000a1");
    assert_eq!(o.host, "t1.example.com");
    assert_eq!(o.upstream_alias, "t1.example.com:8443");
    assert_eq!(o.auth_plugin_type.as_deref(), Some("plugin.apikey"));
    assert_eq!(o.auth_config.get("secret_ref"), Some("root-key"));

    let t = registry
        .targets
        .iter()
        .find(|t| t.tenant_id.as_deref() == Some("00000000-0000-0000-0000-0000000000a1"))
        .unwrap();
    assert_eq!(t.provider_id, "p");
    assert_eq!(t.alias, "t1.example.com:8443");
    assert_eq!(t.host, "t1.example.com");
    assert_eq!(t.port, 8443, "port always from the entry");
    assert!(!t.use_http, "use_http always from the entry");
    assert_eq!(
        t.api_path, "/openai/v1/responses",
        "api_path always from the entry"
    );
    assert_eq!(t.auth_plugin_type.as_deref(), Some("plugin.apikey"));
    assert_eq!(t.auth_config.get("secret_ref"), Some("root-key"));
}

#[test]
fn override_fields_fall_back_individually() {
    let registry = ok(&one(merge(
        provider(),
        json!({
            "auth_plugin_type": "plugin.apikey",
            "auth_config": { "secret_ref": "root-key" },
            "tenant_overrides": {
                "00000000-0000-0000-0000-0000000000a3": { "upstream_alias": "tenant-alias" },
                "00000000-0000-0000-0000-0000000000a4": { "host": "a.example.com", "auth_config": { "secret_ref": "tenant-key" } },
                "00000000-0000-0000-0000-0000000000a5": { "host": "b.example.com", "auth_plugin_type": "plugin.other" }
            }
        }),
    )));

    let overrides = &registry.providers[0].tenant_overrides;
    let alias_only = &overrides[0];
    assert_eq!(
        alias_only.host, "api.example.com",
        "host falls back to the entry"
    );
    assert_eq!(
        alias_only.upstream_alias, "api.example.com",
        "a hostname endpoint ignores the configured alias and uses the derived one"
    );
    assert_eq!(alias_only.auth_config.get("secret_ref"), Some("root-key"));

    let auth_only = &overrides[1];
    assert_eq!(auth_only.auth_config.get("secret_ref"), Some("tenant-key"));
    assert_eq!(auth_only.auth_plugin_type.as_deref(), Some("plugin.apikey"));

    let plugin_only = &overrides[2];
    assert_eq!(
        plugin_only.auth_plugin_type.as_deref(),
        Some("plugin.other")
    );
    assert_eq!(plugin_only.auth_config.get("secret_ref"), Some("root-key"));
}

#[test]
fn override_host_gets_the_same_charset_check_and_expansion() {
    let bad = err(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "host": "t1/evil" } } }),
    )));
    assert!(
        bad.contains("providers.p.tenant_overrides.00000000-0000-0000-0000-0000000000a1.host"),
        "got: {bad}"
    );

    temp_env::with_vars(
        [
            ("MC_TEST_T1_HOST", Some("t1.example.com")),
            ("MC_TEST_T1_KEY", Some(OPENAI_KEY_SENTINEL)),
        ],
        || {
            let registry = ok(&one(merge(
                provider(),
                json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": {
                    "host": "${MC_TEST_T1_HOST}",
                    "auth_config": { "secret_ref": "${MC_TEST_T1_KEY}" }
                } } }),
            )));
            let o = &registry.providers[0].tenant_overrides[0];
            assert_eq!(o.host, "t1.example.com");
            assert_eq!(o.auth_config.get("secret_ref"), Some(OPENAI_KEY_SENTINEL));
            assert!(!format!("{registry:?}").contains(OPENAI_KEY_SENTINEL));
        },
    );
}

// ---- targets and alias groups -------------------------------------------------

#[test]
fn one_target_per_entry_and_override_in_configuration_order() {
    // Parsed from text: a `serde_json::Value` would sort the keys.
    let map: ProviderMap = serde_json::from_str(
        r#"{
            "zeta": {
                "kind": "openai_responses", "host": "zeta.example.com", "storage_kind": "openai",
                "tenant_overrides": {
                    "00000000-0000-0000-0000-0000000000a2": { "host": "t2.zeta.example.com" },
                    "00000000-0000-0000-0000-0000000000a1": { "host": "t1.zeta.example.com" }
                }
            },
            "alpha": { "kind": "openai_responses", "host": "alpha.example.com", "storage_kind": "openai" }
        }"#,
    )
    .expect("provider map parses");

    let registry = validate_providers(Some(&map)).expect("valid providers");

    let labels: Vec<String> = registry
        .targets
        .iter()
        .map(super::ProviderTarget::label)
        .collect();
    assert_eq!(
        labels,
        [
            "providers.zeta",
            "providers.zeta.tenant_overrides.00000000-0000-0000-0000-0000000000a2",
            "providers.zeta.tenant_overrides.00000000-0000-0000-0000-0000000000a1",
            "providers.alpha"
        ]
    );
    let ids: Vec<&str> = registry.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["zeta", "alpha"], "entries keep configuration order");
}

#[test]
fn targets_sharing_an_alias_with_identical_settings_are_valid() {
    let auth = json!({ "auth_plugin_type": "plugin.apikey", "auth_config": { "secret_ref": "k" } });

    let registry = ok(&json!({
        "a": merge(provider(), auth.clone()),
        "b": merge(provider(), auth),
    }));

    assert_eq!(registry.targets.len(), 2);
    assert_eq!(registry.targets[0].alias, registry.targets[1].alias);
}

#[test]
fn override_targeting_the_entry_alias_with_the_same_settings_is_valid() {
    ok(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "upstream_alias": "api.example.com" } } }),
    )));
}

#[test]
fn override_with_its_own_host_does_not_join_the_entry_alias_group() {
    let registry = ok(&one(merge(
        provider(),
        json!({
            "auth_config": { "secret_ref": "root-key" },
            "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": { "host": "t1.example.com", "auth_config": { "secret_ref": "t1-key" } } }
        }),
    )));

    assert_ne!(registry.targets[0].alias, registry.targets[1].alias);
}

fn conflict_fields(error: &ConfigError) -> Vec<String> {
    match error {
        ConfigError::AliasConflict { fields, .. } => fields.clone(),
        other => panic!("expected an alias conflict, got: {other}"),
    }
}

/// Two providers `a` and `b` forced into one alias, differing only in `delta`
/// applied to `b`.
fn conflict_between(delta: &Value) -> ConfigError {
    let a = merge(
        provider(),
        json!({
            "host": "10.0.0.1",
            "upstream_alias": "shared-alias",
            "auth_plugin_type": "plugin.apikey",
            "auth_config": { "secret_ref": "key-a" }
        }),
    );
    let b = merge(a.clone(), delta.clone());
    validate(&json!({ "a": a, "b": b })).expect_err("targets differ, so the alias group conflicts")
}

#[test]
fn alias_conflict_is_reported_for_each_differing_field() {
    for (field, delta) in [
        ("host", json!({ "host": "10.0.0.2" })),
        ("port", json!({ "port": 8443 })),
        ("use_http", json!({ "use_http": true, "port": 443 })),
        (
            "auth_plugin_type",
            json!({ "auth_plugin_type": "plugin.other" }),
        ),
        (
            "auth_config",
            json!({ "auth_config": { "secret_ref": "key-b" } }),
        ),
        ("api_path", json!({ "api_path": "/openai/v1/responses" })),
    ] {
        let error = conflict_between(&delta);
        let message = error.to_string();

        assert!(message.contains("shared-alias"), "{field}: {message}");
        assert!(message.contains("providers.a"), "{field}: {message}");
        assert!(message.contains("providers.b"), "{field}: {message}");
        assert!(
            conflict_fields(&error).iter().any(|f| f == field),
            "{field}: {error:?}"
        );
        assert!(
            !message.contains("key-a") && !message.contains("key-b"),
            "{field}: leaks auth values: {message}"
        );
        assert!(
            !format!("{error:?}").contains("key-b"),
            "{field}: leaks auth values in Debug"
        );
    }
}

#[test]
fn alias_conflict_never_leaks_auth_values_even_with_sentinels() {
    let a = merge(
        provider(),
        json!({ "auth_config": { "secret_ref": OPENAI_KEY_SENTINEL } }),
    );
    let b = merge(
        provider(),
        json!({ "auth_config": { "secret_ref": "SENTINEL-OTHER-8842" } }),
    );

    let error = validate(&json!({ "a": a, "b": b })).expect_err("auth_config differs");

    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(OPENAI_KEY_SENTINEL), "leak: {text}");
        assert!(!text.contains("SENTINEL-OTHER-8842"), "leak: {text}");
    }
    assert!(conflict_fields(&error).iter().any(|f| f == "auth_config"));
}

#[test]
fn alias_conflict_with_a_tenant_override_names_the_override_target() {
    let message = err(&one(merge(
        provider(),
        json!({
            "auth_config": { "secret_ref": "root-key" },
            "tenant_overrides": { "00000000-0000-0000-0000-0000000000a1": {
                "upstream_alias": "api.example.com",
                "auth_config": { "secret_ref": "t1-key" }
            } }
        }),
    )));

    assert!(message.contains("api.example.com"), "got: {message}");
    assert!(message.contains("providers.p"), "got: {message}");
    assert!(
        message.contains("providers.p.tenant_overrides.00000000-0000-0000-0000-0000000000a1"),
        "got: {message}"
    );
    assert!(message.contains("auth_config"), "got: {message}");
    assert!(
        !message.contains("root-key") && !message.contains("t1-key"),
        "leak: {message}"
    );
}

#[test]
fn every_later_target_is_compared_with_the_first_of_its_group() {
    let same = merge(provider(), json!({ "upstream_alias": "shared-alias" }));
    let different = merge(same.clone(), json!({ "api_path": "/other" }));

    let error = validate(&json!({ "first": same, "second": same, "third": different }))
        .expect_err("third differs from the first");

    let message = error.to_string();
    assert!(message.contains("providers.first"), "got: {message}");
    assert!(message.contains("providers.third"), "got: {message}");
    assert!(
        !message.contains("providers.second"),
        "second matches the first: {message}"
    );
    assert!(conflict_fields(&error).iter().any(|f| f == "api_path"));
}

#[test]
fn alias_conflict_lists_every_differing_field() {
    let error = conflict_between(&json!({ "host": "10.0.0.2", "api_path": "/x" }));

    let fields = conflict_fields(&error);
    assert!(fields.iter().any(|f| f == "host"), "{fields:?}");
    assert!(fields.iter().any(|f| f == "api_path"), "{fields:?}");
    assert!(!fields.iter().any(|f| f == "port"), "{fields:?}");
}

#[test]
fn auth_config_helpers_compare_as_key_value_maps() {
    let a = AuthConfig::from_pairs([("b", "2"), ("a", "1")]);
    let b = AuthConfig::from_pairs([("a", "1"), ("b", "2")]);

    assert_eq!(a, b);
    assert!(!format!("{a:?}").contains('2'));
}

// ---- OAGW-derived aliases ----------------------------------------------------

fn alias_of(entry: Value) -> String {
    ok(&one(merge(provider(), entry))).providers[0]
        .upstream_alias
        .clone()
}

#[test]
fn hostname_alias_is_the_host_on_the_standard_port() {
    assert_eq!(alias_of(json!({})), "api.example.com");
    assert_eq!(
        alias_of(json!({ "use_http": true })),
        "api.example.com",
        "80 is standard for http"
    );
    assert_eq!(alias_of(json!({ "port": 443 })), "api.example.com");
    assert_eq!(
        alias_of(json!({ "use_http": true, "port": 80 })),
        "api.example.com"
    );
}

#[test]
fn hostname_alias_carries_a_non_standard_port() {
    assert_eq!(alias_of(json!({ "port": 8443 })), "api.example.com:8443");
    assert_eq!(
        alias_of(json!({ "host": "localhost", "port": 18418, "use_http": true })),
        "localhost:18418"
    );
    assert_eq!(
        alias_of(json!({ "use_http": false, "port": 80 })),
        "api.example.com:80",
        "80 is not standard for https"
    );
    assert_eq!(
        alias_of(json!({ "use_http": true, "port": 443 })),
        "api.example.com:443",
        "443 is not standard for http"
    );
}

#[test]
fn hostname_alias_is_normalized_like_oagw() {
    assert_eq!(
        alias_of(json!({ "host": "API.Example.COM.", "port": 8443 })),
        "api.example.com:8443"
    );
}

#[test]
fn a_configured_alias_differing_from_the_derived_one_is_ignored_for_hostnames() {
    assert_eq!(
        alias_of(json!({ "upstream_alias": "my-alias", "port": 8443 })),
        "api.example.com:8443"
    );
    assert_eq!(
        alias_of(json!({ "upstream_alias": "API.example.com." })),
        "api.example.com"
    );
}

#[test]
fn ip_literal_takes_the_normalized_explicit_alias_or_the_derived_form() {
    assert_eq!(
        alias_of(json!({ "host": "10.0.0.5", "upstream_alias": "Internal-LLM." })),
        "internal-llm"
    );
    assert_eq!(alias_of(json!({ "host": "10.0.0.5" })), "10.0.0.5");
    assert_eq!(
        alias_of(json!({ "host": "10.0.0.5", "port": 8080, "use_http": true })),
        "10.0.0.5:8080"
    );
    assert_eq!(alias_of(json!({ "host": "[::1]" })), "::1");
}

#[test]
fn helper_functions_follow_the_oagw_rules() {
    use super::{derived_alias, effective_alias, is_ip_literal, normalize_alias};

    assert_eq!(derived_alias("Host.Example.", 443, false), "host.example");
    assert_eq!(
        derived_alias("host.example", 8080, true),
        "host.example:8080"
    );
    assert_eq!(normalize_alias("Foo.Bar.."), "foo.bar");
    assert!(is_ip_literal("[2001:db8::1]"));
    assert!(is_ip_literal("127.0.0.1"));
    assert!(!is_ip_literal("localhost"));
    assert_eq!(
        effective_alias("localhost", 18418, true, Some("custom")),
        "localhost:18418"
    );
    assert_eq!(
        effective_alias("127.0.0.1", 18418, true, Some("Custom")),
        "custom"
    );
}

#[test]
fn aliases_differing_only_by_case_or_trailing_dot_group_together() {
    let a = merge(
        provider(),
        json!({ "host": "10.0.0.1", "upstream_alias": "Shared", "auth_config": { "secret_ref": "k" } }),
    );
    let b = merge(
        provider(),
        json!({ "host": "10.0.0.1", "upstream_alias": "shared.", "auth_config": { "secret_ref": "other" } }),
    );

    let error = validate(&json!({ "a": a, "b": b })).expect_err("same normalized alias");

    assert!(conflict_fields(&error).iter().any(|f| f == "auth_config"));
}

// ---- tenant override keys ----------------------------------------------------

#[test]
fn tenant_override_key_must_be_a_uuid() {
    let message = err(&one(merge(
        provider(),
        json!({ "tenant_overrides": { "not-a-uuid": { "host": "t.example.com" } } }),
    )));

    assert!(
        message.contains("providers.p.tenant_overrides.not-a-uuid"),
        "got: {message}"
    );
}

#[test]
fn tenant_override_key_is_stored_in_canonical_form() {
    let registry = ok(&one(merge(
        provider(),
        json!({ "tenant_overrides": {
            "00000000000000000000000000000ABC": { "host": "a.example.com" },
            "00000000-0000-0000-0000-0000000000D1": { "host": "b.example.com" }
        } }),
    )));

    let ids: Vec<&str> = registry.providers[0]
        .tenant_overrides
        .iter()
        .map(|o| o.tenant_id.as_str())
        .collect();
    assert!(
        ids.contains(&"00000000-0000-0000-0000-000000000abc"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"00000000-0000-0000-0000-0000000000d1"),
        "{ids:?}"
    );
}

#[test]
fn two_keys_of_the_same_tenant_are_rejected() {
    let message = err(&one(merge(
        provider(),
        json!({ "tenant_overrides": {
            "00000000-0000-0000-0000-000000000abc": { "host": "a.example.com" },
            "00000000000000000000000000000ABC": { "host": "b.example.com" }
        } }),
    )));

    assert!(message.contains("tenant_overrides"), "got: {message}");
    assert!(message.contains("duplicates"), "got: {message}");
}
