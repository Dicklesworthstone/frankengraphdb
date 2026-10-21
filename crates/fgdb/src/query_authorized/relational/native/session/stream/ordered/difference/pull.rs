//! Resumable authorized result differences over the parent module's admitted endpoints.
//! The callback facade remains in the parent; this module owns pull lifecycle only.

use super::*;
use crate::NativeSpoolDifference;

/// Opening phase for one capability-scoped historical result difference.
/// Schema and exact endpoints are available before polling; polling finishes
/// both endpoint queries and the external difference sorts without consuming
/// the first changed tuple.
pub struct AuthorizedDifferenceExecution<'q, F> {
    future: Pin<Box<dyn Future<Output = Result<AuthorizedDifferenceCursor<'q, F>>> + Send + 'q>>,
    columns: Vec<String>,
    before: CommitSeq,
    after: CommitSeq,
}

/// Backpressured, capability-checked changes from one historical comparison.
/// A returned prefix is provisional until `state()` is Exhausted.
pub struct AuthorizedDifferenceCursor<'q, F> {
    inner: Option<NativeSpoolDifference<'q, F>>,
    control: Option<LiveControl<'q>>,
    guard: Option<PinGuard<'q>>,
    columns: Vec<String>,
    before: CommitSeq,
    after: CommitSeq,
    state: ScanState,
}

#[path = "pull/cursor.rs"]
mod cursor;
#[path = "pull/open.rs"]
mod open;

#[cfg(test)]
#[path = "pull/tests.rs"]
mod tests;
