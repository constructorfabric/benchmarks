//! Plugin chain composition
//! (`cpt-cf-oagw-algo-plugin-system-chain-order`, DoD
//! `cpt-cf-oagw-dod-plugin-system-chain-ordering`).
//!
//! The composition is a pure concatenation with two sharing rules and one
//! authority rule:
//!
//! * upstream-level bindings come before route-level bindings, in binding
//!   order within each level, so `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`;
//! * an ancestor binding inherited under `sharing: private` contributes
//!   nothing to a descendant's chain, while one inherited under
//!   `sharing: enforce` is retained so no descendant composition can remove
//!   it;
//! * the single auth plugin is taken from the upstream `auth` block, never
//!   from the chain, and there is at most one per request.
// @cpt-algo:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1

use crate::domain::dto::{AuthConfig, SharingMode};
use crate::domain::error::DomainError;
use crate::domain::repo::PluginBinding;

// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-1
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-7
/// One level of the composition input.
///
/// `inherited` is `Some` only for a level the requesting tenant does not own —
/// an ancestor contribution — and carries the sharing mode that level declared
/// for its plugin field. The caller's own upstream and route levels are *not*
/// inherited, so `private` on them is the caller's own choice and is honoured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChainLayer {
    pub bindings: Vec<PluginBinding>,
    pub inherited: Option<SharingMode>,
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-credential-isolation:p1:inst-ps-iso-1
//

impl ChainLayer {
    /// A level the requesting tenant owns.
    #[must_use]
    pub fn owned(bindings: Vec<PluginBinding>) -> Self {
        Self { bindings, inherited: None }
    }

    /// An ancestor-contributed level.
    #[must_use]
    pub fn inherited(bindings: Vec<PluginBinding>, sharing: SharingMode) -> Self {
        Self { bindings, inherited: Some(sharing) }
    }
}

/// The composed chain: one ordered binding list plus the single auth
/// reference taken from the upstream `auth` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposedChain {
    /// The ordered bindings, positions contiguous from zero.
    pub bindings: Vec<PluginBinding>,
    /// The auth plugin reference from the upstream `auth` block, `None` when
    /// the upstream declares no auth plugin.
    pub auth_ref: Option<String>,
}

impl ComposedChain {
    /// Whether the chain carries no plugin at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty() && self.auth_ref.is_none()
    }
}

/// Compose the effective chain for one proxied request.
///
/// `ancestors` must be ordered base -> most specific, mirroring the merge
/// engine's layer order, so the ancestor-then-descendant order of
/// `inst-ps-ord-1` falls out of the input order.
///
/// # Errors
///
/// Returns a validation error when a level's binding positions are not
/// contiguous from zero, which is the binding invariant the storage step
/// established and which composition preserves.
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-2
// `inst-ps-ord-2`: the levels concatenate upstream-then-route, in binding
// order within each level.
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-1
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-7
pub fn compose(
    ancestors: &[ChainLayer],
    upstream: &[PluginBinding],
    route: &[PluginBinding],
    auth: Option<&AuthConfig>,
) -> Result<ComposedChain, DomainError> {
    let mut composed: Vec<PluginBinding> = Vec::new();
    for level in ancestors {
        // `inst-ps-ord-1`: a `private` ancestor binding is invisible to a
        // descendant requester; `inst-ps-ord-3`: an `enforce` one is retained.
        let visible = !matches!(level.inherited, Some(SharingMode::Private));
        if visible {
            composed.extend(level.bindings.iter().cloned());
        }
    }
    composed.extend(upstream.iter().cloned());
    composed.extend(route.iter().cloned());

    // `inst-ps-ord-4`: the *resulting* positions are contiguous from zero, so
    // a consumer can address a chain entry by its position. The upstream and
    // the route level each number their own bindings from zero, so the
    // composition renumbers the concatenation rather than requiring the raw
    // concatenation to have been globally numbered; the per-level contiguity
    // is still validated per level, because a level that arrives with a hole
    // cannot be renumbered into a well-formed chain.
    validate_contiguous(upstream, "upstream")?;
    validate_contiguous(route, "route")?;
    for (index, binding) in composed.iter_mut().enumerate() {
        binding.position = index as u32;
    }

    // `inst-ps-ord-5`: at most one auth plugin, taken from the upstream `auth`
    // block and never from the chain.
    let auth_ref = auth.and_then(|auth| auth.auth_type.clone());
    Ok(ComposedChain { bindings: composed, auth_ref })
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-1
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-chain-order:p1:inst-ps-ord-2

fn validate_contiguous(level: &[PluginBinding], name: &str) -> Result<(), DomainError> {
    for (index, binding) in level.iter().enumerate() {
        if binding.position != index as u32 {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "field `plugins.items` rejected: {name} chain position {} is not contiguous from zero",
                    binding.position
                ),
                path: Some("plugins.items".to_owned()),
                trace_id: None,
            });
        }
    }
    Ok(())
}
