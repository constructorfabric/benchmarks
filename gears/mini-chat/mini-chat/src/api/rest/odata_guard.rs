//! Complexity guard in front of the platform `OData` extractor.
//!
//! The platform `$filter` parser and compiler are recursive: a flat chain of a
//! few hundred `and`, `or` or `not` terms (a few KB) overflows the stack of the
//! serving thread (2 MiB for a tokio worker) in the filter conversion and
//! aborts the whole process. The platform budgets (8 KB, 2000 nodes) are
//! checked only after parsing, so they do not prevent it. [`GuardedOData`]
//! inspects the raw `$filter` first and rejects, with the platform `OData`
//! problem (`400 invalid_argument`, resource type
//! `gts.cf.core.odata.query.v1~`, reason `FILTER_TOO_COMPLEX`), a filter that
//! is too long, has too many logical operators or nests too deeply. Every
//! other query is handed to the platform extractor unchanged.
//!
//! Two layers, because the platform grammar lets an operator keyword touch its
//! neighbours (`trueortrue`, `1orid eq 1`, `nottrue`), so a lexical check
//! alone can be bypassed:
//!
//! 1. a cheap lexical check (length, parenthesis nesting, and/or/not counted
//!    inside every alphabetic run outside string literals);
//! 2. a structural check that cannot be bypassed lexically: the filter (at
//!    most [`MAX_GUARDED_FILTER_LEN`] bytes by then) is parsed with the
//!    platform parser, and its AST depth and node count are bounded. A filter
//!    that can nest (it has a `(` or a `not`) is parsed on a dedicated
//!    large-stack thread, at most [`MAX_CONCURRENT_STRUCTURE_CHECKS`] at a time
//!    (a caller that cannot get a slot within [`PERMIT_WAIT`] is answered
//!    `503 service_unavailable`); a flat chain is bounded by the operator count
//!    and is parsed inline. The AST is dropped where it was built, and the
//!    serving thread only ever converts an AST within the bounds. A filter the
//!    platform parser rejects is answered right here with the platform's own
//!    `INVALID_FILTER` problem instead of being parsed a second time.
//!
//! The bounds are far above what any client of the chat lists needs and far
//! below the depth that is unsafe in a debug build.

use std::time::Duration;

use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use tokio::sync::Semaphore;
use toolkit::api::canonical_prelude::OData;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::ODataQuery;
use toolkit_odata::ast::Expr;
use toolkit_odata::errors::OdataError;

/// Longest `$filter` (bytes) the guard lets through to the parser. A longer
/// filter up to the platform limit is `FILTER_TOO_COMPLEX`; beyond the
/// platform limit the platform answers `FILTER_TOO_LONG`.
pub const MAX_GUARDED_FILTER_LEN: usize = 2048;

/// Platform limit of the filter length (`toolkit::api::odata::MAX_FILTER_LEN`);
/// longer filters are left to the platform extractor.
const PLATFORM_MAX_FILTER_LEN: usize = 8 * 1024;

/// Most `and`, `or` and `not` operators in one filter.
pub const MAX_LOGICAL_OPERATORS: usize = 50;

/// Deepest parenthesis nesting (grouping and function calls).
pub const MAX_NESTING: usize = 32;

/// Deepest `$filter` AST (a leaf is depth 1).
pub const MAX_AST_DEPTH: usize = 32;

/// Most nodes in the `$filter` AST.
pub const MAX_AST_NODES: usize = 200;

/// Stack of the thread that parses and measures a filter. The lexical bounds
/// (at most [`MAX_NESTING`] parentheses and [`MAX_LOGICAL_OPERATORS`]
/// operators in at most [`MAX_GUARDED_FILTER_LEN`] bytes) cap the parser's
/// recursion far below what this stack holds, even in a debug build.
const STRUCTURE_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Most structure checks (and so large-stack threads) running at once.
pub const MAX_CONCURRENT_STRUCTURE_CHECKS: usize = 4;

/// How long a request waits for a structure-check slot before it is answered
/// `503 service_unavailable`. A check of a bounded filter takes milliseconds.
pub const PERMIT_WAIT: Duration = Duration::from_millis(250);

/// `Retry-After` of the `503` answer, in seconds.
const RETRY_AFTER_SECS: u64 = 1;

/// The slots of the structure checks, shared by every request.
static STRUCTURE_PERMITS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_STRUCTURE_CHECKS);

/// Keywords the platform grammar treats as logical operators.
const LOGICAL_KEYWORDS: [&str; 3] = ["and", "or", "not"];

/// The query name the problem is reported for.
const FILTER_PARAM: &str = "$filter";

/// Why a filter was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overrun {
    Length,
    Operators,
    Nesting,
    Structure,
}

impl Overrun {
    fn detail(self) -> &'static str {
        match self {
            Self::Length => "Filter too complex: longer than 2048 bytes",
            Self::Operators => "Filter too complex: more than 50 logical operators",
            Self::Nesting => "Filter too complex: nested more than 32 levels",
            Self::Structure => {
                "Filter too complex: expression deeper than 32 levels or larger than 200 nodes"
            }
        }
    }
}

/// Checks one raw (percent-decoded) `$filter` value against the bounds.
fn check_filter(raw: &str) -> Result<(), Overrun> {
    let raw = raw.trim();
    if raw.len() > PLATFORM_MAX_FILTER_LEN {
        // The platform extractor rejects it as `FILTER_TOO_LONG`.
        return Ok(());
    }
    if raw.len() > MAX_GUARDED_FILTER_LEN {
        return Err(Overrun::Length);
    }

    let mut in_string = false;
    let mut depth = 0_usize;
    let mut operators = 0_usize;
    let mut word = String::new();
    // Text inside single quotes (a doubled quote is an escaped quote and
    // toggles twice) is data, not syntax.
    for ch in raw.chars().chain(std::iter::once(' ')) {
        if in_string {
            if ch == '\'' {
                in_string = false;
            }
            continue;
        }
        if ch.is_ascii_alphabetic() {
            word.push(ch);
            continue;
        }
        // The grammar allows an operator glued to its neighbours, so every
        // occurrence inside the run counts, not only a run equal to a keyword.
        operators += count_logical_keywords(&word);
        if operators > MAX_LOGICAL_OPERATORS {
            return Err(Overrun::Operators);
        }
        word.clear();
        match ch {
            '\'' => in_string = true,
            '(' => {
                depth += 1;
                if depth > MAX_NESTING {
                    return Err(Overrun::Nesting);
                }
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Whether the platform extractor will parse `raw`: not empty and within the
/// platform length limit (a longer one is rejected as `FILTER_TOO_LONG` before
/// parsing, and this guard never parses it).
fn needs_structure_check(raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty() && raw.len() <= PLATFORM_MAX_FILTER_LEN
}

/// Counts the non-overlapping, case-insensitive occurrences of `and`, `or`
/// and `not` in one alphabetic run, scanning left to right. The field and
/// function vocabulary of both lists (`updated_at`, `created_at`, `id`,
/// `title`, `role`, `contains`, `startswith`, `endswith`, ...) contains none
/// of them, so a legitimate filter counts only its real operators.
fn count_logical_keywords(word: &str) -> usize {
    let bytes = word.as_bytes();
    let mut count = 0;
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        match LOGICAL_KEYWORDS.iter().find(|keyword| {
            rest.len() >= keyword.len()
                && rest[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
        }) {
            Some(keyword) => {
                count += 1;
                at += keyword.len();
            }
            None => at += 1,
        }
    }
    count
}

/// Depth of an AST (a leaf is 1).
fn ast_depth(expr: &Expr) -> usize {
    1 + match expr {
        Expr::Identifier(_) | Expr::Value(_) => 0,
        Expr::Not(inner) => ast_depth(inner),
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Compare(left, _, right) => {
            ast_depth(left).max(ast_depth(right))
        }
        Expr::In(left, list) => list.iter().map(ast_depth).fold(ast_depth(left), usize::max),
        Expr::Function(_, args) => args.iter().map(ast_depth).max().unwrap_or(0),
    }
}

/// Why the structure check did not pass.
#[derive(Debug)]
enum Rejected {
    /// The filter exceeds a bound: `400 FILTER_TOO_COMPLEX`.
    TooComplex(Overrun),
    /// The platform parser rejects the filter: the platform's own
    /// `400 INVALID_FILTER`.
    Unparsable(toolkit_odata::Error),
    /// No slot or thread for the check: `503 service_unavailable`.
    Unavailable,
}

impl Rejected {
    fn into_problem(self) -> CanonicalError {
        match self {
            Self::TooComplex(overrun) => OdataError::invalid_argument()
                .with_field_violation(FILTER_PARAM, overrun.detail(), "FILTER_TOO_COMPLEX")
                .create(),
            // The same conversion the platform extractor applies to a parse
            // failure, so the answer is identical to the one it would give.
            Self::Unparsable(error) => CanonicalError::from(error),
            Self::Unavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(RETRY_AFTER_SECS)
                .create(),
        }
    }
}

/// Whether a filter can nest: only a parenthesis or a `not` (also glued to its
/// neighbours, in any case) recurses in the grammar. Everything else is a flat
/// chain whose parse is iterative and whose length the operator bound caps.
fn can_nest(raw: &str) -> bool {
    raw.contains('(') || raw.to_ascii_lowercase().contains("not")
}

/// Parses `raw` with the platform parser and bounds the AST depth and node
/// count. A filter the parser rejects is rejected here with the platform's
/// `INVALID_FILTER`. A filter that can nest must run on a large stack
/// ([`STRUCTURE_STACK_SIZE`]); the AST is dropped by the caller's thread.
fn check_structure(raw: &str) -> Result<(), Rejected> {
    let parsed = toolkit_odata::parse_filter_string(raw.trim()).map_err(Rejected::Unparsable)?;
    if parsed.node_count() > MAX_AST_NODES || ast_depth(parsed.as_expr()) > MAX_AST_DEPTH {
        return Err(Rejected::TooComplex(Overrun::Structure));
    }
    Ok(())
}

/// Runs [`check_structure`] so that no filter can overflow a serving thread
/// however it is written: a filter that cannot nest is parsed inline; any other
/// runs on a dedicated large-stack thread, holding one of `permits` until the
/// thread ends. Fails closed with [`Rejected::Unavailable`]: no slot within
/// `wait`, a thread that cannot be started, or a thread that dies.
async fn check_structure_with(
    raw: &str,
    permits: &'static Semaphore,
    wait: Duration,
) -> Result<(), Rejected> {
    if !can_nest(raw) {
        return check_structure(raw);
    }
    let Ok(Ok(permit)) = tokio::time::timeout(wait, permits.acquire()).await else {
        return Err(Rejected::Unavailable);
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let raw = raw.to_owned();
    let spawned = std::thread::Builder::new()
        .name("mini-chat-odata-guard".to_owned())
        .stack_size(STRUCTURE_STACK_SIZE)
        .spawn(move || {
            let outcome = check_structure(&raw);
            // The slot frees when the work is done, even if the request was
            // cancelled meanwhile, so the bound is on running threads.
            drop(permit);
            // The receiver is gone only when the request was cancelled.
            sender.send(outcome).ok();
        });
    if spawned.is_err() {
        return Err(Rejected::Unavailable);
    }
    receiver.await.unwrap_or(Err(Rejected::Unavailable))
}

/// [`check_structure_with`] on the shared slots.
async fn check_structure_on_big_stack(raw: &str) -> Result<(), Rejected> {
    check_structure_with(raw, &STRUCTURE_PERMITS, PERMIT_WAIT).await
}

/// The platform [`OData`] extractor behind the complexity guard.
#[derive(Debug, Clone)]
pub struct GuardedOData(pub ODataQuery);

impl<S> FromRequestParts<S> for GuardedOData
where
    S: Send + Sync,
{
    type Rejection = CanonicalError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // A query string that does not deserialize is left to the platform
        // extractor, which reports it.
        if let Ok(Query(pairs)) =
            Query::<Vec<(String, String)>>::from_request_parts(parts, state).await
        {
            for (_, value) in pairs.iter().filter(|(key, _)| key == FILTER_PARAM) {
                let checked = match check_filter(value) {
                    Ok(()) if needs_structure_check(value) => {
                        check_structure_on_big_stack(value).await
                    }
                    Ok(()) => Ok(()),
                    Err(overrun) => Err(Rejected::TooComplex(overrun)),
                };
                if let Err(rejected) = checked {
                    return Err(rejected.into_problem());
                }
            }
        }
        let OData(query) = OData::from_request_parts(parts, state).await?;
        Ok(Self(query))
    }
}

#[cfg(test)]
#[path = "odata_guard_tests.rs"]
mod tests;
