//! Appendix C `ProcedureCall`: a registered procedure's result relation as a
//! read-pipeline source (`CALL ns.name(args) YIELD out [AS alias], ...`).
//!
//! Loom only names, types and positions the call. The host resolves and runs
//! it (Prism for `fnx.*`) through [`GraphSetSource::procedure`], so this crate
//! never depends on a procedure implementation. A host that supplies no
//! procedures refuses with `ProcedureUnavailable`; an unknown name or bad
//! argument is the host's typed refusal. Yield columns are the Any domain, as
//! UNWIND columns are: their values are checked where they are used.

use crate::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use crate::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetValue};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalScalar;

/// A procedure's induced graph, resolved during preparation. The selection
/// narrows the host's already authorized snapshot; it grants no authority and
/// never chooses another branch, generation, or database. The current native
/// projection admits one optional label and one optional relation, preserving
/// isolated selected vertices and retaining edges only when both endpoints
/// belong to the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcedureGraphProjection {
    pub vertex_label: Option<LabelId>,
    pub relation: Option<RelationId>,
    pub weight: ProcedureGraphWeight,
    /// None preserves the registered procedure's required/default direction.
    pub direction: Option<ProcedureGraphDirection>,
    pub parallel_edges: ProcedureParallelEdges,
    pub self_loops: ProcedureSelfLoops,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcedureGraphWeight {
    Unit,
    Property {
        key: PropertyKeyId,
        missing: ProcedureMissingWeight,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcedureMissingWeight {
    Reject = 0,
    Unit = 1,
    Zero = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcedureGraphDirection {
    Directed = 0,
    Reversed = 1,
    Undirected = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcedureParallelEdges {
    Reject = 0,
    CollapseUnit = 1,
    Minimum = 2,
    Maximum = 3,
    Sum = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcedureSelfLoops {
    Keep = 0,
    Drop = 1,
    Reject = 2,
}

impl ProcedureGraphProjection {
    pub(crate) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        for id in [
            self.vertex_label.map(|id| id.0),
            self.relation.map(|id| id.0),
        ] {
            match id {
                None => bytes.push(0),
                Some(id) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&id.to_be_bytes());
                }
            }
        }
        match self.weight {
            ProcedureGraphWeight::Unit => bytes.push(0),
            ProcedureGraphWeight::Property { key, missing } => {
                bytes.push(1);
                bytes.extend_from_slice(&key.0.to_be_bytes());
                bytes.push(missing as u8);
            }
        }
        bytes.push(self.direction.map_or(0, |direction| direction as u8 + 1));
        bytes.push(self.parallel_edges as u8);
        bytes.push(self.self_loops as u8);
    }
}

/// A prepared, bound procedure invocation. Arguments are constant values
/// (literals or bound parameters), evaluated once per execution.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedProcedureCall {
    pub(super) namespace: String,
    pub(super) name: String,
    pub(super) arguments: Vec<GraphSetValue>,
    /// One name per argument for a named call; empty for a positional one.
    pub(super) names: Vec<String>,
    /// Catalog-bound graph argument, separate from runtime algorithm values.
    pub(super) graph_projection: Option<ProcedureGraphProjection>,
    /// The procedure's own output names, in YIELD order.
    pub(super) outputs: Vec<String>,
    /// Ascending positions in `outputs` the statement matches as vertices.
    pub(super) vertices: Vec<usize>,
}

impl PreparedProcedureCall {
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The argument names, parallel to the evaluated arguments, when the
    /// call named them (`k => 10`); empty for a positional call. A host that
    /// takes only positional arguments must refuse a named call. A schema-name
    /// argument already carries the integer identity its name resolved to.
    #[must_use]
    pub fn argument_names(&self) -> &[String] {
        &self.names
    }
    /// The optional query-local graph argument. Its labels, relation and
    /// weight property are frozen catalog identities, never runtime strings.
    #[must_use]
    pub fn graph_projection(&self) -> Option<&ProcedureGraphProjection> {
        self.graph_projection.as_ref()
    }
    /// The procedure output columns the host must return, in this order.
    #[must_use]
    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }
    /// Positions in [`Self::outputs`] the statement uses as vertices: an
    /// identity MATCH on the column, written (`YIELD n MATCH (n)`) or implied
    /// by a property read (`YIELD n RETURN n.p`). The host must refuse the
    /// call unless each is a vertex-valued output; a scalar there would
    /// otherwise match nothing and drop every row silently.
    #[must_use]
    pub fn vertex_outputs(&self) -> &[usize] {
        &self.vertices
    }
    /// Whether the procedure's own deterministic ranking order (rather than
    /// canonical tuple order) is its output contract when no explicit ORDER BY
    /// overrides it. `hybrid.search` returns top-k hits ordered by fused score
    /// descending, ties broken by ascending vertex ID.
    #[must_use]
    pub fn preserves_row_order(&self) -> bool {
        self.namespace == "hybrid"
    }
    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        for text in [&self.namespace, &self.name] {
            bytes.extend_from_slice(&(text.len() as u64).to_be_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        // A named call sets the count's high bit and prefixes each value by
        // its name, so every positional call keeps its exact bytes.
        let named = if self.names.is_empty() { 0 } else { 1 << 63 };
        let projected = if self.graph_projection.is_some() {
            1 << 62
        } else {
            0
        };
        bytes.extend_from_slice(&(self.arguments.len() as u64 | named | projected).to_be_bytes());
        if let Some(projection) = &self.graph_projection {
            projection.append_transcript(bytes);
        }
        for (index, argument) in self.arguments.iter().enumerate() {
            if let Some(name) = self.names.get(index) {
                bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
                bytes.extend_from_slice(name.as_bytes());
            }
            super::projection::append_value_transcript(argument, bytes);
        }
        bytes.extend_from_slice(&(self.outputs.len() as u64).to_be_bytes());
        for output in &self.outputs {
            bytes.extend_from_slice(&(output.len() as u64).to_be_bytes());
            bytes.extend_from_slice(output.as_bytes());
        }
        bytes.extend_from_slice(&(self.vertices.len() as u64).to_be_bytes());
        for &output in &self.vertices {
            bytes.extend_from_slice(&(output as u64).to_be_bytes());
        }
    }
}

/// The trusted row sources one set execution may read: graph patterns and,
/// when the host supplies them, procedure calls. Every existing closure over
/// patterns is a source with no procedures.
pub trait GraphSetSource<E, C> {
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>;

    /// `None` means this host executes no procedures. Rows must carry exactly
    /// `call.outputs()` columns, in that order.
    fn procedure(
        &mut self,
        _call: &PreparedProcedureCall,
        _arguments: &[GraphValue],
        _policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        None
    }

    /// A write tail's owned head rows (`SetNode::Owned`), taken once. `None`
    /// means this host supplies none; only a write executes such a tail.
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        None
    }

    /// One stored property of a vertex or edge value, read at the same pinned
    /// cut as every pattern, for `x.key` over an element an expression produced:
    /// a path's nodes, a collected list, an UNWIND row (fgdb-xnd7x). `Ok(None)`
    /// is an absent property. `None` means this host reads no element
    /// properties, so the read stays a typed error, never a guessed NULL.
    fn element_property(
        &mut self,
        _element: ElementId,
        _key: PropertyKeyId,
    ) -> Option<Result<Option<CanonicalScalar>, E>> {
        None
    }
}

impl<E, C, F> GraphSetSource<E, C> for F
where
    F: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self(pattern, policy)
    }
}

/// A pattern source plus a procedure host.
pub struct WithProcedures<S, P> {
    pub patterns: S,
    pub procedures: P,
}

impl<E, C, S, P> GraphSetSource<E, C> for WithProcedures<S, P>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    P: FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        (self.patterns)(pattern, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        Some((self.procedures)(call, arguments, policy))
    }
}

/// A source plus the host's element property reader (fgdb-xnd7x).
pub struct WithElementReads<S, R> {
    pub source: S,
    pub elements: R,
}

impl<E, C, S, R> GraphSetSource<E, C> for WithElementReads<S, R>
where
    S: GraphSetSource<E, C>,
    R: FnMut(ElementId, PropertyKeyId) -> Result<Option<CanonicalScalar>, E>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<CanonicalScalar>, E>> {
        Some((self.elements)(element, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set_text::{ReadOrderProjection, ReadPageNumber, ReadStageTemplate};

    #[test]
    fn unprojected_call_keeps_the_existing_transcript_fixture() {
        let call = PreparedProcedureCall {
            namespace: "fnx".to_owned(),
            name: "p".to_owned(),
            arguments: Vec::new(),
            names: Vec::new(),
            graph_projection: None,
            outputs: vec!["v".to_owned()],
            vertices: Vec::new(),
        };
        let mut bytes = Vec::new();
        call.append_transcript(&mut bytes);
        // Fixed pre-projection encoding: namespace, procedure, zero args,
        // one output named v, zero vertex-use requirements.
        let fixture = concat!(
            "\0\0\0\0\0\0\0\x03fnx",
            "\0\0\0\0\0\0\0\x01p",
            "\0\0\0\0\0\0\0\0",
            "\0\0\0\0\0\0\0\x01",
            "\0\0\0\0\0\0\0\x01v",
            "\0\0\0\0\0\0\0\0",
        );
        assert_eq!(bytes, fixture.as_bytes());
    }

    #[test]
    fn projected_call_and_computed_page_have_distinct_template_tags() {
        let mut call = ReadStageTemplate::Call {
            at: 0,
            namespace: "fnx".to_owned(),
            name: "p".to_owned(),
            arguments: Vec::new(),
            names: Vec::new(),
            offsets: Vec::new(),
            outputs: vec![("v".to_owned(), "v".to_owned())],
            vertices: Vec::new(),
            graph_projection: None,
        };
        let mut unprojected = Vec::new();
        call.append_template_transcript(&mut unprojected);
        assert_eq!(unprojected[0], 5);
        if let ReadStageTemplate::Call {
            graph_projection, ..
        } = &mut call
        {
            *graph_projection = Some(ProcedureGraphProjection {
                vertex_label: None,
                relation: None,
                weight: ProcedureGraphWeight::Unit,
                direction: None,
                parallel_edges: ProcedureParallelEdges::Reject,
                self_loops: ProcedureSelfLoops::Keep,
            });
        }
        let mut projected = Vec::new();
        call.append_template_transcript(&mut projected);
        let page = ReadStageTemplate::Page {
            at: 0,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            computed: Some(ReadOrderProjection {
                values: Vec::new(),
                visible: 0,
            }),
        };
        let mut computed = Vec::new();
        page.append_template_transcript(&mut computed);
        assert_eq!(computed[0], 7);
        assert_eq!(projected[0], 8);
        assert_ne!(projected[0], computed[0]);
    }
}
