//! Actual pull scans over a pinned, already decoded database generation.
//!
//! Persistent property candidates or the history index supply ordered successors.
//! Equality keeps at most two roots. Numeric ranges merge at most 64 value-bucket
//! heads; wider ranges test historical membership one identity at a time. Neither
//! path collects all candidate IDs or resolves unrequested MVCC winners.
//! The source holds an EmbeddedReadView, not a borrow
//! of the mutable Database: writes, compaction, reopen and handle drop cannot
//! change its generation. This is not paging an evidence artifact or materialized
//! graph table, and it does not claim that the pinned generation is out of core.

use crate::gql_exec::source::{IndexMap, SourceEvent, numeric};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{
    GlaPlan, GraphValueRow, IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::edge_stream::{EdgeExpansionSourceError, EdgeScanRow};
use fgdb_gql::scan_stream::SourceStep;
use fgdb_gql::stream::{
    VertexScanCursor, VertexScanError, VertexScanEvent, VertexScanOutput, VertexScanPlan,
    VertexScanRow, VertexScanSource, VertexScanSourceError,
};
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GqlQueryPolicy, PreparedGqlQuery};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, QueryCx, VId};
use std::sync::Arc;

mod global_aggregate;
mod prefix;
mod range;

type Cancel = Box<asupersync::error::Error>;
type StreamError = GqlQueryError<VertexScanError<ReadError>, Cancel>;

type EqualityKey = (PropertyKeyId, Arc<[u8]>);

struct RootEquality {
    key: PropertyKeyId,
    value: CanonicalScalar,
}

enum RootCandidates {
    Histories,
    // Resolve keys on first demand, never while opening a cursor. Numeric
    // equality has at most one exact key per canonical numeric domain.
    Pending {
        keys: [Option<EqualityKey>; 2],
        equality: RootEquality,
    },
    Indexed {
        roots: [Option<IndexMap<VId, ()>>; 2],
        equality: RootEquality,
    },
    // Boxed: a range cursor is several times the size of every other arm.
    Range(Box<range::RangeCandidates>),
    Prefix(prefix::PrefixCandidates),
}

/// Public only because it is a parameter of the returned public cursor. The
/// host alone constructs it from an admitted view and an exact retained cut.
/// No source mutation, raw-snapshot constructor or unchecked cursor import is
/// exposed. The caller normally names the enclosing VertexScanCursor<_, _>.
pub struct SnapshotVertexSource<'q> {
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    as_of: CommitSeq,
    after: Option<VId>,
    candidates: RootCandidates,
}

impl SnapshotVertexSource<'_> {
    /// Prefer an admitted leading equality, then numeric constraints on one
    /// allowed property, then a safe leading prefix. Later fallible predicates
    /// retain their ordinary evaluation order.
    pub(crate) fn with_root_equality<Row: VertexScanOutput>(
        self,
        plan: &VertexScanPlan<Row>,
        allows_property: impl Fn(PropertyKeyId) -> bool,
    ) -> Self {
        self.with_root_equality_predicates(plan.leading_root_predicates(), &allows_property)
            .with_root_prefix(plan.leading_root_prefix(), allows_property)
    }

    pub(crate) fn with_root_prefix(
        mut self,
        prefix: Option<(PropertyKeyId, &str)>,
        allows_property: impl Fn(PropertyKeyId) -> bool,
    ) -> Self {
        if matches!(self.candidates, RootCandidates::Histories)
            && let Some((key, prefix)) = prefix
            && allows_property(key)
            && let Some(prefix) = prefix::PrefixCandidates::new(key, prefix)
        {
            self.candidates = RootCandidates::Prefix(prefix);
        }
        self
    }

    /// Aggregate inputs expose the same admitted prefix without exporting a
    /// relaxed row plan that could bypass the ordinary cursor's ordering proof.
    pub(crate) fn with_root_equality_predicates<'p>(
        mut self,
        predicates: impl Iterator<Item = &'p VertexPredicate>,
        allows_property: impl Fn(PropertyKeyId) -> bool,
    ) -> Self {
        let mut equality = None;
        let mut bounds: Option<numeric::NumericBounds> = None;
        for predicate in predicates {
            let selected = match predicate {
                VertexPredicate::IntegerProperty {
                    key,
                    comparison: IntegerComparison::Equal,
                    value,
                } => Some((*key, CanonicalScalar::Int(*value))),
                VertexPredicate::ScalarProperty { key, predicate }
                    if predicate.comparison() == IntegerComparison::Equal
                        && !matches!(predicate.value(), CanonicalScalar::Null) =>
                {
                    Some((*key, predicate.value().clone()))
                }
                _ => None,
            };
            if let Some((key, value)) = selected
                && allows_property(key)
            {
                equality = Some((key, value));
                break;
            }
            if let Some((key, comparison, value)) = numeric::numeric_operand(predicate)
                && allows_property(key)
            {
                let selected = bounds.get_or_insert_with(|| numeric::NumericBounds::new(key));
                if selected.key() == key {
                    selected.constrain(&value, comparison);
                }
            }
        }
        let Some((key, value)) = equality else {
            if let Some(bounds) = bounds {
                self.candidates =
                    RootCandidates::Range(Box::new(range::RangeCandidates::new(bounds)));
            }
            return self;
        };
        let values = match &value {
            CanonicalScalar::Int(_) | CanonicalScalar::Float(_) => numeric::equality_peers(&value),
            scalar => [Some(scalar.clone()), None],
        };
        let mut keys = [None, None];
        for (target, value) in keys.iter_mut().zip(values) {
            if let Some(value) = value {
                let Ok(encoded) = value.encode() else {
                    return self;
                };
                *target = Some((key, Arc::from(encoded)));
            }
        }
        self.candidates = RootCandidates::Pending {
            keys,
            equality: RootEquality { key, value },
        };
        self
    }
}

impl VertexScanSource for SnapshotVertexSource<'_> {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<ReadError, C>> {
        loop {
            match self.next_vertex_step(control)? {
                SourceStep::Item(vid) => return Ok(Some(vid)),
                SourceStep::Skipped(_) => {}
                SourceStep::Exhausted => return Ok(None),
            }
        }
    }

    fn next_vertex_step<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<SourceStep<VId>, VertexScanSourceError<ReadError, C>> {
        let step = self
            .cx
            .with_restriction(|| {
                if let RootCandidates::Pending { keys, .. } = &self.candidates {
                    let mut roots = [None, None];
                    for (target, key) in roots.iter_mut().zip(keys) {
                        let Some(key) = key else { continue };
                        let mut node = self.view.snapshot.property_index().candidates.0.as_deref();
                        while let Some(current) = node {
                            control(VertexScanEvent::Work)?;
                            match key.cmp(&current.key) {
                                core::cmp::Ordering::Less => node = current.left.0.as_deref(),
                                core::cmp::Ordering::Greater => node = current.right.0.as_deref(),
                                core::cmp::Ordering::Equal => {
                                    *target = Some(current.value.clone());
                                    break;
                                }
                            }
                        }
                    }
                    // Move the selected predicate only after every controlled
                    // lookup succeeds. A refusal leaves the source unadvanced.
                    let RootCandidates::Pending { equality, .. } =
                        core::mem::replace(&mut self.candidates, RootCandidates::Histories)
                    else {
                        unreachable!("candidate roots resolved from pending keys")
                    };
                    self.candidates = RootCandidates::Indexed { roots, equality };
                }
                let successor = match &mut self.candidates {
                    RootCandidates::Indexed { roots, .. } => {
                        let mut successor = None;
                        for root in roots.iter().flatten() {
                            if let Some(vid) = index_successor(root, self.after, control)? {
                                successor = Some(successor.map_or(vid, |next: VId| next.min(vid)));
                            }
                        }
                        successor
                    }
                    RootCandidates::Histories => index_successor(
                        &self.view.snapshot.property_index().histories,
                        self.after,
                        control,
                    )?,
                    RootCandidates::Range(range) => {
                        return range.next(
                            self.view.snapshot.property_index(),
                            &self.view.snapshot.patches,
                            self.after,
                            control,
                        );
                    }
                    RootCandidates::Prefix(prefix) => {
                        return prefix.next(
                            self.view.snapshot.property_index(),
                            self.after,
                            control,
                        );
                    }
                    RootCandidates::Pending { .. } => {
                        unreachable!("candidate roots resolved above")
                    }
                };
                Ok(successor.map_or(SourceStep::Exhausted, SourceStep::Item))
            })
            .map_err(VertexScanSourceError::Control)?;
        if let SourceStep::Item(vid) | SourceStep::Skipped(vid) = step {
            self.after = Some(vid);
        }
        Ok(step)
    }

    fn root_is_candidate<C>(
        &self,
        row: &VertexScanRow<'_>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<bool, VertexScanSourceError<ReadError, C>> {
        let key = match &self.candidates {
            RootCandidates::Histories => return Ok(true),
            RootCandidates::Pending { equality, .. } | RootCandidates::Indexed { equality, .. } => {
                equality.key
            }
            RootCandidates::Range(range) => range.key(),
            RootCandidates::Prefix(prefix) => prefix.key(),
        };
        // This poll-only recheck is for the scoped adapter's private candidate
        // admission. The native cursor later meters its ordinary full predicate
        // evaluation; the unscoped source still counts every examined history.
        let mut low = 0;
        let mut high = row.properties.len();
        while low < high {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let mid = low + (high - low) / 2;
            match row.properties[mid].0.cmp(&key) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => {
                    control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
                    // A stored List or Map never meets a scalar root bound.
                    let value = row.properties[mid].1.as_scalar();
                    return Ok(match &self.candidates {
                        RootCandidates::Pending { equality, .. }
                        | RootCandidates::Indexed { equality, .. } => IntegerComparison::Equal
                            .accepts_scalar_pair(value, Some(&equality.value)),
                        RootCandidates::Range(range) => {
                            value.is_some_and(|value| range.accepts(value))
                        }
                        RootCandidates::Prefix(prefix) => {
                            value.is_some_and(|value| prefix.accepts(value))
                        }
                        RootCandidates::Histories => unreachable!("history roots admit every row"),
                    });
                }
            }
        }
        Ok(false)
    }

    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<ReadError, C>> {
        probe_vertex_from_view(&self.view, self.cx, after, control)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<ReadError, C>> {
        self.cx.with_restriction(|| {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let snapshot = &self.view.snapshot;
            snapshot
                .property_index()
                .visible_row(&snapshot.patches, vid, self.as_of, &mut |event| {
                    control(match event {
                        // The cursor charges one candidate history BEFORE this
                        // lookup. Version work is not a second candidate record.
                        SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
                        SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                    })
                })
                .map(|row| {
                    row.map(|row| VertexScanRow {
                        labels: &row.labels,
                        properties: &row.props,
                    })
                })
                .map_err(VertexScanSourceError::Control)
        })
    }

    fn next_probe_edge<C>(
        &self,
        endpoint: VId,
        direction: fgdb_gql::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<ReadError, C>> {
        super::edge_stream::next_from_view(&self.view, self.cx, endpoint, direction, after, control)
    }

    fn probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<ReadError, C>> {
        super::edge_stream::edge_from_view(&self.view, self.cx, self.as_of, eid, control)
            .map_err(EdgeExpansionSourceError::Read)
    }
}
impl core::fmt::Debug for SnapshotVertexSource<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SnapshotVertexSource")
            .field("as_of", &self.as_of)
            .field("generation_and_position", &"[REDACTED]")
            .finish()
    }
}

// Strict successor in the already admitted persistent vertex history index.
// This body is shared by the root and every independent probe position. It
// does not advance a cursor, pre-resolve visibility, clone a pin or collect IDs.
fn vertex_successor<C>(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    after: Option<VId>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<Option<VId>, C> {
    cx.with_restriction(|| {
        index_successor(&view.snapshot.property_index().histories, after, control)
    })
}

fn index_successor<T, C>(
    index: &IndexMap<VId, T>,
    after: Option<VId>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<Option<VId>, C> {
    let mut node = index.0.as_deref();
    let mut successor = None;
    // No +1 sentinel: VId(0) and VId(u128::MAX) are ordinary identities.
    while let Some(current) = node {
        control(VertexScanEvent::Work)?;
        if after.is_none_or(|after| current.key > after) {
            successor = Some(current.key);
            node = current.left.0.as_deref();
        } else {
            node = current.right.0.as_deref();
        }
    }
    Ok(successor)
}

// Both outer source kinds reuse the same successor and own exactly one pin.
// The kernel charges a candidate record before resolving its visible vertex.
pub(super) fn probe_vertex_from_view<C>(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    after: Option<VId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<VId>, EdgeExpansionSourceError<ReadError, C>> {
    vertex_successor(view, cx, after, &mut |event| {
        control(match event {
            VertexScanEvent::Work => GlaExecutionEvent::Work,
            VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        })
    })
    .map_err(|error| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(error)))
}

fn source_error(error: ReadError) -> StreamError {
    GqlQueryError::Source(VertexScanError::Source(error))
}
fn open<'q, Row: VertexScanOutput>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    logical: &GlaPlan<Row>,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    VertexScanCursor<
        SnapshotVertexSource<'q>,
        impl FnMut() -> Result<(), Cancel> + 'q + use<'q, Row>,
        Row,
    >,
    StreamError,
> {
    view.snapshot.check_frontier(as_of).map_err(source_error)?;
    let plan = VertexScanPlan::compile(logical)
        .map_err(|error| GqlQueryError::Source(VertexScanError::Plan(error)))?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)?;
    let source = SnapshotVertexSource {
        view,
        cx,
        as_of,
        after: None,
        candidates: RootCandidates::Histories,
    }
    .with_root_equality(&plan, |_| true);
    Ok(VertexScanCursor::new(source, plan, policy, move || {
        cx.with_restriction(|| cx.checkpoint())
    }))
}

impl<V: Vfs + Clone> Database<V> {
    /// Pull correlated property rows in canonical order without a result set.
    /// The first projected column must be the scanned VId; remaining columns
    /// may repeat that identity or read its canonical properties. The leading
    /// unique identity proves whole-row order and DISTINCT without sorting.
    /// Other projections/orderings refuse, rather than quietly changing order.
    /// Correlated/independent fixed-hop EXISTS/NOT EXISTS share the pinned
    /// vertex, incidence and edge-version readers. Isolates are candidates;
    /// every root and probe vertex/edge examination uses the same allowance.
    /// The stream keeps one view and original QueryCx, not a new graph per root.
    pub fn stream_graph_values_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
            GraphValueRow,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_graph_values_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
            GraphValueRow,
        >,
        StreamError,
    > {
        open(
            self.read_session().map_err(source_error)?,
            cx,
            pattern.plan(),
            as_of,
            policy,
        )
    }

    /// Stream the supported single-vertex identity GLA profile. Opening pins
    /// one immutable generation but scans no candidate and builds no row set.
    /// next() is the demand signal; close/drop never drains the unused suffix.
    /// The cursor owns its read view, so the database remains writable while
    /// it is alive. It borrows only QueryCx, not this handle or the definition.
    pub fn stream_graph_vertices_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern.plan(), as_of, policy)
    }

    /// Health and exact-sequence admission precedes cursor preparation. A future
    /// cut never becomes a successful empty stream, including under LIMIT 0.
    pub fn stream_graph_vertices_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        open(
            self.read_session().map_err(source_error)?,
            cx,
            pattern.plan(),
            as_of,
            policy,
        )
    }

    /// Owned legacy preparation enters the same GLA scan compiler. There is
    /// no reparse, certificate paging, full-table admission or eager fallback.
    pub fn stream_prepared_query_governed<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, &GlaPlan::lower(query.plan()), as_of, policy)
    }

    pub fn stream_prepared_query_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        view.snapshot.check_frontier(as_of).map_err(source_error)?;
        open(view, cx, &GlaPlan::lower(query.plan()), as_of, policy)
    }
}

impl EmbeddedReadView {
    // Reuse the real ordered-history source behind a trusted source adapter.
    // This only clones a pin after exact-cut admission: no candidate histories,
    // graph rows or query results are scanned or collected at construction.
    pub(crate) fn vertex_scan_source<'q>(
        &self,
        cx: &'q QueryCx,
        as_of: CommitSeq,
    ) -> Result<SnapshotVertexSource<'q>, ReadError> {
        self.snapshot.check_frontier(as_of)?;
        Ok(SnapshotVertexSource {
            view: self.clone(),
            cx,
            as_of,
            after: None,
            candidates: RootCandidates::Histories,
        })
    }

    pub fn stream_graph_values_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
            GraphValueRow,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), self.frontier(), policy)
    }

    pub fn stream_graph_values_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
            GraphValueRow,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_graph_vertices_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), self.frontier(), policy)
    }

    pub fn stream_graph_vertices_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_prepared_query_governed<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        StreamError,
    > {
        open(
            self.clone(),
            cx,
            &GlaPlan::lower(query.plan()),
            self.frontier(),
            policy,
        )
    }

    pub fn stream_prepared_query_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        StreamError,
    > {
        self.snapshot.check_frontier(as_of).map_err(source_error)?;
        open(
            self.clone(),
            cx,
            &GlaPlan::lower(query.plan()),
            as_of,
            policy,
        )
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod probe_tests;

#[cfg(test)]
mod independent_tests;
