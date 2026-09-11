//! The alias contract of the `oagw` gateway
//! (`cpt-cf-oagw-feature-alias-resolution`).
//!
//! The alias is the routing key of the proxy path `/oagw/v1/proxy/{alias}/...`,
//! and this module is the single place in the gear where an alias is computed
//! from an endpoint pool, normalized, checked for uniqueness or allowed to
//! change:
//!
//! * [`compute_derived_alias`] derives the alias of an endpoint pool
//!   (`cpt-cf-oagw-algo-alias-derivation`);
//! * [`normalize_alias`] and [`normalize_and_enforce`] normalize an alias and
//!   enforce the per-tenant `(tenant_id, alias)` uniqueness invariant
//!   (`cpt-cf-oagw-algo-alias-normalization`);
//! * [`resolve_alias_for_upstream`] is Flow A, the alias of an upstream at
//!   creation (`cpt-cf-oagw-flow-alias-derivation`);
//! * [`enforce_alias_update`] is Flow B, the immutable-alias transition table
//!   (`cpt-cf-oagw-flow-alias-update-enforcement`);
//! * [`AliasBinding`] tracks the binding of one upstream
//!   (`cpt-cf-oagw-state-alias-binding`).
//!
//! Every rejection is reported through the closed violation kinds of
//! [`crate::domain::error::ViolationKind`]: an alias override and a missing
//! alias as the `endpoint rule violation` kind, which the error mapping renders
//! as the 400 `ValidationError` row, and a per-tenant alias conflict as the
//! `already-exists` kind, whose 409 status stays the management-API layer's
//! decision. No kind, no variant and no mapping row is added here.
//!
//! The module registers no HTTP route: both entry points are pure functions
//! over the repository traits of `cpt-cf-oagw-dod-repository-traits`.
// @cpt-begin:cpt-cf-oagw-dod-alias-derivation-rules:p1:inst-full

use std::fmt;
use std::net::IpAddr;

use uuid::Uuid;

use crate::domain::error::{DomainError, Violation, ViolationKind, Violations};
use crate::domain::model::{Endpoint, EndpointScheme, Upstream, strip_trailing_dot};
use crate::domain::repo::UpstreamRepository;
use crate::domain::validation::validate_alias;

/// The HTTP port a derived alias omits.
const STANDARD_HTTP_PORT: i64 = 80;

/// The TLS-grade port a derived alias omits: HTTPS, WSS, WebTransport and gRPC
/// share it, and a `wt` endpoint on it is a standard port for derivation
/// purposes.
const STANDARD_TLS_PORT: i64 = 443;

/// The number of labels a registrable common suffix carries at least.
const MINIMUM_SUFFIX_LABELS: usize = 2;

/// The class of an endpoint pool that yields no derived alias.
///
/// The class names the reason the pool needs an explicit alias, which the
/// `endpoint rule violation` of a missing alias carries to the caller. A pool
/// that carries no hostname at all is classified [`DerivationFailure::NoHostname`]
/// rather than as a suffix divergence, since it shares no suffix with anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DerivationFailure {
    /// At least one endpoint host is an IP literal, and a pool that mixes
    /// hostnames with IP literals is classified the same way.
    IpBased,
    /// The hostnames of the pool share no registrable common suffix, such as
    /// `us.foo.com` with `eu.bar.com`, which share only the single label `com`.
    NoRegistrableCommonSuffix,
    /// The only suffix the hostnames share is a bare public suffix, such as
    /// `co.uk` for the pool `foo.co.uk` with `bar.co.uk`.
    BarePublicSuffix,
    /// The pool carries no hostname to compare a suffix over: an endpoint
    /// without a host, a host that normalizes to the empty name, or a pool with
    /// no endpoint at all.
    NoHostname,
}

impl DerivationFailure {
    /// The closed name of the failure class, as the FEATURE names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IpBased => "ip-based",
            Self::NoRegistrableCommonSuffix => "no registrable common suffix",
            Self::BarePublicSuffix => "bare public suffix",
            Self::NoHostname => "no hostname",
        }
    }
}

impl fmt::Display for DerivationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalizes one endpoint host: ASCII lowercase with the trailing dot of the
/// fully qualified form stripped, so `Api.OpenAI.COM.` and `api.openai.com`
/// are one hostname and therefore one routing key.
fn normalize_host(host: &str) -> String {
    strip_trailing_dot(host).to_ascii_lowercase()
}

/// Reports whether a host is an IP literal, IPv4 or IPv6.
fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Reports whether `port` is the standard port of `scheme` and is therefore
/// omitted from a derived alias: HTTP 80, and 443 for HTTPS, WSS,
/// WebTransport and gRPC.
fn is_standard_port(scheme: Option<EndpointScheme>, port: i64) -> bool {
    match scheme {
        Some(EndpointScheme::Http) => port == STANDARD_HTTP_PORT,
        Some(
            EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc,
        ) => port == STANDARD_TLS_PORT,
        // A scheme outside the closed scheme set declares no standard port, so
        // its port is always spelled out.
        None => false,
    }
}

/// The alias of `base`, appending `:port` only when the pool's shared port is
/// not the standard port of its scheme.
fn alias_with_port(base: &str, scheme: Option<EndpointScheme>, port: i64) -> String {
    if is_standard_port(scheme, port) {
        base.to_owned()
    } else {
        format!("{base}:{port}")
    }
}

/// The number of dot-separated labels of a domain name.
fn label_count(domain: &str) -> usize {
    domain.split('.').count()
}

/// The longest suffix of labels every hostname of the pool shares, as a dotted
/// name, and the empty name when the pool shares no label at all.
fn common_label_suffix(hostnames: &[String]) -> String {
    let reversed: Vec<Vec<&str>> = hostnames
        .iter()
        .map(|host| host.split('.').rev().collect())
        .collect();
    let Some(shortest) = reversed.iter().map(Vec::len).min() else {
        return String::new();
    };
    let mut shared: Vec<&str> = Vec::with_capacity(shortest);
    for index in 0..shortest {
        let label = reversed[0][index];
        if reversed.iter().all(|labels| labels[index] == label) {
            shared.push(label);
        } else {
            break;
        }
    }
    let mut common = String::new();
    for label in shared.iter().rev() {
        if !common.is_empty() {
            common.push('.');
        }
        common.push_str(label);
    }
    common
}

/// Classifies a pool that yields no registrable common suffix: a pool whose
/// longest shared label suffix is itself a public suffix of at least 2 labels,
/// such as `foo.co.uk` with `bar.co.uk`, is a bare-public-suffix pool, and
/// every other pool shares no registrable suffix at all.
fn classify_failure(hostnames: &[String]) -> DerivationFailure {
    let common = common_label_suffix(hostnames);
    let bare_public_suffix = !common.is_empty()
        && label_count(&common) >= MINIMUM_SUFFIX_LABELS
        && psl::suffix_str(&common) == Some(common.as_str());
    if bare_public_suffix {
        DerivationFailure::BarePublicSuffix
    } else {
        DerivationFailure::NoRegistrableCommonSuffix
    }
}

/// The registrable common suffix of a hostname pool: the registrable domain of
/// every host, the public suffix plus exactly one label, the notion of the
/// `psl` crate, identical across the pool, carrying at least
/// [`MINIMUM_SUFFIX_LABELS`] labels and not itself a bare public suffix.
///
/// A pool of hostnames that fails any of those conditions is classified by
/// [`classify_failure`], so the caller can tell a bare-public-suffix pool from
/// a pool without a registrable common suffix.
fn registrable_common_suffix(hostnames: &[String]) -> Result<String, DerivationFailure> {
    let mut registrable: Option<&str> = None;
    for host in hostnames {
        let Some(domain) = psl::domain_str(host) else {
            // A bare public suffix is no registrable domain, so no suffix of
            // the pool is registrable.
            return Err(classify_failure(hostnames));
        };
        if registrable.is_some_and(|found| found != domain) {
            return Err(classify_failure(hostnames));
        }
        registrable = Some(domain);
    }
    let Some(suffix) = registrable else {
        return Err(classify_failure(hostnames));
    };
    if label_count(suffix) < MINIMUM_SUFFIX_LABELS || psl::suffix_str(suffix) == Some(suffix) {
        return Err(classify_failure(hostnames));
    }
    Ok(suffix.to_owned())
}

/// The normalized hostnames of a pool, rejecting a pool that is not made of
/// hostnames.
fn normalized_hostnames(endpoints: &[Endpoint]) -> Result<Vec<String>, DerivationFailure> {
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-03
    // FOR EACH endpoint of the pool.
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-03
    let mut hostnames: Vec<String> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-02
        // The pool classification: any pool holding at least one IP literal,
        // including a pool that mixes hostnames with IP literals, is classified
        // IP-based and is therefore non-derivable, while a pool of hostnames
        // only is a single-hostname or a multi-hostname pool.
        // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-02
        let Some(host) = endpoint.host.as_deref() else {
            // A hostless endpoint is no hostname and derives nothing; the
            // endpoint rule of `cpt-cf-oagw-algo-endpoint-validation` rejects it
            // before a pool reaches derivation.
            return Err(DerivationFailure::NoHostname);
        };
        // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-02
        // IF any endpoint host is an IP literal, IPv4 or IPv6.
        if is_ip_literal(host) {
            // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-03
            // RETURN derivation-failure, because an IP-based pool is never
            // derivable and always requires an explicit alias.
            return Err(DerivationFailure::IpBased);
            // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-03
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-02
        // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-04
        // The host normalized to ASCII lowercase with any trailing dot stripped,
        // so every derivation reads the same value for the same pool.
        let normalized = normalize_host(host);
        // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-04
        if normalized.is_empty() {
            // An empty host carries no hostname, and the endpoint rule of
            // `cpt-cf-oagw-algo-endpoint-validation` rejects it before a pool
            // reaches derivation.
            return Err(DerivationFailure::NoHostname);
        }
        hostnames.push(normalized);
    }
    if hostnames.is_empty() {
        // An empty pool carries no hostname, so nothing can be derived; the
        // endpoint rule of `cpt-cf-oagw-algo-endpoint-validation` rejects an
        // empty endpoint list before a pool reaches derivation.
        return Err(DerivationFailure::NoHostname);
    }
    Ok(hostnames)
}

/// Derives the alias of an endpoint pool
/// (`cpt-cf-oagw-algo-alias-derivation`).
///
/// The pool is the already-validated endpoint list of an upstream, homogeneous
/// in scheme and port (`cpt-cf-oagw-algo-endpoint-validation`). A pool of one
/// hostname derives that hostname, appending `:port` only for a non-standard
/// port, and a pool of several hostnames derives their registrable common
/// suffix, the public suffix plus exactly one label, keeping the port the same
/// way. A pool that holds at least one IP literal, a mixed pool included, and a
/// pool whose hostnames share no registrable suffix are classified by
/// [`DerivationFailure`].
///
/// # Errors
/// Returns the [`DerivationFailure`] class of a pool that requires an explicit
/// alias: an IP-based pool, a bare-public-suffix pool and a pool whose
/// hostnames share no registrable common suffix.
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Result<String, DerivationFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-01
    // Every hostname of the list normalized to ASCII lowercase with trailing
    // dots stripped, over the endpoint shape
    // `cpt-cf-oagw-algo-endpoint-validation` already accepted.
    let hostnames = normalized_hostnames(endpoints)?;
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-01
    // The pool is homogeneous in scheme and port, so both are read from its
    // first entry.
    let (scheme, port) = endpoints.first().map_or((None, 0), |endpoint| {
        (endpoint.scheme_enum(), endpoint.port)
    });
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-04
    // IF the list holds exactly one normalized hostname.
    if let [host] = hostnames.as_slice() {
        // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-05
        // RETURN the hostname itself, appending `:port` only for a non-standard
        // port and omitting the standard ports HTTP 80 and HTTPS, WSS,
        // WebTransport and gRPC 443.
        return Ok(alias_with_port(host, scheme, port));
        // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-05
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-04
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-06
    // The registrable common suffix shared by all hostnames, judged against the
    // public-suffix list of the `psl` crate.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-06
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-07
    // IF no such registrable common suffix exists.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-07
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-08
    // RETURN derivation-failure, so a heterogeneous pool and a
    // bare-public-suffix pool both land in the explicit-alias branch of
    // `cpt-cf-oagw-flow-alias-derivation`.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-08
    let suffix = registrable_common_suffix(&hostnames)?;
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-09
    // IF the pool's shared port is a non-standard one, it is kept in the
    // derived alias; the standard ports are omitted.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-09
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-10
    // RETURN the `suffix:port` form, which keeps pools that share a domain
    // suffix but not a port from colliding on one routing key.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-10
    // @cpt-begin:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-11
    // RETURN the bare suffix otherwise.
    // @cpt-end:cpt-cf-oagw-algo-alias-derivation:p1:inst-da-11
    Ok(alias_with_port(&suffix, scheme, port))
}
// @cpt-end:cpt-cf-oagw-dod-alias-derivation-rules:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-alias-normalization:p1:inst-full

/// Normalizes an alias: ASCII lowercase with every trailing dot stripped, so
/// `Api.OpenAI.COM.` and `api.openai.com` are the same value at rest and at
/// resolution time.
///
/// The value is not validated here; a normalized alias that the alias pattern
/// rejects is reported by [`normalize_and_enforce`] as the `malformed-alias`
/// violation.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim_end_matches('.').to_ascii_lowercase()
}

/// Normalizes an alias and enforces the per-tenant
/// `(tenant_id, alias)` uniqueness invariant
/// (`cpt-cf-oagw-algo-alias-normalization`).
///
/// This is the only place an alias is normalized and checked, and the check
/// re-uses the alias rule of `cpt-cf-oagw-nfr-input-validation` instead of
/// re-declaring the alias pattern. The invariant is the
/// `UNIQUE (tenant_id, alias)` constraint of `cpt-cf-oagw-db-schema`, read
/// through [`UpstreamRepository::find_by_alias`]: the same alias in another
/// tenant is no conflict, and the key is scoped by the tenant id of the
/// upstream, which no caller of this module may widen.
///
/// `excluding` names the upstream whose own stored alias must not count as a
/// conflict, because Flow B re-checks an alias its upstream already holds.
///
/// The check writes nothing itself, so a caller that rejects on its outcome
/// aborts without a partial write.
///
/// # Errors
/// Returns the `malformed-alias` kind for an alias the alias pattern rejects,
/// and the `already-exists` kind for a per-tenant alias conflict, which names
/// the colliding tenant id and alias. Any other lookup failure is not this
/// invariant's to interpret and is propagated unchanged.
pub fn normalize_and_enforce(
    alias: &str,
    tenant_id: Uuid,
    excluding: Option<Uuid>,
    upstreams: &dyn UpstreamRepository,
) -> Result<String, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-01
    // The normalized alias, the value that is stored and resolved.
    let normalized = normalize_alias(alias);
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-01
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-02
    // The normalized value handed to the alias rule the domain model owns, so a
    // failing value is reported here as the malformed-alias violation and no
    // second pattern is declared.
    let mut violations = Violations::new();
    validate_alias(&normalized, &mut violations);
    violations.into_result(Upstream::FIELD_ORDER)?;
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-02
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-03
    // The uniqueness key is the `(tenant_id, alias)` pair of the caller's
    // tenant id and the normalized alias: the same alias in another tenant is a
    // different aggregate, and no scope above the tenant is consulted here.
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-03
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-04
    // TRY the uniqueness lookup for the `(tenant_id, alias)` key.
    match upstreams.find_by_alias(tenant_id, &normalized) {
        // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-05
        // The lookup goes through the tenant-scoped repository whose
        // duplicate-key semantics are the enforcement point of the invariant.
        // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-05
        // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-06
        // CATCH a duplicate key: an existing upstream of the same tenant that
        // carries the same alias, other than the one the check excludes.
        Ok(holder) if holder.id != excluding => {
            // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-07
            // The per-tenant alias conflict, naming the colliding tenant id and
            // alias, as the `already-exists` kind; the 409 status that renders
            // it is the management-API layer's decision.
            return Err(alias_conflict(tenant_id, &normalized));
            // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-07
        }
        // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-06
        // The upstream the check excludes holds the alias itself.
        Ok(_) => {}
        // A key no upstream of the tenant carries is free to take.
        Err(error) if error.kind() == Some(ViolationKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-04
    // @cpt-begin:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-08
    // RETURN the normalized alias together with the uniqueness decision.
    Ok(normalized)
    // @cpt-end:cpt-cf-oagw-algo-alias-normalization:p1:inst-nu-08
}

/// The per-tenant alias conflict, naming the colliding tenant id and alias.
fn alias_conflict(tenant_id: Uuid, alias: &str) -> DomainError {
    DomainError::already_exists(
        "alias",
        format!(
            "tenant '{tenant_id}' already holds an upstream with the alias '{alias}', so the \
             write is aborted and nothing is stored"
        ),
    )
}

/// The alias override of Flow A: a derivable pool always carries the alias it
/// derives, so a differing supplied alias is rejected.
fn alias_override(derived: &str, supplied: &str) -> DomainError {
    DomainError::from_violation(Violation::new(
        ViolationKind::EndpointRule,
        "alias",
        format!(
            "the supplied alias '{supplied}' overrides the alias '{derived}' that the endpoint \
             pool derives, and a hostname-based upstream always carries the derived routing key"
        ),
    ))
}

/// The missing alias of Flow A: a pool that derives nothing requires the
/// explicit alias the payload omits.
fn missing_alias(failure: DerivationFailure) -> DomainError {
    // A pool that carries no hostname diverges over no suffix, so the reason
    // names the missing host instead of a suffix divergence it cannot have.
    let reason = match failure {
        DerivationFailure::NoHostname => {
            "the endpoint pool carries no hostname to derive an alias from".to_owned()
        }
        other => format!("the endpoint pool is not derivable ({other})"),
    };
    DomainError::from_violation(Violation::new(
        ViolationKind::EndpointRule,
        "alias",
        format!(
            "{reason}, so an explicit alias is required and omitting the alias field is a \
             validation failure"
        ),
    ))
}

/// Re-labels a rejection of Flow B with the transition-table row that decided
/// the outcome, so every rejection of the flow names its row.
fn with_transition_rule(error: DomainError, rule: TransitionRule) -> DomainError {
    let mut violations = Violations::new();
    for violation in error.to_violations() {
        violations.push(Violation::new(
            violation.kind,
            violation.field,
            format!("{rule}: {}", violation.message),
        ));
    }
    violations.finish(Upstream::FIELD_ORDER)
}

/// The `endpoint rule violation` of a rejected transition-table row, naming the
/// row that decided the outcome.
fn rejected_update(rule: TransitionRule, message: &str) -> DomainError {
    DomainError::from_violation(Violation::new(
        ViolationKind::EndpointRule,
        "alias",
        format!("{rule}: {message}"),
    ))
}

/// The delete-and-re-create guidance every rejected endpoint change of Flow B
/// carries.
const IMMUTABLE_ALIAS_GUIDANCE: &str = "the alias is immutable once set, so delete and re-create \
                                        the upstream instead";
// @cpt-end:cpt-cf-oagw-dod-alias-normalization:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-alias-update-table:p1:inst-full

/// The row of the alias update transition table that decided the outcome of
/// [`enforce_alias_update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionRule {
    /// Both pools are derivable, so the proposed pool derives again.
    DerivableToDerivable,
    /// The proposed pool is no longer derivable, so the routing-key class
    /// itself would change.
    DerivableToNonDerivable,
    /// Both pools need an explicit alias, so the stored one is retained.
    NonDerivableToNonDerivable,
    /// The proposed pool derives again, where the stored one did not.
    NonDerivableToDerivable,
    /// The proposed endpoint set is the stored one, so no derivation runs.
    NoEndpointChange,
}

impl TransitionRule {
    /// The closed name of the table row, as FEATURE names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DerivableToDerivable => "derivable -> derivable",
            Self::DerivableToNonDerivable => "derivable -> non-derivable",
            Self::NonDerivableToNonDerivable => "non-derivable -> non-derivable",
            Self::NonDerivableToDerivable => "non-derivable -> derivable",
            Self::NoEndpointChange => "no endpoint change",
        }
    }
}

impl fmt::Display for TransitionRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of [`enforce_alias_update`]: the alias the upstream keeps,
/// unchanged by the update, and the table row that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasDecision {
    /// The alias the upstream keeps, exactly as stored.
    pub alias: Option<String>,
    /// The class of the stored binding, which never changes after creation.
    pub class: AliasClass,
    /// The transition-table row that decided the outcome.
    pub rule: TransitionRule,
    /// The alias the proposed pool derives, when it derives one at all.
    pub derived: Option<String>,
}

/// The normalized `(scheme, host, port)` entries of a pool, in a stable order.
fn normalized_endpoints(endpoints: &[Endpoint]) -> Vec<(String, String, i64)> {
    let mut entries: Vec<(String, String, i64)> = endpoints
        .iter()
        .map(|endpoint| {
            (
                endpoint.scheme.to_ascii_lowercase(),
                endpoint
                    .host
                    .as_deref()
                    .map(normalize_host)
                    .unwrap_or_default(),
                endpoint.port,
            )
        })
        .collect();
    entries.sort();
    entries
}

/// Reports whether the proposed endpoint set is the stored one, compared as a
/// set of normalized endpoints, so a pool resubmitted in another order, with
/// its hosts in the fully qualified form or in another letter case, is still
/// the unchanged endpoint set of the stored upstream.
fn same_endpoint_set(stored: &[Endpoint], proposed: &[Endpoint]) -> bool {
    normalized_endpoints(stored) == normalized_endpoints(proposed)
}

/// Flow B of the FEATURE: the alias of an upstream at replacement
/// (`cpt-cf-oagw-flow-alias-update-enforcement`).
///
/// The alias is immutable once set. The flow keeps the stored alias on every
/// allowed row of the transition table, re-derives it and re-checks it through
/// [`normalize_and_enforce`] only on the two rows where the alias value could
/// change, and rejects every row that would alter the routing key with the
/// delete-and-re-create guidance. No branch of the flow accepts a newly
/// supplied explicit alias: the only place an explicit alias enters the alias
/// contract is upstream creation, so a supplied alias is consulted only to
/// recognize the exact stored value, and a differing one is an alias override.
///
/// # Errors
/// Returns the `endpoint rule violation` kind for every rejected row of the
/// transition table, the `malformed-alias` kind for an alias the alias pattern
/// rejects, and the `already-exists` kind for a per-tenant alias conflict.
pub fn enforce_alias_update(
    existing: &Upstream,
    proposed_endpoints: &[Endpoint],
    supplied_alias: Option<&str>,
    tenant_id: Uuid,
    upstreams: &dyn UpstreamRepository,
) -> Result<AliasDecision, DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-01
    // The existing upstream with its stored alias and endpoint pool, and the
    // proposed replacement endpoint set with its optional alias.
    let stored_endpoints = existing
        .server
        .as_ref()
        .map(|server| server.endpoints.as_slice())
        .unwrap_or_default();
    // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-01
    // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-02
    // The derived-alias class of both pools: derivable when the computation
    // returns a value, non-derivable when it returns derivation-failure. The
    // class of the stored binding never changes after creation, so it is read
    // once from the stored pool and its stored alias.
    let stored_derivation = compute_derived_alias(stored_endpoints);
    let proposed_derivation = compute_derived_alias(proposed_endpoints);
    let stored = existing.alias.as_deref();
    let class = AliasClass::of_stored(&stored_derivation, stored);
    let stored_normalized = stored.map(normalize_alias);
    // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-02
    // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-03
    // IF the proposed endpoint set is unchanged.
    if same_endpoint_set(stored_endpoints, proposed_endpoints) {
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-04
        // IF the supplied alias is absent OR equals the stored alias, compared
        // normalized, so an exact-match alias is recognized however it was
        // written.
        if supplied_alias.is_none()
            || supplied_alias.map(normalize_alias).as_deref() == stored_normalized.as_deref()
        {
            // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-05
            // RETURN no-op, keeping the stored alias unchanged: an exact-match
            // alias on an unchanged endpoint set is tolerated, the stored alias
            // is not re-derived and therefore needs no re-validation, and the
            // per-tenant uniqueness of the stored alias was checked when it was
            // written.
            return Ok(AliasDecision {
                alias: existing.alias.clone(),
                class,
                rule: TransitionRule::NoEndpointChange,
                derived: stored_derivation.ok(),
            });
            // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-05
        }
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-04
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-06
        // ELSE the supplied alias differs from the stored one.
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-06
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-07
        // Reject the payload as an alias override, because an alias is never
        // changed while the endpoints stay the same.
        return Err(rejected_update(
            TransitionRule::NoEndpointChange,
            &format!(
                "the stored alias '{}' is kept while the endpoints stay the same, so the \
                 supplied alias '{}' is not accepted",
                stored.unwrap_or("no alias"),
                supplied_alias.unwrap_or("no alias")
            ),
        ));
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-07
    }
    // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-03
    // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-08
    // ELSE the transition table is applied to the class pair of the stored and
    // the proposed pool.
    let decision = match (&stored_derivation, &proposed_derivation) {
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-09
        // Derivable -> derivable: allowed only when the recomputed alias of the
        // proposed pool equals the stored one, else the delete-and-re-create
        // guidance.
        (Ok(_), Ok(recomputed)) => {
            let recomputed = normalize_alias(recomputed);
            if stored_normalized.as_deref() != Some(recomputed.as_str()) {
                return Err(rejected_update(
                    TransitionRule::DerivableToDerivable,
                    &format!(
                        "the endpoint change would alter the routing key from '{}' to \
                         '{recomputed}'; {IMMUTABLE_ALIAS_GUIDANCE}",
                        stored.unwrap_or("no alias")
                    ),
                ));
            }
            // The alias value could change here, so the normalized value is
            // re-checked against the per-tenant invariant before it is allowed.
            let alias = check_alias_of_update(
                &recomputed,
                tenant_id,
                existing.id(),
                TransitionRule::DerivableToDerivable,
                upstreams,
            )?;
            Ok(AliasDecision {
                alias: existing.alias.clone(),
                class,
                rule: TransitionRule::DerivableToDerivable,
                derived: Some(alias),
            })
        }
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-09
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-10
        // Derivable -> non-derivable: rejected always, even when an explicit
        // alias is provided, because the routing-key class itself would change.
        (Ok(_), Err(_)) => Err(rejected_update(
            TransitionRule::DerivableToNonDerivable,
            &format!(
                "the endpoint pool would stop being derivable, so the routing-key class of '{}' \
                 would change; {IMMUTABLE_ALIAS_GUIDANCE}",
                stored.unwrap_or("no alias")
            ),
        )),
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-10
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-11
        // Non-derivable -> non-derivable: the stored alias is retained as it
        // stands, which needs no re-validation, and a differing user-provided
        // alias is rejected.
        (Err(_), Err(_)) => match supplied_alias.map(normalize_alias) {
            None => Ok(AliasDecision {
                alias: existing.alias.clone(),
                class,
                rule: TransitionRule::NonDerivableToNonDerivable,
                derived: None,
            }),
            // The stored alias supplied again, compared normalized, is the
            // tolerated exact match.
            Some(supplied) if stored_normalized.as_deref() == Some(supplied.as_str()) => {
                Ok(AliasDecision {
                    alias: existing.alias.clone(),
                    class,
                    rule: TransitionRule::NonDerivableToNonDerivable,
                    derived: None,
                })
            }
            Some(supplied) => Err(rejected_update(
                TransitionRule::NonDerivableToNonDerivable,
                &format!(
                    "the stored alias '{}' is retained, so the supplied alias '{supplied}' is not \
                     accepted",
                    stored.unwrap_or("no alias")
                ),
            )),
        },
        // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-11
        // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-12
        // Non-derivable -> derivable: allowed only when the derived alias of the
        // proposed pool equals the existing one, else the delete-and-re-create
        // guidance.
        (Err(_), Ok(recomputed)) => {
            let recomputed = normalize_alias(recomputed);
            if stored_normalized.as_deref() != Some(recomputed.as_str()) {
                return Err(rejected_update(
                    TransitionRule::NonDerivableToDerivable,
                    &format!(
                        "the endpoint change would derive the routing key '{recomputed}' while \
                         the upstream is stored as '{}'; {IMMUTABLE_ALIAS_GUIDANCE}",
                        stored.unwrap_or("no alias")
                    ),
                ));
            }
            let alias = check_alias_of_update(
                &recomputed,
                tenant_id,
                existing.id(),
                TransitionRule::NonDerivableToDerivable,
                upstreams,
            )?;
            Ok(AliasDecision {
                alias: existing.alias.clone(),
                class,
                rule: TransitionRule::NonDerivableToDerivable,
                derived: Some(alias),
            })
        } // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-12
    };
    // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-08
    // @cpt-begin:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-13
    // RETURN allow, or reject together with the specific transition-table rule
    // that decided the outcome, which every rejection of this flow names.
    decision
    // @cpt-end:cpt-cf-oagw-flow-alias-update-enforcement:p1:inst-fb-13
}

/// The normalization hand-off of the two Flow B rows that could change the
/// alias value, re-labelled with the table row that decided the outcome.
fn check_alias_of_update(
    alias: &str,
    tenant_id: Uuid,
    excluding: Option<Uuid>,
    rule: TransitionRule,
    upstreams: &dyn UpstreamRepository,
) -> Result<String, DomainError> {
    normalize_and_enforce(alias, tenant_id, excluding, upstreams)
        .map_err(|error| with_transition_rule(error, rule))
}
// @cpt-end:cpt-cf-oagw-dod-alias-update-table:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-alias-resolution-api:p1:inst-full

/// The class of an alias: derived from the endpoint pool, or supplied by the
/// caller because the pool derives nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AliasClass {
    /// Derived from the endpoint pool by [`compute_derived_alias`].
    Derived,
    /// Supplied by the caller, because the pool derives nothing.
    Explicit,
}

impl AliasClass {
    /// The class of a stored alias: derived when the stored pool re-derives the
    /// stored value, and explicit in every other case, so an alias never
    /// changes class after creation.
    fn of_stored(derivation: &Result<String, DerivationFailure>, stored: Option<&str>) -> Self {
        match derivation {
            Ok(derived) if stored == Some(derived.as_str()) => Self::Derived,
            _ => Self::Explicit,
        }
    }

    /// The closed name of the class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::Explicit => "explicit",
        }
    }
}

impl fmt::Display for AliasClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved alias of an upstream, the value the proxy path addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAlias {
    /// The normalized alias, unique per `(tenant_id, alias)`.
    pub alias: String,
    /// Whether the alias was derived from the endpoint pool or supplied.
    pub class: AliasClass,
}

/// Flow A of the FEATURE: the alias of an upstream at creation
/// (`cpt-cf-oagw-flow-alias-derivation`).
///
/// A derivable pool always carries the alias it derives: a supplied alias equal
/// to the derived value is tolerated as an idempotent no-op and a differing one
/// is rejected as an alias override. A non-derivable pool requires the explicit
/// alias its caller supplies, and rejects an omitted `alias` field. The
/// resolved alias is normalized and checked for per-tenant uniqueness before it
/// is returned, so a caller persists exactly the value this flow validated.
///
/// # Errors
/// Returns the `endpoint rule violation` kind for an alias override
/// (`inst-fa-09`) and for a missing alias (`inst-fa-13`), the
/// `malformed-alias` kind for an alias the alias pattern rejects, and the
/// `already-exists` kind for a per-tenant alias conflict.
pub fn resolve_alias_for_upstream(
    endpoints: &[Endpoint],
    supplied_alias: Option<&str>,
    tenant_id: Uuid,
    upstreams: &dyn UpstreamRepository,
) -> Result<ResolvedAlias, DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-01
    // The endpoint set of the proposed upstream, the alias its caller supplied
    // and the caller's tenant id, the three inputs the flow reads.
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-01
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-05
    // The derived alias of the pool, computed over the normalized hostnames.
    let derivation = compute_derived_alias(endpoints);
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-05
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-06
    // IF derivation succeeded AND (no user alias was supplied OR the supplied
    // alias equals the derived value).
    let resolved = match (&derivation, supplied_alias) {
        (Ok(derived), None) => Ok((derived.clone(), AliasClass::Derived)),
        (Ok(derived), Some(supplied)) => {
            // The raw payload value, bound before the comparison value shadows
            // it: a rejection quotes the alias the caller sent, not the
            // normalized form the comparison reads.
            let raw_supplied = supplied;
            // The supplied alias is compared normalized, so the exact derived
            // value is recognized however it was written.
            let supplied = normalize_alias(supplied);
            if supplied == *derived {
                // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-07
                // RETURN the derived alias, tolerated silently for idempotency:
                // a hostname-based upstream always carries the derived routing
                // key.
                Ok((supplied, AliasClass::Derived))
                // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-07
            } else {
                // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-08
                // ELSE IF derivation succeeded AND the supplied alias differs
                // from the derived value.
                // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-08
                // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-09
                // Reject the payload as an alias override, naming the alias the
                // payload carried.
                Err(alias_override(derived, raw_supplied))
                // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-09
            }
        }
        (Err(_), Some(supplied)) => {
            // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-10
            // ELSE IF derivation failed AND a user alias is present.
            // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-10
            // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-11
            // RETURN the explicit alias as the routing key, recording its class
            // as explicit for the state machine of the FEATURE.
            Ok((supplied.to_owned(), AliasClass::Explicit))
            // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-11
        }
        (Err(failure), None) => {
            // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-12
            // ELSE derivation failed AND no user alias is present.
            // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-12
            // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-13
            // Reject the payload as a missing alias.
            Err(missing_alias(*failure))
            // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-13
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-06
    let (alias, class) = resolved?;
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-15
    // The resolved alias and the caller's tenant id handed to
    // `cpt-cf-oagw-algo-alias-normalization`, which normalizes it and enforces
    // the per-tenant `(tenant_id, alias)` invariant before the alias is
    // returned.
    let alias = normalize_and_enforce(&alias, tenant_id, None, upstreams)?;
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-15
    // @cpt-begin:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-14
    // RETURN the resolved alias with its class, or the rejection reason.
    Ok(ResolvedAlias { alias, class })
    // @cpt-end:cpt-cf-oagw-flow-alias-derivation:p1:inst-fa-14
}

/// The alias binding of one upstream
/// (`cpt-cf-oagw-state-alias-binding`).
///
/// The machine tracks the alias binding, not the resource lifecycle of
/// [`crate::domain::repo::ResourceLifecycle`], and the two move together: a
/// delete releases the alias with the upstream, and an alias is taken once per
/// tenant while the upstream that holds it still exists. A re-created upstream
/// with the same alias is a new binding that enters from `Unassigned`, not an
/// undelete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AliasBinding {
    /// No upstream holds the alias yet.
    Unassigned,
    /// A stored upstream carries the alias derived from its endpoint pool.
    Derived,
    /// A stored upstream carries the alias its caller supplied.
    Explicit,
    /// The upstream that carried the alias was deleted; terminal.
    Released,
}

impl AliasBinding {
    /// The state of an alias no upstream holds yet.
    #[must_use]
    pub const fn initial() -> Self {
        Self::Unassigned
    }

    /// The binding an alias enters when the upstream carrying it is stored: a
    /// derived alias from `Unassigned`, or an explicit one.
    #[must_use]
    pub fn from_class(class: AliasClass) -> Self {
        match class {
            AliasClass::Derived => Self::Derived,
            AliasClass::Explicit => Self::Explicit,
        }
    }

    /// Applies `to` to `self` per the state machine of the FEATURE.
    ///
    /// Returns `Some` with the reached state, or `None` when the transition is
    /// refused and the binding stays unchanged. Only `Unassigned` reaches
    /// `Derived` or `Explicit`, only a bound state reaches `Released`, and
    /// `Released` is terminal.
    #[must_use]
    pub fn transition(self, to: Self) -> Option<Self> {
        if self == to {
            // A state re-entered is a no-op, as in the resource lifecycle.
            return Some(self);
        }
        match (self, to) {
            // @cpt-begin:cpt-cf-oagw-state-alias-binding:p1:inst-sb-01
            // FROM Unassigned TO Derived WHEN an upstream with a derivable
            // endpoint pool is stored.
            (Self::Unassigned, Self::Derived) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-alias-binding:p1:inst-sb-01
            // @cpt-begin:cpt-cf-oagw-state-alias-binding:p1:inst-sb-02
            // FROM Unassigned TO Explicit WHEN an upstream that carries the
            // alias its caller supplied is stored.
            (Self::Unassigned, Self::Explicit) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-alias-binding:p1:inst-sb-02
            // @cpt-begin:cpt-cf-oagw-state-alias-binding:p1:inst-sb-03
            // FROM Derived TO Released WHEN the upstream that carries the alias
            // is deleted.
            (Self::Derived, Self::Released) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-alias-binding:p1:inst-sb-03
            // @cpt-begin:cpt-cf-oagw-state-alias-binding:p1:inst-sb-04
            // FROM Explicit TO Released WHEN the upstream that carries the
            // alias is deleted.
            (Self::Explicit, Self::Released) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-alias-binding:p1:inst-sb-04
            // A class change is refused in every case: an alias never changes
            // class after creation.
            _ => None,
        }
    }

    /// Binds the alias in the state its class names, from `Unassigned`.
    #[must_use]
    pub fn bind(self, class: AliasClass) -> Option<Self> {
        self.transition(Self::from_class(class))
    }

    /// Releases the binding when the upstream that carries the alias is
    /// deleted.
    #[must_use]
    pub fn release(self) -> Option<Self> {
        self.transition(Self::Released)
    }

    /// The closed name of the state, for diagnostics and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::Derived => "derived",
            Self::Explicit => "explicit",
            Self::Released => "released",
        }
    }
}

impl fmt::Display for AliasBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
// @cpt-end:cpt-cf-oagw-dod-alias-resolution-api:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-alias-storage:p1:inst-full

/// Stamps the resolved alias onto the aggregate the repository write persists
/// (`cpt-cf-oagw-dod-alias-storage`).
///
/// The alias is the normalized value [`resolve_alias_for_upstream`] validated
/// and checked for per-tenant uniqueness, so the value the proxy path addresses
/// is the value that was validated here, and the write enters the
/// [`AliasBinding`] state its class names. The alias is an ordinary field of
/// [`Upstream`], so the store that persists the aggregate persists the alias
/// with it, keyed by the `(tenant_id, alias)` index of
/// `cpt-cf-oagw-db-schema`.
#[must_use]
pub fn apply_resolved_alias(upstream: &mut Upstream, resolved: &ResolvedAlias) -> AliasBinding {
    upstream.alias = Some(resolved.alias.clone());
    AliasBinding::from_class(resolved.class)
}
// @cpt-end:cpt-cf-oagw-dod-alias-storage:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{PROTOCOL_HTTP, ServerConfig};
    use crate::domain::repo::UpstreamRepository;
    use crate::infra::storage::InMemoryStores;

    /// The tenant every test of the module scopes its upstreams to.
    fn tenant() -> Uuid {
        Uuid::from_u128(0x7f0a_0000_0000_0000_0000_0000_0000_0001)
    }

    /// A second tenant, for the cross-tenant cases.
    fn other_tenant() -> Uuid {
        Uuid::from_u128(0x7f0b_0000_0000_0000_0000_0000_0000_0001)
    }

    fn first_id() -> Uuid {
        Uuid::from_u128(0x0f0a_0000_0000_0000_0000_0000_0000_0001)
    }

    fn second_id() -> Uuid {
        Uuid::from_u128(0x0f0a_0000_0000_0000_0000_0000_0000_0002)
    }

    fn endpoint(scheme: &str, host: &str, port: i64) -> Endpoint {
        Endpoint::new(scheme, host, port)
    }

    fn vendor_pool() -> Vec<Endpoint> {
        vec![
            endpoint("https", "us.vendor.com", 443),
            endpoint("https", "eu.vendor.com", 443),
        ]
    }

    fn upstream(
        id: Uuid,
        tenant_id: Uuid,
        alias: Option<&str>,
        endpoints: Vec<Endpoint>,
    ) -> Upstream {
        Upstream {
            id: Some(id),
            tenant_id: Some(tenant_id),
            enabled: true,
            alias: alias.map(ToOwned::to_owned),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            server: Some(ServerConfig { endpoints }),
            ..Upstream::default()
        }
    }

    /// `cpt-cf-oagw-algo-alias-derivation`: one hostname on a standard port
    /// derives the hostname itself.
    #[test]
    fn a_single_hostname_on_a_standard_port_derives_the_hostname() {
        for (scheme, port) in [
            ("https", 443),
            ("http", 80),
            ("wss", 443),
            ("grpc", 443),
            ("wt", 443),
        ] {
            let pool = [endpoint(scheme, "api.example.com", port)];
            assert_eq!(
                compute_derived_alias(&pool).unwrap(),
                "api.example.com",
                "{scheme} on {port} is a standard port"
            );
        }
    }

    /// `cpt-cf-oagw-algo-alias-derivation`: a non-standard port is kept.
    #[test]
    fn a_non_standard_port_is_kept_in_the_derived_alias() {
        for (scheme, port) in [("https", 8443), ("http", 8080), ("wt", 9000), ("grpc", 444)] {
            let pool = [endpoint(scheme, "api.openai.com", port)];
            assert_eq!(
                compute_derived_alias(&pool).unwrap(),
                format!("api.openai.com:{port}"),
                "{scheme} on {port} is not a standard port"
            );
        }
    }

    /// `cpt-cf-oagw-algo-alias-derivation`: hosts are normalized first.
    #[test]
    fn hosts_are_normalized_before_derivation() {
        let pool = [endpoint("https", "Api.OpenAI.COM.", 443)];
        assert_eq!(compute_derived_alias(&pool).unwrap(), "api.openai.com");
    }

    /// `cpt-cf-oagw-algo-alias-derivation`: the registrable common suffix, not
    /// the longest shared label suffix.
    #[test]
    fn a_multi_hostname_pool_derives_the_registrable_common_suffix() {
        assert_eq!(compute_derived_alias(&vendor_pool()).unwrap(), "vendor.com");
        let nested = [
            endpoint("https", "a.us.vendor.com", 443),
            endpoint("https", "b.us.vendor.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&nested).unwrap(),
            "vendor.com",
            "the registrable domain, not the label suffix us.vendor.com"
        );
    }

    /// `cpt-cf-oagw-algo-alias-derivation`: the shared port is preserved.
    #[test]
    fn a_shared_non_standard_port_is_kept_in_the_derived_suffix() {
        let pool = [
            endpoint("https", "us.vendor.com", 8443),
            endpoint("https", "eu.vendor.com", 8443),
        ];
        assert_eq!(compute_derived_alias(&pool).unwrap(), "vendor.com:8443");
    }

    /// Acceptance criterion 3: a pool whose only common suffix is a bare public
    /// suffix derives nothing.
    #[test]
    fn a_bare_public_suffix_pool_is_not_derivable() {
        let pool = [
            endpoint("https", "foo.co.uk", 443),
            endpoint("https", "bar.co.uk", 443),
        ];
        assert_eq!(
            compute_derived_alias(&pool),
            Err(DerivationFailure::BarePublicSuffix)
        );
    }

    /// Acceptance criterion 4: a pool without a registrable common suffix
    /// derives nothing.
    #[test]
    fn a_pool_without_a_registrable_common_suffix_is_not_derivable() {
        let pool = [
            endpoint("https", "us.foo.com", 443),
            endpoint("https", "eu.bar.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&pool),
            Err(DerivationFailure::NoRegistrableCommonSuffix)
        );
    }

    /// Acceptance criterion 5: an IP-based pool, a mixed pool included, derives
    /// nothing.
    #[test]
    fn an_ip_based_pool_is_not_derivable() {
        let ips = [
            endpoint("http", "10.0.1.1", 80),
            endpoint("http", "10.0.1.2", 80),
        ];
        assert_eq!(compute_derived_alias(&ips), Err(DerivationFailure::IpBased));
        let mixed = [
            endpoint("http", "10.0.1.1", 80),
            endpoint("http", "api.vendor.com", 80),
        ];
        assert_eq!(
            compute_derived_alias(&mixed),
            Err(DerivationFailure::IpBased)
        );
        let ipv6 = [endpoint("https", "2001:db8::1", 443)];
        assert_eq!(
            compute_derived_alias(&ipv6),
            Err(DerivationFailure::IpBased)
        );
    }

    /// A pool of no hostname derives nothing, and is classified as carrying no
    /// hostname rather than as a pool whose hostnames diverge over a suffix.
    #[test]
    fn a_pool_of_no_hostname_derives_nothing() {
        assert_eq!(
            compute_derived_alias(&[]),
            Err(DerivationFailure::NoHostname)
        );
        let hostless = [endpoint("https", "", 443)];
        assert_eq!(
            compute_derived_alias(&hostless),
            Err(DerivationFailure::NoHostname)
        );
        let trailing_dot_only = [endpoint("https", ".", 443)];
        assert_eq!(
            compute_derived_alias(&trailing_dot_only),
            Err(DerivationFailure::NoHostname)
        );
    }

    /// The failure classes are named, for the rejection messages that carry
    /// them.
    #[test]
    fn the_derivation_failure_classes_are_named() {
        assert_eq!(DerivationFailure::IpBased.to_string(), "ip-based");
        assert_eq!(
            DerivationFailure::BarePublicSuffix.to_string(),
            "bare public suffix"
        );
        assert_eq!(
            DerivationFailure::NoRegistrableCommonSuffix.to_string(),
            "no registrable common suffix"
        );
        assert_eq!(DerivationFailure::NoHostname.to_string(), "no hostname");
    }

    /// `cpt-cf-oagw-algo-alias-normalization`: normalization is lowercase plus
    /// trailing dots, and idempotent.
    #[test]
    fn an_alias_is_normalized_to_lowercase_without_trailing_dots() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_alias("API.Example.COM"), "api.example.com");
        assert_eq!(normalize_alias("api.openai.com"), "api.openai.com");
        assert_eq!(normalize_alias("a."), "a");
        assert_eq!(
            normalize_alias(&normalize_alias("A.B.")),
            normalize_alias("a.b")
        );
    }

    /// Acceptance criterion 12: an alias the alias pattern rejects is reported
    /// as the `malformed-alias` kind, re-used from the domain validation.
    #[test]
    fn an_alias_that_fails_the_alias_pattern_is_a_malformed_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let pool = [endpoint("http", "10.0.0.1", 80)];
        for alias in ["", "-bad", "bad-", "has space", "a!b"] {
            let error =
                resolve_alias_for_upstream(&pool, Some(alias), tenant(), &upstreams).unwrap_err();
            assert_eq!(error.kind(), Some(ViolationKind::MalformedAlias), "{alias}");
            assert_eq!(error.field(), "alias", "{alias}");
        }
    }

    /// Flow A: a derivable pool always carries the derived alias, and an
    /// exactly matching one is an idempotent no-op.
    #[test]
    fn a_derivable_pool_carries_the_derived_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let resolved = resolve_alias_for_upstream(&vendor_pool(), None, tenant(), &upstreams)
            .expect("a derivable pool needs no alias");
        assert_eq!(resolved.alias, "vendor.com");
        assert_eq!(resolved.class, AliasClass::Derived);
        let exact =
            resolve_alias_for_upstream(&vendor_pool(), Some("Vendor.COM."), tenant(), &upstreams)
                .expect("the derived value is tolerated");
        assert_eq!(exact, resolved);
    }

    /// Flow A `inst-fa-09`: an alias override is rejected.
    #[test]
    fn a_differing_alias_on_a_derivable_pool_is_an_alias_override() {
        let upstreams = InMemoryStores::new().upstreams();
        let error =
            resolve_alias_for_upstream(&vendor_pool(), Some("my-service"), tenant(), &upstreams)
                .unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert_eq!(error.field(), "alias");
        assert!(error.message().contains("overrides the alias 'vendor.com'"));
    }

    /// Flow A `inst-fa-09`: the alias override names the alias the payload
    /// carried, not the normalized form it was compared against.
    #[test]
    fn an_alias_override_names_the_supplied_alias_as_the_payload_carried_it() {
        let upstreams = InMemoryStores::new().upstreams();
        let error = resolve_alias_for_upstream(
            &vendor_pool(),
            Some("My-Service.Example.COM."),
            tenant(),
            &upstreams,
        )
        .unwrap_err();

        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(
            error.message().contains("'My-Service.Example.COM.'"),
            "the override quotes the literal payload value: {}",
            error.message()
        );
    }

    /// Flow A `inst-fa-13`: a pool of no hostname requires the explicit alias,
    /// and the rejection names the missing host rather than a suffix divergence
    /// the pool has none of.
    #[test]
    fn a_pool_of_no_hostname_requires_an_explicit_alias_naming_the_missing_host() {
        let upstreams = InMemoryStores::new().upstreams();
        let error =
            resolve_alias_for_upstream(&[endpoint("https", "", 443)], None, tenant(), &upstreams)
                .unwrap_err();

        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert_eq!(error.field(), "alias");
        assert!(
            error.message().contains("no hostname"),
            "the rejection names the missing host: {}",
            error.message()
        );
        assert!(
            !error.message().contains("no registrable common suffix"),
            "a missing host is no suffix divergence: {}",
            error.message()
        );
    }

    /// Flow A `inst-fa-13`: an IP-based pool requires the explicit alias.
    #[test]
    fn an_ip_based_pool_requires_an_explicit_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let pool = [
            endpoint("http", "10.0.1.1", 80),
            endpoint("http", "10.0.1.2", 80),
        ];
        let error = resolve_alias_for_upstream(&pool, None, tenant(), &upstreams).unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(error.message().contains("ip-based"));
        let resolved =
            resolve_alias_for_upstream(&pool, Some("my-service"), tenant(), &upstreams).unwrap();
        assert_eq!(resolved.alias, "my-service");
        assert_eq!(resolved.class, AliasClass::Explicit);
    }

    /// Flow A: a bare-public-suffix pool and a pool without a registrable
    /// common suffix need an explicit alias too.
    #[test]
    fn a_non_derivable_pool_of_hostnames_needs_an_explicit_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let pools = [
            vec![
                endpoint("https", "foo.co.uk", 443),
                endpoint("https", "bar.co.uk", 443),
            ],
            vec![
                endpoint("https", "us.foo.com", 443),
                endpoint("https", "eu.bar.com", 443),
            ],
        ];
        for pool in &pools {
            let error = resolve_alias_for_upstream(pool, None, tenant(), &upstreams).unwrap_err();
            assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
            let resolved =
                resolve_alias_for_upstream(pool, Some("europe-gateway"), tenant(), &upstreams)
                    .unwrap();
            assert_eq!(resolved.class, AliasClass::Explicit);
            assert_eq!(resolved.alias, "europe-gateway");
        }
    }

    /// Acceptance criterion 10: a per-tenant alias conflict is reported and
    /// stores nothing.
    #[test]
    fn a_per_tenant_alias_conflict_is_reported_and_stores_nothing() {
        let stores = InMemoryStores::new();
        let upstreams = stores.upstreams();
        upstreams
            .insert(&upstream(
                first_id(),
                tenant(),
                Some("vendor.com"),
                vendor_pool(),
            ))
            .expect("the first upstream is valid");
        let error =
            resolve_alias_for_upstream(&vendor_pool(), None, tenant(), &upstreams).unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::AlreadyExists));
        assert!(error.message().contains("vendor.com"));
        assert!(error.message().contains(&tenant().to_string()));
        // The store is untouched: the alias is still held by the first upstream.
        assert!(upstreams.find_by_alias(tenant(), "vendor.com").is_ok());
        assert!(upstreams.find(tenant(), second_id()).is_err());
    }

    /// Acceptance criterion 10: the same alias in another tenant is no
    /// conflict.
    #[test]
    fn the_same_alias_in_another_tenant_is_no_conflict() {
        let stores = InMemoryStores::new();
        let upstreams = stores.upstreams();
        upstreams
            .insert(&upstream(
                first_id(),
                tenant(),
                Some("vendor.com"),
                vendor_pool(),
            ))
            .unwrap();
        let resolved =
            resolve_alias_for_upstream(&vendor_pool(), None, other_tenant(), &upstreams).unwrap();
        assert_eq!(resolved.alias, "vendor.com");
        assert_eq!(resolved.class, AliasClass::Derived);
        upstreams
            .insert(&upstream(
                second_id(),
                other_tenant(),
                Some("vendor.com"),
                vendor_pool(),
            ))
            .unwrap();
        assert!(
            upstreams
                .find_by_alias(other_tenant(), "vendor.com")
                .is_ok()
        );
    }

    /// Flow B row 1: derivable to derivable, keeping the alias.
    #[test]
    fn derivable_to_derivable_keeping_the_alias_is_allowed() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        let proposed = vec![
            endpoint("https", "apac.vendor.com", 443),
            endpoint("https", "emea.vendor.com", 443),
        ];
        let decision = enforce_alias_update(
            &existing,
            &proposed,
            Some("vendor.com"),
            tenant(),
            &upstreams,
        )
        .unwrap();
        assert_eq!(decision.alias.as_deref(), Some("vendor.com"));
        assert_eq!(decision.class, AliasClass::Derived);
        assert_eq!(decision.rule, TransitionRule::DerivableToDerivable);
        assert_eq!(decision.derived.as_deref(), Some("vendor.com"));
        // The alias field may be omitted as well: the stored alias is kept.
        let silent =
            enforce_alias_update(&existing, &proposed, None, tenant(), &upstreams).unwrap();
        assert_eq!(silent, decision);
    }

    /// Flow B row 1, rejected: an endpoint change that would alter the routing
    /// key carries the delete-and-re-create guidance.
    #[test]
    fn derivable_to_derivable_altering_the_alias_is_rejected() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        let proposed = vec![endpoint("https", "api.other.com", 443)];
        let error =
            enforce_alias_update(&existing, &proposed, None, tenant(), &upstreams).unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert_eq!(error.field(), "alias");
        assert!(error.message().contains("derivable -> derivable"));
        assert!(error.message().contains("delete and re-create"));
        assert!(error.message().contains("'vendor.com'"));
        assert!(error.message().contains("'api.other.com'"));
    }

    /// Flow B row 2: derivable to non-derivable is rejected always, even when
    /// an explicit alias is provided.
    #[test]
    fn derivable_to_non_derivable_is_rejected_even_with_an_explicit_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        let proposed = vec![endpoint("http", "10.0.0.1", 80)];
        for supplied in [None, Some("payments")] {
            let error = enforce_alias_update(&existing, &proposed, supplied, tenant(), &upstreams)
                .unwrap_err();
            assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
            assert!(
                error.message().contains("derivable -> non-derivable"),
                "{supplied:?}: {}",
                error.message()
            );
            assert!(error.message().contains("delete and re-create"));
        }
    }

    /// Flow B row 3: non-derivable to non-derivable retains the stored alias.
    #[test]
    fn non_derivable_to_non_derivable_retains_the_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(
            first_id(),
            tenant(),
            Some("payments"),
            vec![
                endpoint("http", "10.0.0.1", 80),
                endpoint("http", "10.0.0.2", 80),
            ],
        );
        let proposed = vec![
            endpoint("https", "foo.co.uk", 443),
            endpoint("https", "bar.co.uk", 443),
        ];
        let decision =
            enforce_alias_update(&existing, &proposed, None, tenant(), &upstreams).unwrap();
        assert_eq!(decision.alias.as_deref(), Some("payments"));
        assert_eq!(decision.class, AliasClass::Explicit);
        assert_eq!(decision.rule, TransitionRule::NonDerivableToNonDerivable);
        assert_eq!(decision.derived, None);
        let exact = enforce_alias_update(
            &existing,
            &proposed,
            Some("Payments."),
            tenant(),
            &upstreams,
        )
        .unwrap();
        assert_eq!(exact.alias.as_deref(), Some("payments"));
        let error = enforce_alias_update(&existing, &proposed, Some("other"), tenant(), &upstreams)
            .unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(error.message().contains("non-derivable -> non-derivable"));
    }

    /// Flow B row 4: non-derivable to derivable, only for the existing alias.
    #[test]
    fn non_derivable_to_derivable_is_allowed_only_for_the_existing_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(
            first_id(),
            tenant(),
            Some("payments.vendor.com"),
            vec![endpoint("http", "10.0.0.1", 80)],
        );
        let same = vec![endpoint("https", "payments.vendor.com", 443)];
        let decision = enforce_alias_update(&existing, &same, None, tenant(), &upstreams).unwrap();
        assert_eq!(decision.rule, TransitionRule::NonDerivableToDerivable);
        assert_eq!(decision.alias.as_deref(), Some("payments.vendor.com"));
        assert_eq!(decision.derived.as_deref(), Some("payments.vendor.com"));
        let other = vec![endpoint("https", "vendor.com", 443)];
        let error =
            enforce_alias_update(&existing, &other, None, tenant(), &upstreams).unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(error.message().contains("non-derivable -> derivable"));
        assert!(error.message().contains("delete and re-create"));
    }

    /// Flow B: an unchanged endpoint set tolerates the exact stored alias and
    /// rejects a differing one.
    #[test]
    fn an_unchanged_endpoint_set_tolerates_only_the_stored_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        let reordered = vec![
            endpoint("https", "Eu.Vendor.Com.", 443),
            endpoint("https", "us.vendor.com", 443),
        ];
        let decision = enforce_alias_update(
            &existing,
            &reordered,
            Some("Vendor.COM."),
            tenant(),
            &upstreams,
        )
        .unwrap();
        assert_eq!(decision.rule, TransitionRule::NoEndpointChange);
        assert_eq!(decision.alias.as_deref(), Some("vendor.com"));
        assert_eq!(decision.class, AliasClass::Derived);
        let silent =
            enforce_alias_update(&existing, &reordered, None, tenant(), &upstreams).unwrap();
        assert_eq!(silent, decision);
        let error =
            enforce_alias_update(&existing, &reordered, Some("other"), tenant(), &upstreams)
                .unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(error.message().contains("no endpoint change"));
    }

    /// No branch of Flow B accepts a newly supplied explicit alias, so the
    /// alias value of every allowed row is the stored one.
    #[test]
    fn no_branch_accepts_a_newly_supplied_explicit_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let derivable = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        let non_derivable = upstream(
            second_id(),
            tenant(),
            Some("payments"),
            vec![
                endpoint("http", "10.0.0.1", 80),
                endpoint("http", "10.0.0.2", 80),
            ],
        );
        // The endpoint set is unchanged, so the flow returns a no-op that keeps
        // the stored alias, never the supplied one.
        let unchanged = enforce_alias_update(
            &derivable,
            &vendor_pool(),
            Some("nope"),
            tenant(),
            &upstreams,
        )
        .unwrap_err();
        assert!(unchanged.message().contains("no endpoint change"));
        // A changed pool that still derives the same alias keeps the stored
        // alias, never the supplied one.
        let derivable_to_derivable = vec![
            endpoint("https", "apac.vendor.com", 443),
            endpoint("https", "emea.vendor.com", 443),
        ];
        let decision = enforce_alias_update(
            &derivable,
            &derivable_to_derivable,
            Some("nope"),
            tenant(),
            &upstreams,
        )
        .unwrap();
        assert_eq!(decision.alias.as_deref(), Some("vendor.com"));
        // A pool that stays non-derivable retains the stored alias.
        let still_non_derivable = vec![
            endpoint("http", "192.168.0.1", 80),
            endpoint("http", "192.168.0.2", 80),
        ];
        let retained = enforce_alias_update(
            &non_derivable,
            &still_non_derivable,
            Some("nope"),
            tenant(),
            &upstreams,
        )
        .unwrap_err();
        assert!(
            retained
                .message()
                .contains("non-derivable -> non-derivable")
        );
    }

    /// The uniqueness re-check of Flow B refuses an alias a sibling upstream of
    /// the same tenant holds, and the rejection names the table row.
    #[test]
    fn the_uniqueness_recheck_names_the_transition_rule() {
        let stores = InMemoryStores::new();
        let upstreams = stores.upstreams();
        let sibling_pool = vendor_pool();
        // A stored sibling of the same tenant holds the alias the proposed pool
        // re-derives, so the re-check speaks before the write.
        upstreams
            .insert(&upstream(
                first_id(),
                tenant(),
                Some("vendor.com"),
                sibling_pool.clone(),
            ))
            .unwrap();
        // The candidate holds the alias on a pool that derives a ported form of
        // it, so its endpoint set is a changed one and Flow B re-derives the
        // proposed pool.
        let candidate = upstream(
            second_id(),
            tenant(),
            Some("vendor.com"),
            vec![endpoint("https", "us.vendor.com", 8443)],
        );
        let proposed = vec![
            endpoint("https", "apac.vendor.com", 443),
            endpoint("https", "emea.vendor.com", 443),
        ];
        let error = enforce_alias_update(
            &candidate,
            &proposed,
            Some("vendor.com"),
            tenant(),
            &upstreams,
        )
        .unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::AlreadyExists));
        assert!(error.message().contains("derivable -> derivable"));
        assert!(error.message().contains("already holds"));
    }

    /// An upstream stored without an alias keeps having none: no branch of
    /// Flow B accepts a newly supplied explicit alias.
    #[test]
    fn an_alias_less_upstream_accepts_no_newly_supplied_alias() {
        let upstreams = InMemoryStores::new().upstreams();
        let existing = upstream(
            first_id(),
            tenant(),
            None,
            vec![
                endpoint("http", "10.0.0.1", 80),
                endpoint("http", "10.0.0.2", 80),
            ],
        );
        let proposed = vec![
            endpoint("https", "foo.co.uk", 443),
            endpoint("https", "bar.co.uk", 443),
        ];
        let retained =
            enforce_alias_update(&existing, &proposed, None, tenant(), &upstreams).unwrap();
        assert_eq!(retained.alias, None);
        assert_eq!(retained.class, AliasClass::Explicit);
        assert_eq!(retained.rule, TransitionRule::NonDerivableToNonDerivable);
        let error =
            enforce_alias_update(&existing, &proposed, Some("payments"), tenant(), &upstreams)
                .unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::EndpointRule));
        assert!(error.message().contains("non-derivable -> non-derivable"));
    }

    /// `cpt-cf-oagw-dod-alias-storage`: the resolved alias is persisted
    /// normalized and unique per tenant.
    #[test]
    fn a_resolved_alias_is_persisted_normalized_and_unique_per_tenant() {
        let stores = InMemoryStores::new();
        let upstreams = stores.upstreams();
        let pool = vec![endpoint("https", "Api.OpenAI.COM.", 443)];
        let resolved = resolve_alias_for_upstream(&pool, None, tenant(), &upstreams).unwrap();
        assert_eq!(resolved.alias, "api.openai.com");
        let mut candidate = upstream(first_id(), tenant(), None, pool.clone());
        assert_eq!(
            apply_resolved_alias(&mut candidate, &resolved),
            AliasBinding::Derived
        );
        upstreams.insert(&candidate).unwrap();
        // The normalized alias is the key the proxy path addresses, and the
        // value at rest.
        assert_eq!(
            upstreams
                .find_by_alias(tenant(), "api.openai.com")
                .unwrap()
                .alias
                .as_deref(),
            Some("api.openai.com")
        );
        // The tenant scope keeps the same alias free in another tenant.
        let elsewhere =
            resolve_alias_for_upstream(&pool, None, other_tenant(), &upstreams).unwrap();
        assert_eq!(elsewhere.alias, "api.openai.com");
        // A second upstream of the same tenant cannot take the alias again.
        let error = resolve_alias_for_upstream(&pool, None, tenant(), &upstreams).unwrap_err();
        assert_eq!(error.kind(), Some(ViolationKind::AlreadyExists));
    }

    /// `cpt-cf-oagw-dod-alias-storage`: an explicit alias is persisted as the
    /// caller supplied it, normalized.
    #[test]
    fn an_explicit_alias_is_persisted_normalized() {
        let upstreams = InMemoryStores::new().upstreams();
        let pool = vec![
            endpoint("http", "10.0.0.1", 80),
            endpoint("http", "10.0.0.2", 80),
        ];
        let resolved =
            resolve_alias_for_upstream(&pool, Some("Payments."), tenant(), &upstreams).unwrap();
        assert_eq!(resolved.alias, "payments");
        let mut candidate = upstream(first_id(), tenant(), None, pool);
        assert_eq!(
            apply_resolved_alias(&mut candidate, &resolved),
            AliasBinding::Explicit
        );
        upstreams.insert(&candidate).unwrap();
        assert_eq!(
            upstreams
                .find_by_alias(tenant(), "payments")
                .unwrap()
                .alias
                .as_deref(),
            Some("payments")
        );
    }

    /// `cpt-cf-oagw-dod-alias-storage`: a delete releases the alias, and a
    /// re-created upstream enters a new binding.
    #[test]
    fn deleting_the_upstream_releases_the_alias() {
        let stores = InMemoryStores::new();
        let upstreams = stores.upstreams();
        let pool = vendor_pool();
        let resolved = resolve_alias_for_upstream(&pool, None, tenant(), &upstreams).unwrap();
        let mut candidate = upstream(first_id(), tenant(), None, pool.clone());
        let binding = apply_resolved_alias(&mut candidate, &resolved);
        upstreams.insert(&candidate).unwrap();
        assert_eq!(binding, AliasBinding::Derived);
        assert_eq!(binding.release(), Some(AliasBinding::Released));
        upstreams.delete(tenant(), first_id()).unwrap();
        assert!(upstreams.find_by_alias(tenant(), "vendor.com").is_err());
        // The alias is free again in this tenant, entering from `Unassigned`.
        assert_eq!(
            AliasBinding::initial().bind(AliasClass::Derived),
            Some(AliasBinding::Derived)
        );
        assert_eq!(
            resolve_alias_for_upstream(&pool, None, tenant(), &upstreams)
                .unwrap()
                .alias,
            "vendor.com"
        );
    }

    /// `cpt-cf-oagw-state-alias-binding`: the closed transition set.
    #[test]
    fn the_alias_binding_refuses_every_unlisted_transition() {
        // The two accepted entries.
        assert_eq!(
            AliasBinding::initial().transition(AliasBinding::Derived),
            Some(AliasBinding::Derived)
        );
        assert_eq!(
            AliasBinding::initial().transition(AliasBinding::Explicit),
            Some(AliasBinding::Explicit)
        );
        assert_eq!(
            AliasBinding::initial().bind(AliasClass::Explicit),
            Some(AliasBinding::Explicit)
        );
        // A class change is refused in every case.
        assert_eq!(
            AliasBinding::Derived.transition(AliasBinding::Explicit),
            None
        );
        assert_eq!(
            AliasBinding::Explicit.transition(AliasBinding::Derived),
            None
        );
        assert_eq!(AliasBinding::Derived.bind(AliasClass::Explicit), None);
        // `Released` is terminal.
        assert_eq!(
            AliasBinding::Released.transition(AliasBinding::Derived),
            None
        );
        assert_eq!(
            AliasBinding::Released.release(),
            Some(AliasBinding::Released)
        );
        // An unbound alias is not released by anything.
        assert_eq!(AliasBinding::Unassigned.release(), None);
        // A re-entered state is a no-op.
        assert_eq!(
            AliasBinding::Derived.transition(AliasBinding::Derived),
            Some(AliasBinding::Derived)
        );
        assert_eq!(AliasBinding::initial().as_str(), "unassigned");
        assert_eq!(AliasBinding::Released.as_str(), "released");
        assert_eq!(AliasClass::Derived.to_string(), "derived");
        assert_eq!(AliasClass::Explicit.to_string(), "explicit");
    }

    /// The resolution entry point is a pure function over the repository
    /// traits, so the closed shell of the crate registers no route for it: the
    /// only paths carrying an alias are the proxy paths of the proxy pipeline.
    #[test]
    fn alias_resolution_registers_no_http_route() {
        for (method, path) in crate::api::rest::route_shell::shell_routes() {
            if path.contains("alias") {
                assert!(
                    path.starts_with("/proxy/{alias}"),
                    "{method} {path} must be a proxy path, not an alias-resolution route"
                );
            }
        }
    }

    /// The class of a stored alias is read from its own pool, so an alias never
    /// changes class after creation.
    #[test]
    fn the_class_of_a_stored_alias_never_changes() {
        let upstreams = InMemoryStores::new().upstreams();
        let derived = upstream(first_id(), tenant(), Some("vendor.com"), vendor_pool());
        // A pool that re-derives the stored alias keeps the class derived.
        let same =
            enforce_alias_update(&derived, &vendor_pool(), None, tenant(), &upstreams).unwrap();
        assert_eq!(same.class, AliasClass::Derived);
        // A non-derivable pool stored with an explicit alias keeps that class
        // even when the proposed pool would derive the stored alias.
        let explicit = upstream(
            second_id(),
            tenant(),
            Some("vendor.com"),
            vec![
                endpoint("http", "10.0.0.1", 80),
                endpoint("http", "10.0.0.2", 80),
            ],
        );
        let decision =
            enforce_alias_update(&explicit, &vendor_pool(), None, tenant(), &upstreams).unwrap();
        assert_eq!(decision.class, AliasClass::Explicit);
        assert_eq!(decision.rule, TransitionRule::NonDerivableToDerivable);
        assert_eq!(decision.alias.as_deref(), Some("vendor.com"));
    }
}
