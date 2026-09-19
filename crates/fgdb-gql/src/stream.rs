//! Pull execution for ordered, single-vertex GLA identity scans.
//!
//! This is an operator cursor, not paging over an already computed result.
//! Each pull advances only until the next accepted identity. The source owns
//! one immutable, admitted generation and yields candidate identities in
//! strictly increasing order. Historical candidates may have no visible row.
//! Only the existing Select predicates, identity tests, identity projection,
//! DISTINCT/ALL, canonical identity order and terminal SKIP/LIMIT are admitted.
//! Unsupported operators refuse before a source is driven; there is no eager
//! fallback, AST interpreter, alternate property semantics or storage model.
//!
//! Previous successful pulls remain delivered after a later error. The error
//! is emitted once, permanently fuses the cursor, and drops its source. Closing
//! or dropping provides backpressure without scanning the unused suffix. This
//! does not promise a server lease, restart token, spill, or byte-memory limit.

use crate::algebra::{GlaOperator, GlaPlan, VertexPredicate};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats,
    GqlQueryError, GqlQueryPolicy,
};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CanonicalScalar, CommitSeq, VId};
use std::iter::FusedIterator;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexScanBuildError {
    /// GLA instruction position, never source text or a graph identity.
    pub operator: usize,
}
impl core::fmt::Display for VertexScanBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "operator {} is outside the ordered vertex-stream profile", self.operator)
    }
}
impl core::error::Error for VertexScanBuildError {}

#[derive(Clone)]
enum Test {
    Predicate(VertexPredicate),
    Identity(bool),
}

/// Checked physical specialization of an existing GLA definition.
/// Preparation owns its predicates; polling neither reparses nor rebinds.
#[derive(Clone)]
pub struct VertexScanPlan {
    empty: bool,
    tests: Arc<[Test]>,
    offset: u64,
    count: Option<u64>,
}
impl VertexScanPlan {
    pub fn compile(plan: &GlaPlan) -> Result<Self, VertexScanBuildError> {
        let operators = plan.operators();
        let empty = match operators.first() {
            Some(GlaOperator::ScanVertices) => false,
            Some(GlaOperator::Empty) => true,
            _ => return Err(VertexScanBuildError { operator: 0 }),
        };
        let mut at = 1;
        let mut tests = Vec::new();
        loop {
            match operators.get(at) {
                Some(GlaOperator::Select { slot, predicates }) if slot.ordinal() == 0 => {
                    tests.extend(predicates.iter().cloned().map(Test::Predicate));
                }
                Some(GlaOperator::VertexIdentity { left, right, equal })
                    if left.ordinal() == 0 && right.ordinal() == 0 =>
                {
                    tests.push(Test::Identity(*equal));
                }
                Some(GlaOperator::Project { slot }) if slot.ordinal() == 0 => break,
                _ => return Err(VertexScanBuildError { operator: at }),
            }
            at += 1;
        }
        at += 1;
        if matches!(operators.get(at), Some(GlaOperator::Distinct)) { at += 1; }
        // A strictly increasing one-row-per-identity source already implements
        // both ALL and DISTINCT and exactly this order. Other orders need a
        // different physical operator and cannot silently use this cursor.
        if !matches!(operators.get(at), Some(GlaOperator::OrderByVertexId)) {
            return Err(VertexScanBuildError { operator: at });
        }
        at += 1;
        let Some(GlaOperator::Limit { offset, count }) = operators.get(at) else {
            return Err(VertexScanBuildError { operator: at });
        };
        if at + 1 != operators.len() {
            return Err(VertexScanBuildError { operator: at + 1 });
        }
        Ok(Self { empty, tests: tests.into(), offset: *offset, count: *count })
    }

    fn accepts<E>(
        &self,
        row: VertexScanRow<'_>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        for test in self.tests.iter() {
            control(VertexScanEvent::Work)?;
            match test {
                Test::Identity(false) => return Ok(false),
                Test::Identity(true) => {}
                Test::Predicate(predicate) => {
                    for _ in 0..predicate.comparison_work_units() {
                        control(VertexScanEvent::Work)?;
                    }
                    // Govern each field lookup, then use the SAME canonical
                    // predicate evaluator as the ordinary GLA execution path.
                    let (label, property) = match predicate {
                        VertexPredicate::HasLabel(wanted) => {
                            let found = seek(row.labels, wanted, |label| *label, control)?;
                            (found.copied(), None)
                        }
                        _ => {
                            let key = predicate.property_key().expect("property predicate");
                            let found = seek(row.properties, &key, |entry| entry.0, control)?;
                            (None, found.map(|(key, value)| (*key, value)))
                        }
                    };
                    if !predicate.matches_borrowed(label, property) { return Ok(false); }
                }
            }
        }
        Ok(true)
    }
}
impl core::fmt::Debug for VertexScanPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexScanPlan([REDACTED])")
    }
}

fn seek<'a, T, K: Ord, E>(
    values: &'a [T], wanted: &K, key: impl Fn(&T) -> K,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
) -> Result<Option<&'a T>, E> {
    let (mut low, mut high) = (0, values.len());
    while low < high {
        control(VertexScanEvent::Work)?;
        let middle = low + (high - low) / 2;
        match key(&values[middle]).cmp(wanted) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Ok(Some(&values[middle])),
        }
    }
    Ok(None)
}

/// Borrowed fields from one already admitted visible vertex. Both slices are
/// canonically sorted with unique label/property keys. No payload is copied.
#[derive(Clone, Copy)]
pub struct VertexScanRow<'a> {
    pub labels: &'a [LabelId],
    pub properties: &'a [(PropertyKeyId, CanonicalScalar)],
}
impl core::fmt::Debug for VertexScanRow<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexScanRow([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexScanEvent { Work, ScratchEntry }

/// Source effects and caller controls remain distinct; neither becomes a
/// missing row. Implementations must propagate each refused control unchanged.
#[derive(Debug)]
pub enum VertexScanSourceError<E, C> { Source(E), Control(C) }

/// An immutable, admitted source. This trait is an execution seam, NOT an
/// authentication interface: implementing it does not confer graph authority.
/// next_vertex must yield every candidate once in increasing VId order; vertex
/// resolves only that identity at snapshot_seq, including absence/retirement.
/// Controls precede source work or scratch growth. No I/O may bypass its host's
/// purpose context, and unvalidated raw records must not enter this interface.
pub trait VertexScanSource {
    type Error;
    fn snapshot_seq(&self) -> CommitSeq;
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>>;
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexScanState { Open, Exhausted, Closed, Failed }

#[derive(Debug)]
pub enum VertexScanError<E> {
    Source(E),
    Plan(VertexScanBuildError),
    /// The source repeated or reversed an identity; no bad row is delivered.
    NonIncreasingIdentity,
    CounterExhausted,
}
impl<E: core::fmt::Display> core::fmt::Display for VertexScanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Plan(error) => error.fmt(f),
            Self::NonIncreasingIdentity => f.write_str("vertex stream source is not strictly increasing"),
            Self::CounterExhausted => f.write_str("vertex stream counter exhausted"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for VertexScanError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Plan(error) => Some(error),
            _ => None,
        }
    }
}

type ScanResult<T, E, C> = Result<T, GqlQueryError<VertexScanError<E>, C>>;

struct Meter<F> {
    checkpoint: F,
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}
impl<F> Meter<F> {
    fn event<E, C>(&mut self, event: VertexScanEvent) -> ScanResult<(), E, C>
    where F: FnMut() -> Result<(), C> {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        self.evaluator.charge_event(self.policy.evaluator, match event {
            VertexScanEvent::Work => GlaExecutionEvent::Work,
            VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        }).map_err(GqlQueryError::Evaluator)
    }
    fn record<E, C>(&mut self) -> ScanResult<(), E, C> {
        let count = self.rows.snapshot_records.checked_add(1)
            .ok_or(GqlQueryError::Source(VertexScanError::CounterExhausted))?;
        self.policy.rows.check(GqlBudgetDimension::SnapshotRecords, count)
            .map_err(GqlQueryError::Rows)?;
        self.rows.snapshot_records = count;
        Ok(())
    }
    fn emit<E, C>(&mut self) -> ScanResult<(), E, C>
    where F: FnMut() -> Result<(), C> {
        let count = self.rows.result_rows.checked_add(1)
            .ok_or(GqlQueryError::Source(VertexScanError::CounterExhausted))?;
        self.policy.rows.check(GqlBudgetDimension::ResultRows, count)
            .map_err(GqlQueryError::Rows)?;
        // Last fallible boundary before the caller receives this identity.
        self.event(VertexScanEvent::Work)?;
        self.rows.result_rows = count;
        Ok(())
    }
}

/// A single-owner pull cursor. Source and predicate work share one cumulative
/// allowance across ALL pulls; collecting another page cannot reset it.
/// SnapshotRecords counts examined candidate identity histories (even when
/// invisible at the cut), before their visible fields are resolved. ResultRows
/// counts emitted identities AFTER predicates and SKIP/LIMIT. This is not the
/// eager executor's complete-table admission count. Work/scratch are logical
/// controls, not allocator bytes, source residency, or caller collection space.
///
/// At most one borrowed row and one VId result are live in the cursor's pull
/// path; the source may retain an entire shared immutable database generation.
/// LIMIT exhaustion does not prefetch a later candidate. Natural EOF is known
/// on the first pull past the final result. An error is terminal, never EOF.
pub struct VertexScanCursor<S, F> {
    source: Option<S>,
    plan: VertexScanPlan,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    last: Option<VId>,
    skip: u64,
    state: VertexScanState,
}
impl<S: VertexScanSource, F> VertexScanCursor<S, F> {
    /// Pure construction: no source scan or checkpoint occurs until a pull.
    /// Database adapters admit ownership/frontier and QueryCx before this call.
    pub fn new(source: S, plan: VertexScanPlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            snapshot_seq: source.snapshot_seq(),
            skip: plan.offset,
            source: Some(source),
            plan,
            meter: Meter { checkpoint, policy, rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 }, evaluator: GlaExecutionStats::default() },
            last: None,
            state: VertexScanState::Open,
        }
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq { self.snapshot_seq }
    #[must_use]
    pub fn state(&self) -> VertexScanState { self.state }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats { self.meter.rows }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats { self.meter.evaluator }
    /// Idempotent close drops the pinned source without reading its suffix.
    /// Completed/failed state and all counters remain inspectable unchanged.
    pub fn close(&mut self) {
        if self.state == VertexScanState::Open { self.state = VertexScanState::Closed; }
        self.source = None;
    }

    fn advance<C>(&mut self) -> ScanResult<Option<VId>, S::Error, C>
    where F: FnMut() -> Result<(), C> {
        let meter = &mut self.meter;
        meter.event(VertexScanEvent::Work)?;
        if self.plan.empty || self.plan.count == Some(0) { return Ok(None); }
        let source = self.source.as_mut().expect("open cursor owns its source");
        loop {
            let next = flatten(source.next_vertex(&mut |event| meter.event(event)))?;
            let Some(vid) = next else { return Ok(None); };
            meter.event(VertexScanEvent::Work)?;
            if self.last.is_some_and(|last| vid <= last) {
                return Err(GqlQueryError::Source(VertexScanError::NonIncreasingIdentity));
            }
            self.last = Some(vid);
            meter.record()?;
            let row = flatten(source.vertex(vid, &mut |event| meter.event(event)))?;
            let Some(row) = row else { continue; };
            if !self.plan.accepts(row, &mut |event| meter.event(event))? { continue; }
            meter.event(VertexScanEvent::Work)?;
            if self.skip != 0 { self.skip -= 1; continue; }
            meter.emit()?;
            return Ok(Some(vid));
        }
    }
}
fn flatten<T, E, C>(
    result: Result<T, VertexScanSourceError<E, GqlQueryError<VertexScanError<E>, C>>>,
) -> ScanResult<T, E, C> {
    result.map_err(|error| match error {
        VertexScanSourceError::Source(error) => GqlQueryError::Source(VertexScanError::Source(error)),
        VertexScanSourceError::Control(error) => error,
    })
}
impl<S, F, C> Iterator for VertexScanCursor<S, F>
where S: VertexScanSource, F: FnMut() -> Result<(), C> {
    type Item = ScanResult<VId, S::Error, C>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != VertexScanState::Open { return None; }
        match self.advance() {
            Ok(Some(vid)) => {
                if self.plan.count == Some(self.meter.rows.result_rows) {
                    self.state = VertexScanState::Exhausted;
                    self.source = None;
                }
                Some(Ok(vid))
            }
            Ok(None) => {
                self.state = VertexScanState::Exhausted;
                self.source = None;
                None
            }
            Err(error) => {
                self.state = VertexScanState::Failed;
                self.source = None;
                Some(Err(error))
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        // An open cursor may emit an error, so LIMIT is not an item upper bound.
        (0, (self.state != VertexScanState::Open).then_some(0))
    }
}
impl<S, F, C> FusedIterator for VertexScanCursor<S, F>
where S: VertexScanSource, F: FnMut() -> Result<(), C> {}
impl<S, F> core::fmt::Debug for VertexScanCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VertexScanCursor")
            .field("snapshot_seq", &self.snapshot_seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("definition_and_source", &"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests;
