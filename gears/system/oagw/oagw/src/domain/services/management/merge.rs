use crate::domain::error::DomainError;
use crate::domain::model::{Route, Upstream};

/// Check whether an upstream is visible to descendant tenants.
///
/// Per `cpt-cf-oagw-algo-tenant-alias-shadow` step 2b, an ancestor upstream is
/// visible if its own tenant matches the requester OR any per-field sharing flag
/// (`auth`, `rate_limit`, `plugins`, `cors`) is not `private`.
///
/// Returns `false` when all shareable fields are `None` — this is intentional.
/// An upstream with no auth, rate_limit, plugins, or cors has no configuration
/// to share with descendants, so it is treated as invisible. Fields without a
/// sharing mode (e.g. `headers`) do not contribute to visibility.
pub(crate) fn is_visible_to_descendant(upstream: &Upstream) -> bool {
    use crate::domain::model::SharingMode;

    let auth_shared = upstream
        .auth
        .as_ref()
        .is_some_and(|a| a.sharing != SharingMode::Private);
    let rate_shared = upstream
        .rate_limit
        .as_ref()
        .is_some_and(|r| r.sharing != SharingMode::Private);
    let plugins_shared = upstream
        .plugins
        .as_ref()
        .is_some_and(|p| p.sharing != SharingMode::Private);
    let cors_shared = upstream
        .cors
        .as_ref()
        .is_some_and(|c| c.sharing != SharingMode::Private);

    auth_shared || rate_shared || plugins_shared || cors_shared
}

/// Compute the effective upstream configuration by merging ancestor upstreams
/// in the tenant chain (root → descendant).
///
/// Per `cpt-cf-oagw-algo-tenant-config-merge`:
/// - Auth:       `private` → local-only (blocked by ancestor `enforce`); `inherit` → override; `enforce` → sticky
/// - Rate limit: `private` → local-only (constrained by ancestor `enforce` via `min()`); else `min(ancestor, descendant)`
/// - Plugins:    `private` → local-only (ancestor `enforce` items preserved); else concatenate `ancestor + descendant`
/// - Tags:       union (add-only)
///
/// `ancestor_chain` is ordered root-first: `[root, parent, ..., selected]`.
/// The last element is the selected (resolved) upstream.
pub(crate) fn compute_effective_config(
    ancestor_chain: &[Upstream],
    route: Option<&Route>,
) -> Result<Upstream, DomainError> {
    use crate::domain::model::{BudgetMode, SharingMode};

    if ancestor_chain.is_empty() {
        return Err(DomainError::Internal {
            message: "compute_effective_config called with empty ancestor_chain".into(),
        });
    }

    // Start with the root upstream as the base.
    let mut effective = ancestor_chain[0].clone();

    // Walk root → descendant, merging each layer.
    for layer in &ancestor_chain[1..] {
        // Auth merge
        merge_auth(&mut effective, layer);

        // Rate limit merge
        merge_rate_limit(&mut effective, layer);

        // Plugins merge
        merge_plugins(&mut effective, layer);

        // CORS merge
        merge_cors(&mut effective, layer)?;

        // Tags: union (add-only)
        for tag in &layer.tags {
            if !effective.tags.contains(tag) {
                effective.tags.push(tag.clone());
            }
        }

        // Server, protocol, enabled, alias: always use the selected upstream's values.
        effective.id = layer.id;
        effective.tenant_id = layer.tenant_id;
        effective.alias = layer.alias.clone();
        effective.server = layer.server.clone();
        effective.protocol = layer.protocol.clone();
        effective.enabled = layer.enabled;
        effective.headers = layer.headers.clone().or(effective.headers);
    }

    // Defense-in-depth: if the effective config has a shared budget but
    // pool_owner_id was never set (single-element chain or all layers were
    // no-ops), initialize it to the effective upstream's ID.
    if let Some(ref mut rl) = effective.rate_limit
        && rl.pool_owner_id.is_none()
        && rl
            .budget
            .as_ref()
            .is_some_and(|b| b.mode == BudgetMode::Shared)
    {
        rl.pool_owner_id = Some(effective.id);
    }

    // Route-level overrides (route > upstream base per config layering).
    if let Some(route) = route {
        // Route plugins: concatenate upstream + route plugins.
        if let Some(ref route_plugins) = route.plugins {
            match route_plugins.sharing {
                SharingMode::Private => {
                    // Private → route's plugins replace upstream's (local-only).
                    effective.plugins = Some(crate::domain::model::PluginsConfig {
                        sharing: route_plugins.sharing,
                        items: route_plugins.items.clone(),
                    });
                }
                SharingMode::Inherit | SharingMode::Enforce => {
                    // Dedup by plugin_ref; route config wins on conflict.
                    let mut merged_items = effective
                        .plugins
                        .as_ref()
                        .map(|p| p.items.clone())
                        .unwrap_or_default();
                    for item in &route_plugins.items {
                        if let Some(existing) = merged_items
                            .iter_mut()
                            .find(|m| m.plugin_ref == item.plugin_ref)
                        {
                            *existing = item.clone();
                        } else {
                            merged_items.push(item.clone());
                        }
                    }
                    effective.plugins = Some(crate::domain::model::PluginsConfig {
                        sharing: route_plugins.sharing,
                        items: merged_items,
                    });
                }
            }
        }

        // Route rate limit: min(effective, route).
        if let Some(ref route_rl) = route.rate_limit {
            match route_rl.sharing {
                SharingMode::Private => {}
                _ => {
                    effective.rate_limit =
                        Some(min_rate_limit(effective.rate_limit.as_ref(), route_rl));
                }
            }
        }

        // Route CORS: follows same sharing semantics as upstream-level merge.
        // Per inst-merge-3a5: private → skip; inherit → union origins;
        // enforce → use upstream CORS (keep effective unchanged).
        if let Some(ref route_cors) = route.cors {
            let effective_is_enforced = effective
                .cors
                .as_ref()
                .is_some_and(|c| c.sharing == SharingMode::Enforce);
            if !effective_is_enforced {
                match route_cors.sharing {
                    SharingMode::Private | SharingMode::Enforce => {
                        // Private → skip; Enforce → use upstream CORS.
                    }
                    SharingMode::Inherit => {
                        let mut merged = route_cors.clone();
                        if let Some(ref upstream_cors) = effective.cors {
                            for origin in &upstream_cors.allowed_origins {
                                if !merged.allowed_origins.contains(origin) {
                                    merged.allowed_origins.push(origin.clone());
                                }
                            }
                        }
                        crate::domain::cors::validate_cors_config(&merged)?;
                        effective.cors = Some(merged);
                    }
                }
            }
        }

        // Route tags: union.
        for tag in &route.tags {
            if !effective.tags.contains(tag) {
                effective.tags.push(tag.clone());
            }
        }
    }

    Ok(effective)
}

/// Merge auth config from a descendant layer onto the effective config.
///
/// Key invariant: once an ancestor sets `enforce`, no descendant can override
/// regardless of the descendant's own sharing mode.  This is defense-in-depth;
/// `validate_bind_constraints` also guards this at create/update time.
///
/// Sharing semantics:
/// - `Private` + ancestor enforced → keep ancestor (enforce is sticky)
/// - `Private` + ancestor not enforced → descendant replaces (local-only)
/// - `Inherit` → descendant overrides ancestor
/// - `Enforce` → descendant's enforce becomes sticky for further descendants
fn merge_auth(effective: &mut Upstream, layer: &Upstream) {
    use crate::domain::model::SharingMode;

    let effective_is_enforced = effective
        .auth
        .as_ref()
        .is_some_and(|a| a.sharing == SharingMode::Enforce);

    match &layer.auth {
        None => {} // Absent → inherit from previous level (no-op).
        Some(_) if effective_is_enforced => {
            // Ancestor enforced — no descendant can change it regardless of sharing mode.
        }
        Some(descendant_auth) => {
            // Private → local-only replace; Inherit → override; Enforce → becomes sticky.
            effective.auth = Some(descendant_auth.clone());
        }
    }
}

/// Merge rate limit config: `min(ancestor_enforced, descendant)`.
///
/// Key invariant: if the effective rate limit is already `Enforce`, a
/// descendant `Private` cannot drop it — `min()` is applied instead.
/// This is defense-in-depth; `validate_bind_constraints` also guards
/// this at create/update time.
///
/// When the effective rate limit has `budget.mode == Shared`, the
/// `pool_owner_id` is set to the effective upstream's ID so all
/// descendants share one token bucket at runtime.
pub(super) fn merge_rate_limit(effective: &mut Upstream, layer: &Upstream) {
    use crate::domain::model::{BudgetMode, SharingMode};

    // Capture shared-pool owner before merging (the ancestor that defines
    // the shared budget is the pool owner).
    let pool_owner = effective
        .rate_limit
        .as_ref()
        .and_then(|rl| rl.budget.as_ref())
        .filter(|b| b.mode == BudgetMode::Shared)
        .map(|_| effective.id);

    let effective_is_enforced = effective
        .rate_limit
        .as_ref()
        .is_some_and(|r| r.sharing == SharingMode::Enforce);

    match &layer.rate_limit {
        None => {} // Absent = inherit from previous level (no-op).
        Some(descendant_rl) => match descendant_rl.sharing {
            SharingMode::Private if effective_is_enforced => {
                // Ancestor enforced — descendant cannot escape; apply min.
                effective.rate_limit =
                    Some(min_rate_limit(effective.rate_limit.as_ref(), descendant_rl));
            }
            SharingMode::Private => {
                effective.rate_limit = Some(descendant_rl.clone());
            }
            SharingMode::Inherit | SharingMode::Enforce => {
                effective.rate_limit =
                    Some(min_rate_limit(effective.rate_limit.as_ref(), descendant_rl));
            }
        },
    }

    // Propagate shared-pool owner to descendants so the proxy uses
    // the defining ancestor's rate limit key.  Only set once: the first
    // ancestor that defines BudgetMode::Shared pins the owner; later
    // merges must not overwrite it.
    //
    // Skip for non-enforced Private: the descendant opted out of the
    // shared pool and should use its own independent bucket.
    let is_private_override = layer
        .rate_limit
        .as_ref()
        .is_some_and(|rl| rl.sharing == SharingMode::Private)
        && !effective_is_enforced;

    if !is_private_override
        && let Some(owner_id) = pool_owner
        && let Some(ref mut rl) = effective.rate_limit
        && rl.pool_owner_id.is_none()
    {
        rl.pool_owner_id = Some(owner_id);
    }
}

/// Merge CORS config from a descendant layer onto the effective config.
///
/// Per `inst-merge-3a5` (feature 0005 — tenant hierarchy):
/// - `Private`  → skip (do not modify effective).
/// - Absent     → inherit from previous level; Private must not propagate.
/// - `Inherit`  → descendant config wins, `allowed_origins` is the union
///   of ancestor + descendant origins (deduped); Private ancestor origins
///   are excluded from the union.
/// - `Enforce`  → use ancestor CORS (keep effective unchanged).
///
/// Ancestor enforce is sticky: once effective is `Enforce`, no descendant
/// can change it regardless of sharing mode.
fn merge_cors(effective: &mut Upstream, layer: &Upstream) -> Result<(), DomainError> {
    use crate::domain::model::SharingMode;

    let effective_is_enforced = effective
        .cors
        .as_ref()
        .is_some_and(|c| c.sharing == SharingMode::Enforce);

    match &layer.cors {
        None => {
            // Absent → inherit from previous level, but Private must not propagate.
            if effective
                .cors
                .as_ref()
                .is_some_and(|c| c.sharing == SharingMode::Private)
            {
                effective.cors = None;
            }
        }
        Some(_) if effective_is_enforced => {
            // Ancestor enforced — no descendant can change it.
        }
        Some(descendant_cors) => match descendant_cors.sharing {
            SharingMode::Private => {
                // Per inst-merge-3a5: private → skip (do not modify effective).
            }
            SharingMode::Enforce => {
                // Per inst-merge-3a5: enforce → use ancestor CORS (keep effective unchanged).
            }
            SharingMode::Inherit => {
                // Union allowed_origins from ancestor + descendant, skipping Private ancestor.
                let mut merged = descendant_cors.clone();
                if let Some(ref ancestor) = effective.cors
                    && ancestor.sharing != SharingMode::Private
                {
                    for origin in &ancestor.allowed_origins {
                        if !merged.allowed_origins.contains(origin) {
                            merged.allowed_origins.push(origin.clone());
                        }
                    }
                }
                crate::domain::cors::validate_cors_config(&merged)?;
                effective.cors = Some(merged);
            }
        },
    }

    Ok(())
}

/// Return the stricter of two rate limit configs (lower rate wins).
fn min_rate_limit(
    a: Option<&crate::domain::model::RateLimitConfig>,
    b: &crate::domain::model::RateLimitConfig,
) -> crate::domain::model::RateLimitConfig {
    match a {
        None => b.clone(),
        Some(a) => {
            let a_rate = rate_per_second(a);
            let b_rate = rate_per_second(b);
            if b_rate < a_rate {
                let mut winner = b.clone();
                // Preserve pool_owner_id from the existing effective config so
                // shared-pool keying is not lost when a stricter route wins.
                if winner.pool_owner_id.is_none() {
                    winner.pool_owner_id = a.pool_owner_id;
                }
                winner
            } else {
                a.clone()
            }
        }
    }
}

/// Convert a window enum to its duration in seconds.
pub(in crate::domain::services) fn window_to_secs(window: crate::domain::model::Window) -> f64 {
    use crate::domain::model::Window;
    match window {
        Window::Second => 1.0,
        Window::Minute => 60.0,
        Window::Hour => 3600.0,
        Window::Day => 86400.0,
    }
}

/// Normalize a rate limit to requests-per-second for comparison.
pub(in crate::domain::services) fn rate_per_second(
    rl: &crate::domain::model::RateLimitConfig,
) -> f64 {
    f64::from(rl.sustained.rate) / window_to_secs(rl.sustained.window)
}

/// Merge plugins config: concatenate ancestor + descendant (dedup by `plugin_ref`).
///
/// Key invariant: if the effective plugins are already `Enforce`, a
/// descendant `Private` cannot drop enforced items — they are preserved
/// and the descendant's items are appended. On `plugin_ref` conflict,
/// the enforced ancestor config wins.
///
/// For `Inherit`/`Enforce` descendants, plugins are concatenated with
/// descendant config winning on `plugin_ref` conflict (override semantics).
fn merge_plugins(effective: &mut Upstream, layer: &Upstream) {
    use crate::domain::model::SharingMode;

    let effective_is_enforced = effective
        .plugins
        .as_ref()
        .is_some_and(|p| p.sharing == SharingMode::Enforce);

    match &layer.plugins {
        None => {} // Inherit from previous level.
        Some(descendant_plugins) => match descendant_plugins.sharing {
            SharingMode::Private if effective_is_enforced => {
                // Ancestor enforced — preserve enforced items, append descendant.
                // Dedup by plugin_ref; ancestor (enforced) config wins on conflict.
                let mut merged = effective
                    .plugins
                    .as_ref()
                    .map(|p| p.items.clone())
                    .unwrap_or_default();
                for item in &descendant_plugins.items {
                    if !merged.iter().any(|m| m.plugin_ref == item.plugin_ref) {
                        merged.push(item.clone());
                    }
                }
                effective.plugins = Some(crate::domain::model::PluginsConfig {
                    sharing: SharingMode::Enforce,
                    items: merged,
                });
            }
            SharingMode::Private => {
                effective.plugins = Some(descendant_plugins.clone());
            }
            SharingMode::Inherit | SharingMode::Enforce => {
                // Concatenate: ancestor + descendant (dedup by plugin_ref).
                // Descendant config wins on conflict (override semantics).
                let mut merged = effective
                    .plugins
                    .as_ref()
                    .map(|p| p.items.clone())
                    .unwrap_or_default();
                for item in &descendant_plugins.items {
                    if let Some(existing) =
                        merged.iter_mut().find(|m| m.plugin_ref == item.plugin_ref)
                    {
                        *existing = item.clone();
                    } else {
                        merged.push(item.clone());
                    }
                }
                effective.plugins = Some(crate::domain::model::PluginsConfig {
                    sharing: descendant_plugins.sharing,
                    items: merged,
                });
            }
        },
    }
}
