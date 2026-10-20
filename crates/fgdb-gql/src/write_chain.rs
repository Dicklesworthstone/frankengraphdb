//! A chained write statement (fgdb-cpny9): `CREATE (a) WITH a CREATE (b)`,
//! `MATCH (n) WITH n AS a CREATE (a)-[:T]->() WITH a AS x CREATE (x)-[:T]->()`,
//! `CREATE (a), (b) MERGE (a)-[:R]->(b) RETURN count(a)`.
//!
//! Each write after the first runs once per row the previous write's WITH
//! tail returned (openCypher: a clause after a write sees the post-write
//! state, row by row). A write directly after a CREATE reads every binding,
//! as `WITH *` would. Every step is an existing native write family; a later
//! step's relational input is one owned leaf (`SetNode::Owned`) that the host
//! fills with the previous step's rows, so a carried element is never re-read
//! from the graph and its properties are the hidden head columns the previous
//! write computed. One statement is one transaction: the host stages each
//! write before the next reads its rows, all inside one atomic workspace,
//! under one cumulative allowance.

use crate::insertion::PreparedGraphInsert;
use crate::insertion_query_text::InsertReturnTemplate;
use crate::mutation_text::{EdgeMergeReturnTemplate, PreparedGraphVertexMergeRowsText};
use crate::{
    GraphEdgeMergeQueryError, GraphEdgeMergeTextError, GraphInsertQueryError, GraphInsertTextError,
    GraphVertexMergeTextError, PreparedGraphEdgeMerge, PreparedGraphEdgeMergeQuery,
    PreparedGraphEdgeMergeText, PreparedGraphInsertQuery, PreparedGraphInsertText,
    PreparedGraphVertexMergeRows,
};

/// One parsed write of a chain.
#[derive(Clone)]
pub(crate) enum ChainTextStep {
    /// CREATE, with its WITH head (every step but the last) or RETURN.
    Insert(Box<(PreparedGraphInsertText, Option<InsertReturnTemplate>)>),
    /// A relationship MERGE over the previous write's rows (the last step).
    EdgeMerge(Box<(PreparedGraphEdgeMergeText, Option<EdgeMergeReturnTemplate>)>),
    /// A relationship MERGE another write follows: its rows are the row's
    /// first `usize` (visible) columns and the relationship.
    EdgeMergeRows(Box<(PreparedGraphEdgeMergeText, usize)>),
    /// A vertex MERGE over the previous write's rows, or over one empty row
    /// as the first write; any step but one with RETURN.
    VertexMerge(Box<PreparedGraphVertexMergeRowsText>),
}

/// A parsed chain: two or more writes, each but the last with a (written or
/// implicit) WITH head whose tail ends at the next write. The last write may
/// have a RETURN.
#[derive(Clone)]
pub struct PreparedGraphWriteChainText {
    pub(crate) statement: String,
    pub(crate) steps: Vec<ChainTextStep>,
}

impl core::fmt::Debug for PreparedGraphWriteChainText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteChainText")
            .field("steps", &self.steps.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Preparing or binding a chain: the failing step's own family error.
#[derive(Debug)]
pub enum GraphWriteChainTextError {
    Insert(GraphInsertTextError),
    EdgeMerge(GraphEdgeMergeTextError),
    VertexMerge(GraphVertexMergeTextError),
}
impl From<GraphInsertTextError> for GraphWriteChainTextError {
    fn from(error: GraphInsertTextError) -> Self {
        Self::Insert(error)
    }
}
impl From<GraphVertexMergeTextError> for GraphWriteChainTextError {
    fn from(error: GraphVertexMergeTextError) -> Self {
        Self::VertexMerge(error)
    }
}
impl From<GraphEdgeMergeTextError> for GraphWriteChainTextError {
    fn from(error: GraphEdgeMergeTextError) -> Self {
        Self::EdgeMerge(error)
    }
}
impl From<crate::GraphPatternTextError> for GraphWriteChainTextError {
    fn from(error: crate::GraphPatternTextError) -> Self {
        Self::Insert(error.into())
    }
}
impl core::fmt::Display for GraphWriteChainTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Insert(error) => error.fmt(f),
            Self::EdgeMerge(error) => error.fmt(f),
            Self::VertexMerge(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphWriteChainTextError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Insert(error) => Some(error),
            Self::EdgeMerge(error) => Some(error),
            Self::VertexMerge(error) => Some(error),
        }
    }
}

/// Executing a chain: the failing step's own family error. Earlier steps'
/// staged effects are restored by the host's workspace, as for every write.
#[derive(Debug)]
pub enum GraphWriteChainError<E, A> {
    Insert(GraphInsertQueryError<E, A>),
    EdgeMerge(GraphEdgeMergeQueryError<E, A>),
    VertexMerge(crate::GraphVertexMergeRowsQueryError<E, A>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphWriteChainError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Insert(error) => error.fmt(f),
            Self::EdgeMerge(error) => error.fmt(f),
            Self::VertexMerge(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphWriteChainError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Insert(error) => Some(error),
            Self::EdgeMerge(error) => Some(error),
            Self::VertexMerge(error) => Some(error),
        }
    }
}

/// One bound write of a chain.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphWriteChainStep {
    /// A CREATE with a WITH head (every step but the last) or a RETURN (the
    /// last step): it returns rows.
    Query(Box<PreparedGraphInsertQuery>),
    /// The last write, a CREATE without RETURN.
    Insert(PreparedGraphInsert),
    /// The last write, a relationship MERGE over the previous write's rows,
    /// without RETURN.
    EdgeMerge(Box<PreparedGraphEdgeMerge>),
    /// The last write, a relationship MERGE with RETURN.
    EdgeMergeQuery(Box<PreparedGraphEdgeMergeQuery>),
    /// A relationship MERGE another write follows. Its rows: each input
    /// row's first `keep` columns and each relationship its pair resolved to.
    EdgeMergeRows {
        merge: Box<PreparedGraphEdgeMerge>,
        keep: usize,
    },
    /// A vertex MERGE over the previous write's rows (or one empty row, or
    /// a leading UNWIND's rows, as the first write). Its rows are the input
    /// row and the merged vertex.
    VertexMerge(Box<PreparedGraphVertexMergeRows>),
    /// The last write, a vertex MERGE with RETURN.
    VertexMergeQuery(Box<crate::PreparedGraphVertexMergeRowsQuery>),
}

impl core::fmt::Debug for GraphWriteChainStep {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Query(_) => "Query([REDACTED])",
            Self::Insert(_) => "Insert([REDACTED])",
            Self::EdgeMerge(_) => "EdgeMerge([REDACTED])",
            Self::EdgeMergeQuery(_) => "EdgeMergeQuery([REDACTED])",
            Self::EdgeMergeRows { .. } => "EdgeMergeRows([REDACTED])",
            Self::VertexMerge(_) => "VertexMerge([REDACTED])",
            Self::VertexMergeQuery(_) => "VertexMergeQuery([REDACTED])",
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
            Some(GraphWriteChainStep::EdgeMergeQuery(query)) => query.columns(),
            Some(GraphWriteChainStep::VertexMergeQuery(query)) => query.columns(),
            _ => &[],
        }
    }

    /// Whether the statement returns rows (its last write has a RETURN).
    #[must_use]
    pub fn returns_rows(&self) -> bool {
        matches!(
            self.steps.last(),
            Some(
                GraphWriteChainStep::Query(_)
                    | GraphWriteChainStep::EdgeMergeQuery(_)
                    | GraphWriteChainStep::VertexMergeQuery(_)
            )
        )
    }
}
