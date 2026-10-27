//! Appendix C `ProcedureCall`: a registered procedure's result relation as a
//! read-pipeline source (`CALL ns.name(args) YIELD out [AS alias], ...`).
//!
//! Loom only names, types and positions the call. The host resolves and runs
//! it (Prism for `fnx.*`) through [`GraphSetSource::procedure`], so this crate
//! never depends on a procedure implementation. A host that supplies no
//! procedures refuses with `ProcedureUnavailable`; an unknown name or bad
//! argument is the host's typed refusal. Yield columns are the Any domain, as
//! UNWIND columns are: their values are checked where they are used.

use crate::algebra::{
    GraphValue, GraphValueRow, PreparedGraphPattern, PreparedGraphPatternFactors,
};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetValue,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::VId;

macro_rules! forward_metadata_to_source {
    () => {
        fn element_metadata(
            &mut self,
            element: ElementId,
            kind: super::projection::GraphElementMetadata,
            catalog: &crate::ReverseSymbolCatalog,
            control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
        ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
            self.source
                .element_metadata(element, kind, catalog, control)
        }
    };
}

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

/// Both complete factors from one original graph admission. `result_rows`
/// counts the sum of retained factor rows, not their Cartesian product and
/// not external result rows. `snapshot_records` counts the original admission
/// once; evaluator statistics include all admission and both evaluations.
/// Each factor's first column contains unique, non-null vertex identities.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphPatternFactorExecution {
    pub value: [Vec<GraphValueRow>; 2],
    pub rows: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
}

impl core::fmt::Debug for GraphPatternFactorExecution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphPatternFactorExecution")
            .field("value", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .finish()
    }
}

/// Pull rows from a procedure that has completed its source validation and
/// certification. The cursor owns its state and does not borrow its host, so
/// downstream expressions may use that host's ordinary element reader.
/// Opening the cursor must validate all value domains, even under LIMIT 0.
pub trait GraphProcedureRowCursor<E, C> {
    fn next_row(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), C>,
    ) -> Result<Option<GraphValueRow>, GqlQueryError<E, C>>;
}

/// Complete admission statistics precede the first downstream row. Output
/// rows are in canonical tuple order and lead with strictly ascending unique
/// vertex identities. `rows.result_rows` is the exact cursor cardinality.
/// The cursor is private execution state, not externally delivered rows.
pub struct GraphProcedureExecution<E, C> {
    pub cursor: Box<dyn GraphProcedureRowCursor<E, C>>,
    pub rows: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
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

    /// Definition-only capability: this host handles every checked two-factor
    /// request. Inspect no source data, allocate nothing and record no read.
    /// Its admitted graph domain contains each vertex identity exactly once.
    /// `true` promises `pattern_factors` returns Some; violating that promise
    /// is a source-contract error, never a second ordinary source execution.
    fn supports_pattern_factors(&self) -> bool {
        false
    }

    /// An optional physical product owner for one original graph pattern.
    /// The default `None` is an unsupported capability and MUST perform no
    /// source work or observation. Sources advertising no factor capability
    /// execute the unchanged logical leaf without constructing a request.
    /// A returned result, including an error, never falls back or reopens it.
    ///
    /// Admit `request.original()` once at the same pinned and authorized cut as
    /// `pattern`, retaining its complete source and witness domain. Required
    /// labels and stored properties must already be total table lookups after
    /// admission; factor evaluation must not introduce lazy source reads. Evaluate
    /// BOTH factors against those admitted tables before returning, even if a
    /// factor or the consumer's leading rows are empty. This boundary precedes
    /// all imported-key filtering. Each factor has its declared columns in
    /// their original local order, starting with a unique non-null vertex
    /// identity from the original source's unique vertex domain. The consumer
    /// rejects repeated or null factor identities before key matching. The
    /// private-row allowance is independent of final result limits; all actual
    /// work and scratch remain cumulative.
    fn pattern_factors(
        &mut self,
        _request: &PreparedGraphPatternFactors<'_>,
        _policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        None
    }

    /// Definition-only capability: this host answers `seeded_pattern` for a
    /// single-root vertex pattern. Inspect no source data and record no read.
    /// `false` keeps every consumer on the unchanged logical leaf, charging
    /// nothing for a seed set it never builds.
    fn supports_seeded_patterns(&self) -> bool {
        false
    }

    /// An optional physical restriction of one original graph pattern whose
    /// one vertex root a leading bag binds by identity (fgdb-2acff): `CALL
    /// ... YIELD node RETURN node.p`, or a MATCH vertex reusing an UNWIND
    /// row's vertex. `seeds` holds the leading bag's distinct root identities
    /// in VId order. Return every row of `pattern` whose root is one of them,
    /// at the same pinned and authorized cut as `pattern`; other rows of
    /// `pattern` may also be returned, since the consumer still joins on the
    /// identity equality. Charge the records actually admitted, not the graph.
    /// `None` (the default, or a host declining this pattern's shape) MUST
    /// perform no source work; the consumer then executes the unchanged leaf.
    /// A returned result, including an error, never falls back or reopens it.
    fn seeded_pattern(
        &mut self,
        _pattern: &PreparedGraphPattern<GraphValueRow>,
        _seeds: &[VId],
        _policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        None
    }

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

    /// Definition-only capability for every canonical vertex-first FnX call.
    /// A supporting host returns Some from `procedure_rows`, including typed
    /// binding/source refusals; it may not silently reopen a materialized call.
    fn supports_procedure_rows(&self) -> bool {
        false
    }

    /// Optional completed cursor for a canonical vertex-first procedure.
    /// A host without this capability uses the unchanged materialized
    /// procedure. Its default `None` must do no source work. A host advertising
    /// support must return `Some`; `None` is a source-contract error, never a
    /// materialized retry. A returned error is authoritative and never falls back.
    /// No row may expose a deferred source/domain/certificate failure: all
    /// such validation must finish before this method returns successfully.
    fn procedure_rows(
        &mut self,
        _call: &PreparedProcedureCall,
        _arguments: &[GraphValue],
        _policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
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
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        None
    }

    /// Read visible metadata with the same cut and authority as `pattern`.
    /// Reserve work and scratch BEFORE retaining metadata or copying payloads.
    /// A control error is a stop signal: return `Interrupted` immediately.
    /// Outer `None` declines this capability; inner `None` is an absent or
    /// masked element. The evaluator restores the original control failure.
    fn element_metadata(
        &mut self,
        _element: ElementId,
        _kind: super::projection::GraphElementMetadata,
        _catalog: &crate::ReverseSymbolCatalog,
        _control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        None
    }
}

/// Borrow a host through a consuming row phase, then reuse that same host for
/// the aggregate's output expressions. Iterator entrypoints can attach their
/// optional element reader without turning an unsupported read into NULL.
pub(crate) struct BorrowedSource<'s, 'r, E, C> {
    pub(crate) source: &'s mut dyn GraphSetSource<E, C>,
    pub(crate) elements: Option<&'s mut super::projection::ElementRead<'r, E>>,
}

impl<E, C> GraphSetSource<E, C> for BorrowedSource<'_, '_, E, C> {
    fn supports_procedure_rows(&self) -> bool {
        self.source.supports_procedure_rows()
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }

    fn supports_pattern_factors(&self) -> bool {
        self.source.supports_pattern_factors()
    }

    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        self.source.pattern_factors(request, policy)
    }

    fn supports_seeded_patterns(&self) -> bool {
        self.source.supports_seeded_patterns()
    }

    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.seeded_pattern(pattern, seeds, policy)
    }

    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }

    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        self.source.procedure_rows(call, arguments, policy)
    }

    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }

    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        match &mut self.elements {
            Some(read) => read.property(element, key),
            None => self.source.element_property(element, key),
        }
    }
    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: super::projection::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        match &mut self.elements {
            Some(read) => read.metadata(element, kind, catalog, control),
            None => self
                .source
                .element_metadata(element, kind, catalog, control),
        }
    }
}

/// A caller's pattern callback, erased at a governed set boundary so the
/// set engine is compiled once per error pair, not once per caller closure
/// (fgdb-2goz).
pub(crate) type DynPatterns<'a, E, C> = dyn FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>
    + 'a;

/// A caller's seeded pattern callback (fgdb-2acff), erased at the same
/// boundary. `None` declines the pattern's shape.
pub(crate) type DynSeeded<'a, E, C> = dyn FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        &[VId],
        GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>>
    + 'a;

/// A caller's procedure callback, erased at the same boundary.
pub(crate) type DynProcedures<'a, E, C> = dyn FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>
    + 'a;

/// Borrow one caller-specific pattern callback behind one stable source type.
/// This is an execution-code-size boundary only: it forwards the exact policy,
/// rows, errors and control domain without buffering or fallback.
pub(crate) struct PatternSource<'a, E, C> {
    pub(crate) patterns: &'a mut DynPatterns<'a, E, C>,
}

impl<E, C> GraphSetSource<E, C> for PatternSource<'_, E, C> {
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        (self.patterns)(pattern, policy)
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

/// A source plus an optional completed procedure cursor. Existing pattern,
/// property and materialized procedure semantics remain with `source`.
pub struct WithProcedureRows<S, P> {
    pub source: S,
    pub procedures: P,
}

/// Compose pinned source callbacks with completed procedure cursors.
/// Materialized procedures remain available for other output orders and
/// operators that require their complete input bag.
pub fn procedure_row_source<E, C>(
    patterns: impl FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    seeded: impl FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        &[VId],
        GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>>,
    procedures: impl FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    rows: impl FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>>,
    elements: impl FnMut(
        ElementId,
        PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>,
) -> impl GraphSetSource<E, C> {
    WithProcedureRows {
        source: WithSeededPatterns {
            source: WithElementReads {
                source: WithProcedures {
                    patterns,
                    procedures,
                },
                elements,
            },
            seeded,
        },
        procedures: rows,
    }
}

impl<E, C, S, P> GraphSetSource<E, C> for WithProcedureRows<S, P>
where
    S: GraphSetSource<E, C>,
    P: FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>>,
{
    forward_metadata_to_source!();
    fn supports_procedure_rows(&self) -> bool {
        true
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn supports_pattern_factors(&self) -> bool {
        self.source.supports_pattern_factors()
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        self.source.pattern_factors(request, policy)
    }
    fn supports_seeded_patterns(&self) -> bool {
        self.source.supports_seeded_patterns()
    }
    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.seeded_pattern(pattern, seeds, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        (self.procedures)(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        self.source.element_property(element, key)
    }
}

/// A source plus the host's pinned element reader. Ordinary property callbacks
/// remain supported; a complete reader also retains controlled metadata.
pub struct WithElementReads<S, R> {
    pub source: S,
    pub elements: R,
}

/// Pinned element access for the ordinary aggregate binding visitor. A
/// property callback implements this trait directly; hosts can attach the
/// same controlled metadata capability used by GraphSetSource through
/// [`with_element_metadata`]. Every read shares the input's snapshot and
/// authority, including identities carried in parameters and list values.
pub trait GraphElementReader<E> {
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>;

    /// Charge before constructing any owned metadata. `None` means the
    /// capability is unavailable; `Some(Ok(None))` means the element is absent
    /// or masked. A control refusal must stop construction and return
    /// GraphElementMetadataError::Interrupted; the evaluator preserves the
    /// original refusal and exposes no partial value.
    fn element_metadata(
        &mut self,
        _element: ElementId,
        _kind: super::projection::GraphElementMetadata,
        _catalog: &crate::ReverseSymbolCatalog,
        _control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        None
    }
}

impl<E, F> GraphElementReader<E> for F
where
    F: FnMut(ElementId, PropertyKeyId) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>,
{
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E> {
        self(element, key)
    }
}

/// Attach a controlled metadata reader without replacing any other source
/// capability. This also attaches metadata to an aggregate's property
/// callback. The callback receives the executing expression's control.
pub struct WithElementMetadata<S, R> {
    pub source: S,
    pub metadata: R,
}

pub fn with_element_metadata<S, R, E>(source: S, metadata: R) -> WithElementMetadata<S, R>
where
    R: FnMut(
        ElementId,
        super::projection::GraphElementMetadata,
        &crate::ReverseSymbolCatalog,
        &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>,
{
    WithElementMetadata { source, metadata }
}

impl<E, S, R> GraphElementReader<E> for WithElementMetadata<S, R>
where
    S: GraphElementReader<E>,
    R: FnMut(
        ElementId,
        super::projection::GraphElementMetadata,
        &crate::ReverseSymbolCatalog,
        &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>,
{
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E> {
        self.source.element_property(element, key)
    }

    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: super::projection::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        Some((self.metadata)(element, kind, catalog, control))
    }
}

impl<E, C, S, R> GraphSetSource<E, C> for WithElementMetadata<S, R>
where
    S: GraphSetSource<E, C>,
    R: FnMut(
        ElementId,
        super::projection::GraphElementMetadata,
        &crate::ReverseSymbolCatalog,
        &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>,
{
    fn supports_procedure_rows(&self) -> bool {
        self.source.supports_procedure_rows()
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn supports_pattern_factors(&self) -> bool {
        self.source.supports_pattern_factors()
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        self.source.pattern_factors(request, policy)
    }
    fn supports_seeded_patterns(&self) -> bool {
        self.source.supports_seeded_patterns()
    }
    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.seeded_pattern(pattern, seeds, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        self.source.procedure_rows(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        self.source.element_property(element, key)
    }
    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: super::projection::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        Some((self.metadata)(element, kind, catalog, control))
    }
}

impl<E, C, S, R> GraphSetSource<E, C> for WithElementReads<S, R>
where
    S: GraphSetSource<E, C>,
    R: GraphElementReader<E>,
{
    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: super::projection::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<GraphValue>, super::projection::GraphElementMetadataError<E>>> {
        match self
            .elements
            .element_metadata(element, kind, catalog, control)
        {
            Some(result) => Some(result),
            None => self
                .source
                .element_metadata(element, kind, catalog, control),
        }
    }
    fn supports_procedure_rows(&self) -> bool {
        self.source.supports_procedure_rows()
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn supports_pattern_factors(&self) -> bool {
        self.source.supports_pattern_factors()
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        self.source.pattern_factors(request, policy)
    }
    fn supports_seeded_patterns(&self) -> bool {
        self.source.supports_seeded_patterns()
    }
    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.seeded_pattern(pattern, seeds, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        self.source.procedure_rows(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        Some(self.elements.element_property(element, key))
    }
}

/// A source plus the host's seeded single-root pattern reader (fgdb-2acff).
/// The reader may decline a shape with `None`, keeping the unchanged leaf.
pub(crate) struct WithSeededPatterns<S, F> {
    pub(crate) source: S,
    pub(crate) seeded: F,
}

impl<E, C, S, F> GraphSetSource<E, C> for WithSeededPatterns<S, F>
where
    S: GraphSetSource<E, C>,
    F: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        &[VId],
        GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>>,
{
    forward_metadata_to_source!();
    fn supports_procedure_rows(&self) -> bool {
        self.source.supports_procedure_rows()
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn supports_pattern_factors(&self) -> bool {
        self.source.supports_pattern_factors()
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<E, C>>> {
        self.source.pattern_factors(request, policy)
    }
    fn supports_seeded_patterns(&self) -> bool {
        true
    }
    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        (self.seeded)(pattern, seeds, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        self.source.procedure_rows(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.source.owned()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        self.source.element_property(element, key)
    }
}

/// A source plus the rows of the previous write of the same statement, for a
/// chained write's input (`CREATE (a) WITH a CREATE (b)`): its relation has
/// one `SetNode::Owned` leaf, and those rows are already charged.
/// The source is a trait object, as the type-erased collectors hold it.
pub(crate) struct WithOwnedRows<'s, E, C> {
    pub(crate) source: &'s mut dyn GraphSetSource<E, C>,
    pub(crate) rows: Option<Vec<GraphValueRow>>,
}

impl<E, C> GraphSetSource<E, C> for WithOwnedRows<'_, E, C> {
    forward_metadata_to_source!();
    fn supports_procedure_rows(&self) -> bool {
        self.source.supports_procedure_rows()
    }
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self.source.pattern(pattern, policy)
    }
    fn supports_seeded_patterns(&self) -> bool {
        self.source.supports_seeded_patterns()
    }
    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: &[VId],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.seeded_pattern(pattern, seeds, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        self.source.procedure(call, arguments, policy)
    }
    fn procedure_rows(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphProcedureExecution<E, C>, GqlQueryError<E, C>>> {
        self.source.procedure_rows(call, arguments, policy)
    }
    fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
        self.rows.take()
    }
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        self.source.element_property(element, key)
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
