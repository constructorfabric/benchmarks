//! Plugin reference counting and GC mark-phase bookkeeping.
//!
//! Implements `cpt-cf-oagw-algo-plugin-ref-count` and
//! `cpt-cf-oagw-algo-plugin-gc-mark-sweep` from
//! `docs/features/plugin-management.md`. The sweep phase (a periodic job
//! deleting rows once `gc_eligible_at` has elapsed) is explicitly
//! out-of-scope for this decomposition round; this module's contract ends
//! at persisting an accurate `gc_eligible_at`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use uuid::Uuid;

use crate::model::route::Route;
use crate::model::upstream::Upstream;

const GC_TTL_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 86_400;

/// `{upstreams, routes}` GTS-identifier lists referencing a plugin, the
/// documented output shape of `cpt-cf-oagw-algo-plugin-ref-count`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginReferences {
    pub upstreams: Vec<String>,
    pub routes: Vec<String>,
}

impl PluginReferences {
    /// `count = len(referenced_by.upstreams) + len(referenced_by.routes)`
    /// (`inst-refcount-return`).
    #[must_use]
    pub fn count(&self) -> usize {
        self.upstreams.len() + self.routes.len()
    }
}

/// Whether a stored `plugins.items[]` (or scalar `auth_plugin_ref`) entry
/// binds the target plugin -- matching either the exact GTS `plugin_ref`
/// string, or (for a UUID-backed plugin) a bare-UUID form of the same
/// binding.
fn plugin_ref_matches(item: &str, plugin_ref: &str, plugin_uuid: Option<Uuid>) -> bool {
    if item == plugin_ref {
        return true;
    }
    plugin_uuid.is_some_and(|expected| Uuid::parse_str(item).is_ok_and(|actual| actual == expected))
}

/// Scan the shared `upstreams`/`routes` config-store maps for bindings to
/// `plugin_ref`/`plugin_uuid`, scoped to `tenant_id`
/// (`cpt-cf-oagw-algo-plugin-ref-count`).
// @cpt-algo:cpt-cf-oagw-algo-plugin-ref-count:p2
pub fn count_plugin_references(
    plugin_ref: &str,
    plugin_uuid: Option<Uuid>,
    tenant_id: Uuid,
    upstreams: &DashMap<Uuid, Arc<Upstream>>,
    routes: &DashMap<Uuid, Arc<Route>>,
) -> PluginReferences {
    let mut references = PluginReferences::default();

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-auth-scalar
    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-upstream-bindings
    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-union
    for entry in upstreams {
        let upstream_id = *entry.key();
        let upstream = entry.value();
        if upstream.tenant_id != tenant_id {
            continue;
        }

        let scalar_hit = upstream
            .auth
            .as_ref()
            .and_then(|auth| auth.auth_type.as_deref())
            .is_some_and(|auth_type| plugin_ref_matches(auth_type, plugin_ref, plugin_uuid));

        let binding_hit = upstream.plugins.as_ref().is_some_and(|bindings| {
            bindings
                .items
                .iter()
                .any(|item| plugin_ref_matches(item, plugin_ref, plugin_uuid))
        });

        if scalar_hit || binding_hit {
            references
                .upstreams
                .push(format!("gts.cf.core.oagw.upstream.v1~{upstream_id}"));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-union
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-upstream-bindings
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-auth-scalar

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-route-bindings
    for entry in routes {
        let route_id = *entry.key();
        let route = entry.value();
        if route.tenant_id != tenant_id {
            continue;
        }

        let binding_hit = route.plugins.as_ref().is_some_and(|bindings| {
            bindings
                .items
                .iter()
                .any(|item| plugin_ref_matches(item, plugin_ref, plugin_uuid))
        });

        if binding_hit {
            references
                .routes
                .push(format!("gts.cf.core.oagw.route.v1~{route_id}"));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-scan-route-bindings

    // @cpt-begin:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-return
    references
    // @cpt-end:cpt-cf-oagw-algo-plugin-ref-count:p2:inst-refcount-return
}

/// Current Unix-epoch seconds, saturating to `i64::MAX` rather than
/// panicking if the clock is ever before `UNIX_EPOCH`.
fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Howard Hinnant's `civil_from_days`: days-since-`1970-01-01` to a
/// proleptic-Gregorian `(year, month, day)` triple. Hand-rolled rather than
/// pulling in a date/time crate dependency, since this feature does not add
/// new `Cargo.toml` dependencies.
#[allow(clippy::integer_division)]
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days_since_epoch = days_since_epoch + 719_468;
    let era = days_since_epoch.div_euclid(146_097);
    let day_of_era = days_since_epoch - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_index = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = day_of_year - (153 * month_index + 2) / 5 + 1; // [1, 31]
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or_default(),
        u32::try_from(day).unwrap_or_default(),
    )
}

/// Format a Unix-epoch-seconds instant as an RFC 3339 UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`).
#[allow(clippy::integer_division)]
fn format_rfc3339_utc(unix_seconds: i64) -> String {
    let days_since_epoch = unix_seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = unix_seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days_since_epoch);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `now() + gc_ttl_days` formatted as RFC 3339 UTC.
fn rfc3339_now_plus_days(days: i64) -> String {
    format_rfc3339_utc(unix_seconds_now() + days * SECONDS_PER_DAY)
}

/// Mark phase of `cpt-cf-oagw-algo-plugin-gc-mark-sweep`: compute the
/// effective `gc_eligible_at` for a stored, UUID-backed plugin given its
/// current reference count. Never deletes a row -- mark phase only.
///
/// Named plugins (`inst-gc-if-named`/`inst-gc-return-named`) are exempt by
/// construction: they have no `oagw_plugin` row, so no caller ever holds a
/// [`crate::model::plugin::Plugin`] value for one to pass here.
// @cpt-algo:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-gc-bookkeeping:p1
// @cpt-state:cpt-cf-oagw-state-plugin-lifecycle:p2
// `inst-gc-if-named`/`inst-gc-return-named` (named-plugin exemption) and
// `inst-gc-return` (the final return) are satisfied structurally by every
// path through this function's body: no caller ever holds a `Plugin` value
// for a named plugin to pass here, and every branch below returns the
// (possibly unchanged) `gc_eligible_at` -- all three markers wrap the whole
// function.
// @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-named
// @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-return-named
// @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-return
#[must_use]
pub fn mark_gc_eligibility(
    current_gc_eligible_at: Option<&str>,
    reference_count: usize,
) -> Option<String> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-else-mark
    if reference_count == 0 {
        match current_gc_eligible_at {
            // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-newly-unlinked
            // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-set-eligible
            // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-referenced-to-unreferenced
            None => Some(rfc3339_now_plus_days(GC_TTL_DAYS)),
            // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-referenced-to-unreferenced
            // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-set-eligible
            // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-newly-unlinked
            // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-else-noop
            // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-noop
            Some(existing) => Some(existing.to_owned()),
            // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-noop
            // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-else-noop
        }
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-relinked
        // @cpt-begin:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-clear-eligible
        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-unreferenced-to-referenced
        None
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-unreferenced-to-referenced
        // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-clear-eligible
        // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-relinked
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-else-mark
}
// @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-return
// @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-return-named
// @cpt-end:cpt-cf-oagw-algo-plugin-gc-mark-sweep:p2:inst-gc-if-named

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::plugin::{PluginType, plugin_gts_ref};
    use crate::model::route::{Route, RouteMatch, RoutePluginsBinding};
    use crate::model::upstream::{AuthConfig, PluginsBinding, ServerConfig, Upstream};

    fn empty_upstream(
        tenant_id: Uuid,
        plugin_ref_items: Vec<String>,
        scalar_auth_type: Option<String>,
    ) -> Upstream {
        Upstream {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig { endpoints: vec![] },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: scalar_auth_type.map(|auth_type| AuthConfig {
                auth_type: Some(auth_type),
                ..Default::default()
            }),
            headers: None,
            plugins: Some(PluginsBinding {
                sharing: crate::model::upstream::Sharing::default(),
                items: plugin_ref_items,
            }),
            rate_limit: None,
            cors: None,
            tenant_id,
        }
    }

    fn route_with_plugins(
        tenant_id: Uuid,
        upstream_id: Uuid,
        plugin_ref_items: Vec<String>,
    ) -> Route {
        Route {
            id: None,
            tenant_id,
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch::default(),
            plugins: Some(RoutePluginsBinding {
                sharing: crate::model::upstream::Sharing::default(),
                items: plugin_ref_items,
            }),
            rate_limit: None,
            enabled: true,
            priority: None,
        }
    }

    #[test]
    fn counts_zero_references_for_an_unbound_plugin() {
        let tenant_id = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin_ref = plugin_gts_ref(PluginType::Guard, uuid);
        let upstreams: DashMap<Uuid, Arc<Upstream>> = DashMap::new();
        let routes: DashMap<Uuid, Arc<Route>> = DashMap::new();

        let references =
            count_plugin_references(&plugin_ref, Some(uuid), tenant_id, &upstreams, &routes);
        assert_eq!(references.count(), 0);
        assert!(references.upstreams.is_empty());
        assert!(references.routes.is_empty());
    }

    #[test]
    fn counts_a_binding_via_plugins_items_and_via_the_scalar_auth_column() {
        let tenant_id = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin_ref = plugin_gts_ref(PluginType::Auth, uuid);

        let upstream_id_scalar = Uuid::new_v4();
        let upstream_id_binding = Uuid::new_v4();
        let upstreams: DashMap<Uuid, Arc<Upstream>> = DashMap::new();
        upstreams.insert(
            upstream_id_scalar,
            Arc::new(empty_upstream(tenant_id, vec![], Some(plugin_ref.clone()))),
        );
        upstreams.insert(
            upstream_id_binding,
            Arc::new(empty_upstream(tenant_id, vec![uuid.to_string()], None)),
        );

        let route_id = Uuid::new_v4();
        let routes: DashMap<Uuid, Arc<Route>> = DashMap::new();
        routes.insert(
            route_id,
            Arc::new(route_with_plugins(
                tenant_id,
                upstream_id_binding,
                vec![plugin_ref.clone()],
            )),
        );

        let references =
            count_plugin_references(&plugin_ref, Some(uuid), tenant_id, &upstreams, &routes);
        assert_eq!(references.count(), 3);
        assert_eq!(references.upstreams.len(), 2);
        assert_eq!(references.routes.len(), 1);
        assert!(references.upstreams.contains(&format!(
            "gts.cf.core.oagw.upstream.v1~{upstream_id_scalar}"
        )));
        assert!(
            references
                .routes
                .contains(&format!("gts.cf.core.oagw.route.v1~{route_id}"))
        );
    }

    #[test]
    fn does_not_count_another_tenants_binding() {
        let owner_tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        let uuid = Uuid::new_v4();
        let plugin_ref = plugin_gts_ref(PluginType::Guard, uuid);

        let upstream_id = Uuid::new_v4();
        let upstreams: DashMap<Uuid, Arc<Upstream>> = DashMap::new();
        upstreams.insert(
            upstream_id,
            Arc::new(empty_upstream(other_tenant, vec![uuid.to_string()], None)),
        );
        let routes: DashMap<Uuid, Arc<Route>> = DashMap::new();

        let references =
            count_plugin_references(&plugin_ref, Some(uuid), owner_tenant, &upstreams, &routes);
        assert_eq!(references.count(), 0);
    }

    #[test]
    fn mark_sets_gc_eligible_at_when_newly_unreferenced() {
        let result = mark_gc_eligibility(None, 0);
        assert!(result.is_some());
    }

    #[test]
    fn mark_leaves_an_already_set_gc_eligible_at_unchanged_while_still_unreferenced() {
        let result = mark_gc_eligibility(Some("2026-01-01T00:00:00Z"), 0);
        assert_eq!(result.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn mark_clears_gc_eligible_at_once_referenced() {
        let result = mark_gc_eligibility(Some("2026-01-01T00:00:00Z"), 1);
        assert_eq!(result, None);
    }

    #[test]
    fn mark_stays_clear_while_already_referenced() {
        let result = mark_gc_eligibility(None, 3);
        assert_eq!(result, None);
    }

    #[test]
    fn format_rfc3339_utc_matches_the_known_epoch_instant() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_utc_matches_a_known_later_instant() {
        // 2024-01-01T00:00:00Z (a well-known reference instant).
        assert_eq!(format_rfc3339_utc(1_704_067_200), "2024-01-01T00:00:00Z");
    }
}
