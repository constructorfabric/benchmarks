//! GTS type-catalogue provisioning routine.
//!
//! Realizes `cpt-cf-oagw-algo-type-catalog-provisioning` and
//! `cpt-cf-oagw-flow-type-provisioning`: one batch register, no retry, per
//! entry success or a typed failure, and an ERROR log per failing identifier
//! that carries the existing entry as evidence. Provisioning never overwrites
//! or deletes an entry it does not own.

use tracing::{error, info};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

use super::catalog::{self, CatalogError};

/// Outcome of a fully provisioned catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CatalogProvisioned {
    /// Number of entries the batch carried.
    pub total: usize,
    /// Number of entries accepted, including entries already registered with
    /// byte-identical content.
    pub succeeded: usize,
}

/// One entry the registry refused.
#[derive(Debug, Clone)]
pub struct EntryFailure {
    /// GTS identifier of the refused entry, `<unknown>` when the registry
    /// could not extract one.
    pub gts_id: String,
    /// The typed error the registry returned.
    pub error: String,
    /// Content summary of the entry already registered, when one could be
    /// read back. Never overwritten or deleted.
    pub existing: Option<String>,
}

/// Why provisioning failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvisioningError {
    /// The catalogue could not be assembled from its frozen inputs.
    #[error("the OAGW type catalogue could not be assembled: {0}")]
    Catalog(CatalogError),
    /// A catastrophic SDK failure: unavailable backend or call timeout. Not
    /// retried and not partially re-issued.
    #[error("types-registry batch register failed: {0}")]
    Registry(String),
    /// At least one per-entry registration failed. The gear never reports
    /// readiness.
    #[error("{failed} of {total} OAGW catalogue entries failed to register: {identifiers}")]
    EntriesFailed {
        /// Number of refused entries.
        failed: usize,
        /// Number of entries the batch carried.
        total: usize,
        /// Comma-separated failing GTS identifiers, for the failure message.
        identifiers: String,
    },
}

/// The per-entry failures behind a [`ProvisioningError::EntriesFailed`].
///
/// Kept out of the error value itself so the error stays `Clone + PartialEq`
/// while the failures carry their full typed detail.
#[derive(Debug, Clone, Default)]
pub struct EntryFailures {
    /// Every refused entry, with its identifier, typed error, and the
    /// existing entry read back from the registry.
    pub entries: Vec<EntryFailure>,
}

impl ProvisioningError {
    /// The failing GTS identifiers, in submission order.
    #[must_use]
    pub fn failing_identifiers<'a>(&self, failures: &'a EntryFailures) -> Vec<&'a str> {
        match self {
            Self::Catalog(_) | Self::Registry(_) => Vec::new(),
            Self::EntriesFailed { .. } => {
                failures.entries.iter().map(|f| f.gts_id.as_str()).collect()
            }
        }
    }
}

/// Provisions the OAGW type catalogue through the types-registry.
///
/// The batch is built parents-first; the registry additionally sorts it
/// lexicographically by GTS identifier, which guarantees a base type
/// (suffix `~`) precedes its instances. A catastrophic SDK failure fails the
/// phase immediately: no retry, and no continuation with an unprovisioned
/// catalogue. Entries registered before a failure stay in the registry — that
/// is the rollback story, since a re-run over identical content succeeds
/// instead of conflicting.
///
/// On failure the caller receives the failing identifiers through
/// [`ProvisioningError::failing_identifiers`], and each one has already been
/// logged at ERROR.
///
/// # Errors
///
/// Returns [`ProvisioningError::Catalog`] when the catalogue cannot be
/// assembled, [`ProvisioningError::Registry`] for a catastrophic SDK failure,
/// and [`ProvisioningError::EntriesFailed`] when any entry is refused.
pub async fn provision(
    client: &dyn TypesRegistryClient,
) -> Result<CatalogProvisioned, ProvisioningError> {
    let batch = catalog::catalog_entities().map_err(ProvisioningError::Catalog)?;
    let total = batch.len();

    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-try
    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-register
    let submitted = client.register(batch).await;
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-register
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-try

    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-catch
    // CATCH a catastrophic SDK failure such as an unavailable backend: the
    // post-init phase fails and the catalogue is never retried here.
    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-catch-handle
    let results = submitted.map_err(|e| ProvisioningError::Registry(e.to_string()))?;
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-catch-handle
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-catch

    let (failures, provisioned) = settle(client, results, total).await;
    report(&failures, total);

    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-check
    if failures.entries.is_empty() {
        // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-ready
        report_success(&provisioned);
        // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-return
        return Ok(provisioned);
        // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-return
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-ready
    }
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-check

    Err(ProvisioningError::EntriesFailed {
        failed: failures.entries.len(),
        total,
        identifiers: failing_identifiers(&failures),
    })
}

/// Classifies the per-entry results and reads back the existing entry for
/// every refusal.
///
/// # Panics
///
/// Never panics.
async fn settle(
    client: &dyn TypesRegistryClient,
    results: Vec<RegisterResult>,
    total: usize,
) -> (EntryFailures, CatalogProvisioned) {
    let mut provisioned = CatalogProvisioned {
        total,
        succeeded: 0,
    };
    let mut failures = EntryFailures::default();

    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-loop
    for result in results {
        match result {
            // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-idempotent
            // An entry that succeeded — or that was already registered with
            // byte-identical content — counts as success.
            RegisterResult::Ok { .. } => provisioned.succeeded += 1,
            // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-idempotent
            // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-collect-failure
            RegisterResult::Err { gts_id, error } => {
                let identifier = gts_id.unwrap_or_else(|| String::from("<unknown>"));
                let existing = read_back(client, &identifier).await;
                failures.entries.push(EntryFailure {
                    gts_id: identifier,
                    error: error.to_string(),
                    existing,
                });
            }
            // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-collect-failure
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-loop

    (failures, provisioned)
}

/// Logs every failing identifier at ERROR with the typed error and the
/// existing entry as evidence; never skips an entry with a warning.
fn report(failures: &EntryFailures, total: usize) {
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-fail
    // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-fail-if
    if !failures.entries.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-fail
        for failure in &failures.entries {
            error!(
                gts_id = %failure.gts_id,
                error = %failure.error,
                existing = failure.existing.as_deref().unwrap_or("<no existing entry>"),
                "Failed to register an OAGW GTS catalogue entry"
            );
        }
        error!(
            failed = failures.entries.len(),
            total,
            identifiers = %failing_identifiers(failures),
            "OAGW type catalogue provisioning failed; readiness stays withheld"
        );
        // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-fail
    }
    // @cpt-end:cpt-cf-oagw-algo-type-catalog-provisioning:p1:inst-catalog-fail-if
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning:p1:inst-type-prov-fail
}

/// Comma-separated failing GTS identifiers, in submission order.
fn failing_identifiers(failures: &EntryFailures) -> String {
    failures
        .entries
        .iter()
        .map(|f| f.gts_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reads back the existing registry entry for a refused identifier, as the
/// evidence an operator needs to decide which remediation applies: a stale
/// entry is the thing to remove, a wrong identifier is the thing to correct.
/// Never overwrites or deletes.
async fn read_back(client: &dyn TypesRegistryClient, gts_id: &str) -> Option<String> {
    let content = if gts_id.ends_with('~') {
        client
            .get_type_schema(gts_id)
            .await
            .ok()
            .map(|schema| schema.raw_schema)
    } else {
        client
            .get_instance(gts_id)
            .await
            .ok()
            .map(|instance| instance.object)
    }?;

    Some(content_summary(&content))
}

/// Short, single-line summary of an existing registry entry.
fn content_summary(content: &serde_json::Value) -> String {
    match content.as_object() {
        None => String::from("<non-object content>"),
        Some(fields) => {
            let keys: Vec<&String> = fields.keys().collect();
            format!(
                "{} keys: {}",
                keys.len(),
                serde_json::to_string(&keys).unwrap_or_default()
            )
        }
    }
}

/// On success, logs the provisioned catalogue so the startup trail shows the
/// phase completed.
fn report_success(provisioned: &CatalogProvisioned) {
    info!(
        total = provisioned.total,
        succeeded = provisioned.succeeded,
        "OAGW type catalogue provisioned"
    );
}
