//! A chained write statement (fgdb-cpny9): `CREATE (a) WITH a CREATE (b)`,
//! `MATCH (n) WITH n AS a CREATE (a)-[:T]->() WITH a AS x CREATE (x)-[:T]->()`.
//!
//! Each write after the first runs once per row the previous write's WITH
//! tail returned (openCypher: a clause after a write sees the post-write
//! state, row by row). Every step is the existing native CREATE family; a
//! later step's relational input is one owned leaf (`SetNode::Owned`) that the
//! host fills with the previous step's rows, so a carried element is never
//! re-read from the graph and its properties are the hidden head columns the
//! previous write computed. One statement is one transaction: the host stages
//! each write before the next reads its rows, all inside one atomic workspace,
//! under one cumulative allowance.

use crate::insertion::PreparedGraphInsert;
use crate::insertion_query_text::InsertReturnTemplate;
use crate::{PreparedGraphInsertQuery, PreparedGraphInsertText};

/// A parsed chain: two or more writes, each but the last with a WITH head
/// whose tail ends at the next write. The last write may have a RETURN.
#[derive(Clone)]
pub struct PreparedGraphWriteChainText {
    pub(crate) statement: String,
    pub(crate) steps: Vec<(PreparedGraphInsertText, Option<InsertReturnTemplate>)>,
}

impl core::fmt::Debug for PreparedGraphWriteChainText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteChainText")
            .field("steps", &self.steps.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// One bound write of a chain.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphWriteChainStep {
    /// A write with a WITH head (every step but the last) or a RETURN (the
    /// last step): it returns rows.
    Query(PreparedGraphInsertQuery),
    /// The last write when the statement has no RETURN.
    Insert(PreparedGraphInsert),
}

impl core::fmt::Debug for GraphWriteChainStep {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Query(_) => "Query([REDACTED])",
            Self::Insert(_) => "Insert([REDACTED])",
        })
    }
}

/// A bound chain. The host executes `steps()` in order, handing each step's
/// rows to the next (`PreparedGraphInsertQuery::execute_governed_over`).
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphWriteChain {
    pub(crate) steps: Vec<GraphWriteChainStep>,
}

impl core::fmt::Debug for PreparedGraphWriteChain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteChain")
            .field("steps", &self.steps.len())
            .finish()
    }
}

impl PreparedGraphWriteChain {
    #[must_use]
    pub fn steps(&self) -> &[GraphWriteChainStep] {
        &self.steps
    }

    /// The statement's result columns: the last step's RETURN, or none.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        match self.steps.last() {
            Some(GraphWriteChainStep::Query(query)) => query.columns(),
            _ => &[],
        }
    }

    /// Whether the statement returns rows (its last write has a RETURN).
    #[must_use]
    pub fn returns_rows(&self) -> bool {
        matches!(self.steps.last(), Some(GraphWriteChainStep::Query(_)))
    }
}
