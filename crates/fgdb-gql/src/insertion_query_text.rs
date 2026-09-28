//! Parse-once CREATE/INSERT queries over native per-occurrence result bindings.
//!
//! The insertion compiler owns creation, the row expression compiler owns
//! RETURN, and the ordinary transaction path owns publication. No source text
//! is rewritten and returned identities never come from a later graph scan.

use crate::algebra::GraphValueOrder;
use crate::set_text::{ReadPageNumber, ReadProjectionTemplate};
use crate::{GraphInsertBinding, GraphSetQuantifier, PreparedGraphInsertText};

#[derive(Clone)]
pub(crate) struct InsertReturnTemplate {
    pub bindings: Vec<GraphInsertBinding>,
    pub projection: Vec<ReadProjectionTemplate>,
    pub quantifier: GraphSetQuantifier,
    pub order: Vec<GraphValueOrder>,
    pub offset: ReadPageNumber,
    pub count: Option<ReadPageNumber>,
    pub at: usize,
}

/// One standalone, MATCH-selected or leading-UNWIND CREATE/INSERT with RETURN.
/// Each source occurrence retains its own created vertex/edge identities and
/// frozen properties. Matched bindings and source-only properties share the
/// same input row as creation; they are not recovered by a later graph scan.
/// RETURN accepts native scalar/CASE/list expressions, DISTINCT, output-column
/// ordering, SKIP and LIMIT. An empty source creates and returns nothing;
/// result paging never suppresses creation effects.
/// `startNode`/`endNode` use the physical endpoints of created or directed
/// matched edges, including anonymous created vertices. Matched `labels`,
/// `type` and captured-path functions use the same native source projections
/// as read queries. Metadata keeps the source adapter's catalog and visibility
/// requirements; none of these functions performs a post-write graph scan.
///
/// Aggregate RETURN, labels/type of newly created elements, MATCH after
/// UNWIND and multi-statement RETURN scripts remain outside this subset.
#[derive(Clone)]
pub struct PreparedGraphInsertQueryText {
    pub(crate) statement: String,
    pub(crate) insertion: PreparedGraphInsertText,
    pub(crate) returning: InsertReturnTemplate,
}

impl core::fmt::Debug for PreparedGraphInsertQueryText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphInsertQueryText")
            .field("output_columns", &self.returning.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
