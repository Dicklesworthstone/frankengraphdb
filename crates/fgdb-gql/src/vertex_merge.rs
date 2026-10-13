//! Bounded unique-vertex MERGE definition.
//!
//! This is the typed get-or-create core for later native MERGE syntax. One
//! prepared MATCH chooses a vertex-valued target column. Duplicate occurrences
//! of the same identity are one match; two distinct identities are ambiguous.
//! If no target exists, one validated standalone insertion creates exactly one
//! vertex. There are deliberately no relationship MERGEs or ON MATCH/ON CREATE
//! actions in this increment; those compose above this exact primitive.

use crate::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::insertion::{GraphInsertError, PreparedGraphInsert};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryPolicy};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeBuildError {
    TargetColumn {
        column: usize,
    },
    CreationMustBeStandalone,
    CreationMustContainOneVertex {
        observed: usize,
    },
    CreationMustNotContainEdges {
        observed: usize,
    },
    RelationMismatch {
        selection: RelationId,
        creation: RelationId,
    },
}
impl core::fmt::Display for GraphVertexMergeBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph vertex MERGE definition: {self:?}")
    }
}
impl core::error::Error for GraphVertexMergeBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexMergePolicy {
    /// One cumulative allowance for match selection, uniqueness reduction and
    /// the standalone creation arm when it is needed.
    pub query: GqlQueryPolicy,
    /// Creation proposals, not matched vertices. Zero still permits a match,
    /// but refuses a missing vertex before requesting an external identity.
    pub max_created_vertices: u64,
}
impl GraphVertexMergePolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy) -> Self {
        Self {
            query,
            max_created_vertices: 1,
        }
    }

    #[must_use]
    pub const fn with_creation_limit(mut self, limit: u64) -> Self {
        self.max_created_vertices = limit;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexMergeStats {
    /// The MATCH phase only. A create branch's relational unit is internal and
    /// does not masquerade as a matched row.
    pub match_selection: GqlExecutionStats,
    /// Cumulative match-reduction plus optional creation evaluator work.
    pub evaluator: GlaExecutionStats,
    pub created_vertices: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphVertexMergeOutcome {
    Matched(VId),
    Created(VId),
}
impl GraphVertexMergeOutcome {
    #[must_use]
    pub const fn vertex(self) -> VId {
        match self {
            Self::Matched(vertex) | Self::Created(vertex) => vertex,
        }
    }
    #[must_use]
    pub const fn created(self) -> bool {
        matches!(self, Self::Created(_))
    }
}
impl core::fmt::Debug for GraphVertexMergeOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct(match self {
            Self::Matched(_) => "Matched",
            Self::Created(_) => "Created",
        })
        .field("vertex", &"[REDACTED]")
        .finish()
    }
}

#[derive(Debug)]
pub enum GraphVertexMergeError<E, A> {
    Source(E),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    AmbiguousMatches {
        observed: u64,
    },
    Creation(GraphInsertError<E, A>),
    AccountingOverflow,
    /// A MERGE over input rows read NULL or a non-scalar from its row for a
    /// property value (openCypher refuses a null MERGE property).
    RowValue {
        row: usize,
        column: usize,
    },
    /// A MERGE over input rows could not build one row's MERGE.
    RowMerge {
        row: usize,
        source: Box<crate::GraphVertexMergeTextError>,
    },
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphVertexMergeError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InvalidSourceStatistics => {
                f.write_str("MERGE source returned inconsistent statistics")
            }
            Self::InputSchema { row, column } => {
                write!(f, "MERGE row {row} has incompatible column {column}")
            }
            Self::AmbiguousMatches { observed } => write!(
                f,
                "MERGE matched {observed} distinct vertices; unique match required"
            ),
            Self::Creation(error) => write!(f, "MERGE creation: {error}"),
            Self::AccountingOverflow => {
                f.write_str("MERGE cumulative resource accounting overflow")
            }
            Self::RowValue { row, column } => write!(
                f,
                "MERGE row {row} column {column} is not a non-null scalar property value"
            ),
            Self::RowMerge { row, source } => write!(f, "MERGE row {row}: {source}"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphVertexMergeError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Creation(error) => Some(error),
            Self::RowMerge { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexMerge {
    selection: PreparedGraphPattern<GraphValueRow>,
    relation: RelationId,
    target: usize,
    creation: PreparedGraphInsert,
}
impl core::fmt::Debug for PreparedGraphVertexMerge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMerge")
            .field("target", &self.target)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphVertexMerge {
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        target: usize,
        creation: PreparedGraphInsert,
    ) -> Result<Self, GraphVertexMergeBuildError> {
        if !matches!(
            selection.value_columns().get(target),
            Some(ValueProjection::Vertex { .. })
        ) {
            return Err(GraphVertexMergeBuildError::TargetColumn { column: target });
        }
        if !creation.is_standalone() {
            return Err(GraphVertexMergeBuildError::CreationMustBeStandalone);
        }
        if creation.vertices_per_row() != 1 {
            return Err(GraphVertexMergeBuildError::CreationMustContainOneVertex {
                observed: creation.vertices_per_row(),
            });
        }
        if creation.edges_per_row() != 0 {
            return Err(GraphVertexMergeBuildError::CreationMustNotContainEdges {
                observed: creation.edges_per_row(),
            });
        }
        if creation.relation() != relation {
            return Err(GraphVertexMergeBuildError::RelationMismatch {
                selection: relation,
                creation: creation.relation(),
            });
        }
        Ok(Self {
            selection,
            relation,
            target,
            creation,
        })
    }

    #[must_use]
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.selection
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    #[must_use]
    pub const fn target_column(&self) -> usize {
        self.target
    }
    #[must_use]
    pub fn creation(&self) -> &PreparedGraphInsert {
        &self.creation
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-merge:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.extend_from_slice(&(self.target as u64).to_be_bytes());
        for definition in [
            self.selection.canonical_bytes(),
            self.creation.canonical_bytes(),
        ] {
            bytes.extend_from_slice(&(definition.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&definition);
        }
        bytes
    }
}

/// One property value of a vertex MERGE over input rows (fgdb-cpny9).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GraphVertexMergeRowValue {
    /// A literal or bound parameter, the same for every row.
    Bound(crate::GqlScalarParameter),
    /// One column of the input row.
    Column(usize),
}

/// A vertex MERGE over input rows: a step of a chained write (`MERGE (a:A)
/// MERGE (b:B)`, `CREATE (x) MERGE (:X {k: v})`, fgdb-cpny9). Each row's
/// property values form a tuple, and rows with equal tuples share one MERGE.
/// A tuple no vertex matches creates exactly one. A tuple several distinct
/// vertices match binds each of them, one output row apiece, as openCypher
/// does. The output row is the input row's visible columns followed by the
/// merged vertex.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexMergeRows {
    pub(crate) relation: RelationId,
    pub(crate) labels: Vec<fgdb_delta_types::LabelId>,
    /// In property-key order.
    pub(crate) properties: Vec<(fgdb_delta_types::PropertyKeyId, GraphVertexMergeRowValue)>,
    /// The input row's column types. `None` is the one empty row of a
    /// statement's first write.
    pub(crate) input: Option<Vec<crate::GraphSetColumnType>>,
    /// Leading input columns the output keeps. Hidden reads after them are
    /// the MERGE's own inputs, read by no later write.
    pub(crate) keep: usize,
}
impl core::fmt::Debug for PreparedGraphVertexMergeRows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMergeRows")
            .field("properties", &self.properties.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphVertexMergeRows {
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }

    /// The input row's column types; `None` for a statement's first write.
    #[must_use]
    pub fn input_column_types(&self) -> Option<&[crate::GraphSetColumnType]> {
        self.input.as_deref()
    }

    /// The rows this MERGE runs over: `owned`, the previous write's rows, or
    /// the one empty row of a first write. Owned rows must have the input
    /// width, and a first write takes none.
    pub fn input_rows<E, A>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
    ) -> Result<Vec<GraphValueRow>, GraphVertexMergeError<E, A>> {
        match (&self.input, owned) {
            (None, None) => Ok(vec![GraphValueRow::from_owned_values(Vec::new())]),
            (Some(types), Some(rows)) => {
                if let Some(row) = rows.iter().position(|row| row.len() != types.len()) {
                    return Err(GraphVertexMergeError::InputSchema {
                        row,
                        column: types.len(),
                    });
                }
                Ok(rows)
            }
            (Some(types), None) => Err(GraphVertexMergeError::InputSchema {
                row: 0,
                column: types.len(),
            }),
            (None, Some(_)) => Err(GraphVertexMergeError::InputSchema { row: 0, column: 0 }),
        }
    }

    /// The property tuple of input row `at`, in property-key order. A NULL
    /// or non-scalar row value refuses: openCypher cannot MERGE on null.
    pub fn tuple<E, A>(
        &self,
        at: usize,
        row: &GraphValueRow,
    ) -> Result<Vec<fgdb_types::CanonicalScalar>, GraphVertexMergeError<E, A>> {
        self.properties
            .iter()
            .map(|(_, value)| match value {
                GraphVertexMergeRowValue::Bound(value) => Ok(value.value().clone()),
                GraphVertexMergeRowValue::Column(column) => row
                    .values()
                    .get(*column)
                    .and_then(crate::algebra::GraphValue::as_scalar)
                    .filter(|value| !matches!(value, fgdb_types::CanonicalScalar::Null))
                    .cloned()
                    .ok_or(GraphVertexMergeError::RowValue {
                        row: at,
                        column: *column,
                    }),
            })
            .collect()
    }

    /// One output row: the input row's kept columns, then the vertex.
    #[must_use]
    pub fn output_row(&self, row: &GraphValueRow, vertex: VId) -> GraphValueRow {
        let mut values = row.values()[..self.keep.min(row.len())].to_vec();
        values.push(crate::algebra::GraphValue::Vertex(vertex));
        GraphValueRow::from_owned_values(values)
    }
}
