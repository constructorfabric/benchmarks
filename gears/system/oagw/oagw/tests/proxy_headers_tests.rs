//! Header transformation of a proxy exchange.
//!
//! Covers `cpt-cf-oagw-algo-header-transform` and the acceptance rows of
//! `cpt-cf-oagw-dod-header-transformation`: the routing header never
//! forwarded, the eight hop-by-hop headers dropped, the three passthrough
//! modes with the shipped-schema default of `none`, the caller's
//! `Authorization` never a candidate, the `set`/`add`/`remove` rule order, the
//! `Host` replacement from the selected endpoint's authority, the plugin
//! mutations carried after the rules, and the 400 an invalid map is answered
//! with.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::data_plane::headers::{transform_request, transform_response};
use std::collections::BTreeMap;

use oagw::data_plane::validate::HOP_BY_HOP;
use oagw::domain::error::ErrorKind;
use oagw::domain::proxy::{EndpointChoice, PluginMutations, ProxyContext, SelectedEndpoint};
use uuid::Uuid;

use oagw::domain::upstream::{
    Endpoint, HeadersConfig, Passthrough, RequestHeaderRules, ResponseHeaderRules,
};
use oagw::domain::{EndpointHost, Scheme};

const TENANT: Uuid = Uuid::from_u128(0xa1);

/// An endpoint on one host and port.
fn endpoint(host: &str, port: Option<u16>) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("a valid endpoint host"),
        port,
    }
}

/// The selected endpoint the transform replaces `Host` with.
fn selected(host: &str, port: Option<u16>) -> SelectedEndpoint {
    SelectedEndpoint {
        endpoint: endpoint(host, port),
        choice: EndpointChoice::Only,
    }
}

/// A proxy context whose headers the caller states.
fn context(headers: &[(&str, &str)]) -> ProxyContext {
    ProxyContext {
        method: String::from("POST"),
        alias: String::from("api.example.com"),
        path_suffix: None,
        query: None,
        headers: headers
            .iter()
            .map(|(name, value)| (String::from(*name), String::from(*value)))
            .collect(),
        target_host: None,
        tenant_id: TENANT,
        subject_id: None,
        correlation: None,
    }
}

/// A header configuration whose request rules the caller states.
fn request_rules(
    set: &[(&str, &str)],
    add: &[(&str, &str)],
    remove: &[&str],
    passthrough: Option<Passthrough>,
    allowlist: &[&str],
) -> HeadersConfig {
    HeadersConfig {
        request: Some(RequestHeaderRules {
            set: set
                .iter()
                .map(|(name, value)| (String::from(*name), String::from(*value)))
                .collect(),
            add: add
                .iter()
                .map(|(name, value)| (String::from(*name), String::from(*value)))
                .collect(),
            remove: remove.iter().map(|name| String::from(*name)).collect(),
            passthrough,
            passthrough_allowlist: allowlist
                .iter()
                .map(|name| String::from(*name))
                .collect(),
        }),
        response: None,
    }
}

/// Reads one value out of the transformed map, case-insensitively.
fn value_of<'a>(map: &'a [(String, String)], name: &str) -> Option<&'a str> {
    map.iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Whether the map holds any entry under one name.
fn holds(map: &[(String, String)], name: &str) -> bool {
    value_of(map, name).is_some()
}

#[test]
fn the_default_passthrough_of_none_forwards_no_inbound_header() {
    let context = context(&[("x-custom", "value"), ("accept", "application/json")]);
    let outbound = transform_request(
        &context,
        &HeadersConfig::default(),
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert!(!holds(&outbound, "x-custom"));
    assert!(!holds(&outbound, "accept"));
    assert_eq!(value_of(&outbound, "host"), Some("upstream.example.com"));
}

#[test]
fn the_routing_header_is_never_forwarded_whatever_the_mode() {
    let context = context(&[("x-oagw-target-host", "us.vendor.com")]);
    let config = request_rules(
        &[],
        &[],
        &[],
        Some(Passthrough::All),
        &[],
    );
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert!(!holds(&outbound, "x-oagw-target-host"));
}

#[test]
fn no_hop_by_hop_header_is_ever_forwarded() {
    let headers: Vec<(&str, &str)> = HOP_BY_HOP
        .iter()
        .map(|name| (*name, "value"))
        .collect();
    let context = context(&headers);
    let config = request_rules(&[], &[], &[], Some(Passthrough::All), &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    for name in HOP_BY_HOP {
        assert!(!holds(&outbound, name), "{name} is hop-by-hop");
    }
}

#[test]
fn the_callers_authorization_is_never_a_passthrough_candidate() {
    let context = context(&[("authorization", "Bearer caller-token")]);
    let config = request_rules(&[], &[], &[], Some(Passthrough::All), &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert!(
        !holds(&outbound, "authorization"),
        "the platform middleware consumed the caller's credential"
    );
}

#[test]
fn the_allowlist_mode_forwards_exactly_the_names_it_lists() {
    let context = context(&[
        ("x-keep", "value"),
        ("x-drop", "value"),
        ("accept", "application/json"),
    ]);
    let config = request_rules(
        &[],
        &[],
        &[],
        Some(Passthrough::Allowlist),
        &["x-keep", "Accept"],
    );
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "x-keep"), Some("value"));
    assert!(!holds(&outbound, "x-drop"));
    assert_eq!(
        value_of(&outbound, "accept"),
        Some("application/json"),
        "the comparison is on the header name, which is case-insensitive"
    );
}

#[test]
fn an_allowlist_entry_that_names_nothing_forwards_nothing() {
    let context = context(&[("x-keep", "value"), ("x-drop", "value")]);
    let config = request_rules(&[], &[], &[], Some(Passthrough::Allowlist), &["x-absent"]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert!(!holds(&outbound, "x-keep"));
    assert!(!holds(&outbound, "x-drop"));
    assert_eq!(value_of(&outbound, "host"), Some("upstream.example.com"));
}

#[test]
fn the_all_mode_forwards_every_header_the_checks_keep() {
    let context = context(&[("x-one", "1"), ("x-two", "2")]);
    let config = request_rules(&[], &[], &[], Some(Passthrough::All), &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "x-one"), Some("1"));
    assert_eq!(value_of(&outbound, "x-two"), Some("2"));
}

#[test]
fn a_set_rule_overwrites_and_positions_itself() {
    let context = context(&[("x-existing", "old")]);
    let config = request_rules(&[("x-existing", "new")], &[], &[], Some(Passthrough::All), &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "x-existing"), Some("new"));
}

#[test]
fn a_set_rule_over_a_name_the_passthrough_did_not_forward_appends_one_entry() {
    let context = context(&[("x-set", "from-passthrough")]);
    let config = request_rules(&[("x-set", "from-rule")], &[], &[], None, &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    let entries: Vec<&str> = outbound
        .iter()
        .filter(|(name, _)| name == "x-set")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(entries, vec!["from-rule"]);
}

#[test]
fn an_add_rule_appends_a_second_value_under_one_name() {
    let context = context(&[("x-multi", "first")]);
    let config = request_rules(&[], &[("x-multi", "second")], &[], Some(Passthrough::All), &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    let entries: Vec<&str> = outbound
        .iter()
        .filter(|(name, _)| name == "x-multi")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(entries, vec!["first", "second"]);
}

#[test]
fn a_remove_rule_drops_every_entry_of_its_name() {
    let context = context(&[("x-gone", "1")]);
    let config = request_rules(
        &[("x-gone", "resurrected")],
        &[],
        &["x-gone"],
        Some(Passthrough::All),
        &[],
    );
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert!(!holds(&outbound, "x-gone"), "remove runs after set");
}

#[test]
fn the_host_replacement_carries_a_non_default_port() {
    let context = context(&[("host", "api.example.com")]);
    let config = request_rules(&[], &[], &[], None, &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", Some(8443)),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "host"), Some("upstream.example.com:8443"));
}

#[test]
fn the_host_replacement_omits_the_documented_default_port() {
    let context = context(&[]);
    let config = request_rules(&[], &[], &[], None, &[]);
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", Some(443)),
        &PluginMutations::default(),
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "host"), Some("upstream.example.com"));
}

#[test]
fn the_plugin_mutations_run_after_the_configuration_rules() {
    let context = context(&[("x-from-config", "config")]);
    let config = request_rules(&[("x-from-config", "config")], &[], &[], None, &[]);
    let mutations = PluginMutations {
        set: vec![(String::from("x-from-config"), String::from("plugin"))],
        removed: Vec::new(),
    };
    let outbound = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &mutations,
        None,
    )
    .expect("the map is valid");
    assert_eq!(value_of(&outbound, "x-from-config"), Some("plugin"));
}

#[test]
fn a_plugin_removal_drops_the_entry_the_rules_wrote() {
    let config = request_rules(&[("x-dropped", "config")], &[], &[], None, &[]);
    let mutations = PluginMutations {
        set: Vec::new(),
        removed: vec![String::from("x-dropped")],
    };
    let outbound = transform_request(
        &context(&[]),
        &config,
        &selected("upstream.example.com", None),
        &mutations,
        None,
    )
    .expect("the map is valid");
    assert!(!holds(&outbound, "x-dropped"));
}

#[test]
fn a_control_character_in_the_resulting_map_is_a_400() {
    let context = context(&[]);
    let config = request_rules(&[("x-evil", "value\r\ninjected")], &[], &[], None, &[]);
    let error = transform_request(
        &context,
        &config,
        &selected("upstream.example.com", None),
        &PluginMutations::default(),
        None,
    )
    .expect_err("the value is an injection vector");
    assert_eq!(error.kind, ErrorKind::ValidationError);
}

#[test]
fn an_empty_host_authority_is_a_400() {
    // The endpoint host is a validated domain type, so the empty authority only
    // reaches the map through a plugin mutation, which is unvalidated input.
    let mutations = PluginMutations {
        set: vec![(String::from("host"), String::new())],
        removed: Vec::new(),
    };
    let outcome = transform_request(
        &context(&[]),
        &request_rules(&[], &[], &[], None, &[]),
        &selected("upstream.example.com", None),
        &mutations,
        None,
    );
    let error = outcome.expect_err("an authority of blanks is not valid");
    assert_eq!(error.kind, ErrorKind::ValidationError);
}

#[test]
fn the_response_rules_run_in_the_same_set_add_remove_order() {
    let upstream_headers = vec![
        (String::from("content-length"), String::from("5")),
        (String::from("transfer-encoding"), String::from("chunked")),
        (String::from("x-kept"), String::from("value")),
    ];
    let config = HeadersConfig {
        request: None,
        response: Some(ResponseHeaderRules {
            set: BTreeMap::from([(
                String::from("x-kept"),
                String::from("replaced"),
            )]),
            add: BTreeMap::from([(
                String::from("x-added"),
                String::from("added"),
            )]),
            remove: vec![String::from("x-gone")],
        }),
    };
    let outbound = transform_response(&upstream_headers, &config, &PluginMutations::default());
    assert!(
        !holds(&outbound, "content-length") && !holds(&outbound, "transfer-encoding"),
        "the gateway re-states the framing itself"
    );
    assert_eq!(value_of(&outbound, "x-kept"), Some("replaced"));
    assert_eq!(value_of(&outbound, "x-added"), Some("added"));
}

#[test]
fn the_response_mutations_of_the_plugin_chain_run_last() {
    let upstream_headers = vec![(String::from("x-upstream"), String::from("value"))];
    let config = HeadersConfig::default();
    let mutations = PluginMutations {
        set: vec![(String::from("x-request-id"), String::from("abc"))],
        removed: Vec::new(),
    };
    let outbound = transform_response(&upstream_headers, &config, &mutations);
    assert_eq!(value_of(&outbound, "x-request-id"), Some("abc"));
    assert_eq!(value_of(&outbound, "x-upstream"), Some("value"));
}

#[test]
fn an_empty_upstream_header_set_answers_an_empty_map() {
    let outbound = transform_response(&[], &HeadersConfig::default(), &PluginMutations::default());
    assert!(outbound.is_empty());
}
