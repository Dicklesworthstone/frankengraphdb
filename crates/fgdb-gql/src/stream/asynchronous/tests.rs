use super::*;
use crate::algebra::{GraphValue, GraphValueRow};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

#[derive(Clone)]
struct Sample {
    vid: VId,
    visible: bool,
    labels: Vec<LabelId>,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}

#[derive(Default)]
struct Counter(AtomicUsize);

impl Counter {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn set(&self, value: usize) {
        self.0.store(value, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Flag(AtomicBool);

impl Flag {
    fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn set(&self, value: bool) {
        self.0.store(value, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Signals {
    reads: Counter,
    dropped: Flag,
    reservations: Counter,
    live_outputs: Counter,
    evaluation_events: Counter,
    live_evaluation: Counter,
}

struct Record {
    sample: Sample,
    signals: Arc<Signals>,
    scratch: Cell<usize>,
}

impl AsyncVertexScanRecord for Record {
    fn as_row(&self) -> VertexScanRow<'_> {
        VertexScanRow {
            labels: &self.sample.labels,
            properties: &self.sample.properties,
        }
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        self.signals
            .live_evaluation
            .set(self.signals.live_evaluation.get() - self.scratch.get());
    }
}

struct OutputGuard(Arc<Signals>);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        self.0.live_outputs.set(self.0.live_outputs.get() - 1);
    }
}

struct Source {
    rows: Vec<Sample>,
    next: usize,
    signals: Arc<Signals>,
    fail_at: Option<usize>,
    pending_at: Option<usize>,
    refuse_output: bool,
    refuse_evaluation: Option<usize>,
    admission: Admission,
}

#[derive(Clone, Copy)]
enum Admission {
    Normal,
    Missing,
    Repeated,
    Different,
    Eof,
}

impl VertexScanSource for Source {
    type Error = &'static str;

    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.next) {
            return Err(VertexScanSourceError::Source("source failed"));
        }
        let result = self.rows.get(self.next).map(|row| row.vid);
        if result.is_some() {
            self.next += 1;
        }
        Ok(result)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.signals.reads.set(self.signals.reads.get() + 1);
        let row = &self.rows[self.next - 1];
        assert_eq!(vid, row.vid);
        Ok(row.visible.then_some(VertexScanRow {
            labels: &row.labels,
            properties: &row.properties,
        }))
    }
}

impl AsyncVertexScanSource for Source {
    type Error = &'static str;
    type Record = Record;
    type OutputGuard = OutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        VertexScanSource::snapshot_seq(self)
    }

    async fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Record>>, VertexScanSourceError<Self::Error, C>> {
        if self.pending_at == Some(self.next) {
            control(AsyncVertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            std::future::pending::<()>().await;
        }
        let map = |event| match event {
            VertexScanEvent::Work => AsyncVertexScanEvent::Work,
            VertexScanEvent::ScratchEntry => AsyncVertexScanEvent::ScratchEntry,
        };
        let Some(vid) = self.next_vertex(&mut |event| control(map(event)))? else {
            return Ok(None);
        };
        match self.admission {
            Admission::Normal => control(AsyncVertexScanEvent::Candidate(vid)),
            Admission::Missing => Ok(()),
            Admission::Repeated => control(AsyncVertexScanEvent::Candidate(vid))
                .and_then(|()| control(AsyncVertexScanEvent::Candidate(vid))),
            Admission::Different => control(AsyncVertexScanEvent::Candidate(VId(vid.0 + 1))),
            Admission::Eof => {
                control(AsyncVertexScanEvent::Candidate(vid))
                    .map_err(VertexScanSourceError::Control)?;
                return Ok(None);
            }
        }
        .map_err(VertexScanSourceError::Control)?;
        let visible = self
            .vertex(vid, &mut |event| control(map(event)))?
            .is_some();
        Ok(Some(AsyncVertexCandidate {
            vid,
            record: visible.then(|| Record {
                sample: self.rows[self.next - 1].clone(),
                signals: Arc::clone(&self.signals),
                scratch: Cell::new(0),
            }),
        }))
    }

    fn reserve_output<C>(
        &self,
        _: &Record,
        columns: usize,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<OutputGuard, VertexScanSourceError<Self::Error, C>> {
        assert!((1..=3).contains(&columns));
        if self.refuse_output {
            return Err(VertexScanSourceError::Source("output memory refused"));
        }
        self.signals
            .reservations
            .set(self.signals.reservations.get() + 1);
        self.signals
            .live_outputs
            .set(self.signals.live_outputs.get() + 1);
        Ok(OutputGuard(Arc::clone(&self.signals)))
    }

    fn evaluation_event(&self, record: &Record, event: VertexScanEvent) -> Result<(), Self::Error> {
        if event == VertexScanEvent::ScratchEntry {
            let count = self.signals.evaluation_events.get() + 1;
            self.signals.evaluation_events.set(count);
            if self.refuse_evaluation == Some(count) {
                return Err("evaluation memory refused");
            }
            record.scratch.set(record.scratch.get() + 1);
            self.signals
                .live_evaluation
                .set(self.signals.live_evaluation.get() + 1);
        }
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.signals.dropped.set(true);
    }
}

fn source(rows: Vec<Sample>) -> Source {
    Source {
        rows,
        next: 0,
        signals: Arc::new(Signals::default()),
        fail_at: None,
        pending_at: None,
        refuse_output: false,
        refuse_evaluation: None,
        admission: Admission::Normal,
    }
}

fn sample(vid: u128, value: Option<CanonicalScalar>, visible: bool) -> Sample {
    Sample {
        vid: VId(vid),
        visible,
        labels: vec![LabelId(1)],
        properties: value
            .into_iter()
            .map(|value| (PropertyKeyId(1), value))
            .collect(),
    }
}

fn plan(text: &str) -> GlaPlan<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => {
            Some(GraphSymbol::Relation(fgdb_delta_types::RelationId(1)))
        }
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .plan()
    .clone()
}

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

// The fixture source is immediately ready except the explicit cancellation
// law. Poll once instead of introducing another runtime or busy-wait executor.
fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture unexpectedly suspended"),
    }
}

#[test]
fn async_rows_share_native_semantics_counters_and_windows_with_the_sync_operator() {
    let rows = vec![
        sample(0, None, true),
        sample(1, Some(CanonicalScalar::Int(100)), false),
        sample(2, Some(CanonicalScalar::Int(2)), true),
        sample(3, Some(CanonicalScalar::Int(3)), true),
        sample(u128::MAX, Some(CanonicalScalar::Int(4)), true),
    ];
    for (text, expected) in [
        (
            "MATCH (n:L) WHERE n.p >= 2 RETURN n AS id, n.p AS p",
            vec![VId(2), VId(3), VId(u128::MAX)],
        ),
        (
            "MATCH (n) WHERE n.p * 2 >= 4 RETURN n AS id, n.p AS p SKIP 1 LIMIT 1",
            vec![VId(3)],
        ),
        (
            "MATCH (n) WHERE n.p IS NULL RETURN n AS id, n.p AS p",
            vec![VId(0)],
        ),
        (
            "MATCH (n) RETURN DISTINCT n AS id, n.p AS p LIMIT 0",
            vec![],
        ),
    ] {
        let logical = plan(text);
        let mut sync = VertexScanCursor::new(
            source(rows.clone()),
            VertexScanPlan::compile(&logical).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        let expected_rows = sync.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let input = source(rows.clone());
        let signals = Arc::clone(&input.signals);
        let mut cursor = AsyncVertexScanCursor::new(
            input,
            AsyncVertexScanPlan::compile(&logical).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(signals.reads.get(), 0, "construction must not read");
        let mut actual = Vec::new();
        while let Some(row) = ready(cursor.next()) {
            let (row, guard) = row.unwrap().into_parts();
            actual.push(row);
            drop(guard);
            assert_eq!(signals.live_evaluation.get(), 0);
        }
        assert_eq!(actual, expected_rows);
        assert_eq!(
            actual
                .iter()
                .map(|row| row.values()[0].clone())
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(GraphValue::Vertex)
                .collect::<Vec<_>>()
        );
        assert_eq!(cursor.row_stats(), sync.row_stats());
        assert_eq!(cursor.evaluator_stats(), sync.evaluator_stats());
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(signals.dropped.get());
        assert_eq!(signals.live_outputs.get(), 0);
    }
}

#[test]
fn outputs_keep_their_guard_after_cursor_drop_and_limit_never_prefetches() {
    let mut input = source(vec![
        sample(1, Some(CanonicalScalar::Int(7)), true),
        sample(2, Some(CanonicalScalar::Int(8)), true),
    ]);
    input.fail_at = Some(1);
    let signals = Arc::clone(&input.signals);
    let mut cursor = AsyncVertexScanCursor::new(
        input,
        AsyncVertexScanPlan::compile(&plan("MATCH (n) RETURN n, n.p LIMIT 1")).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    let row = ready(cursor.next()).unwrap().unwrap();
    assert_eq!(row.values()[0], GraphValue::Vertex(VId(1)));
    assert_eq!(cursor.state(), VertexScanState::Exhausted);
    assert_eq!(signals.reads.get(), 1);
    assert!(signals.dropped.get());
    assert_eq!(signals.live_outputs.get(), 1);
    assert!(ready(cursor.next()).is_none());
    drop(cursor);
    assert_eq!(signals.live_outputs.get(), 1);
    drop(row);
    assert_eq!(signals.live_outputs.get(), 0);
}

#[test]
fn candidate_allowance_precedes_payload_reads_and_source_admission_must_match_identity() {
    let compiled = AsyncVertexScanPlan::compile(&plan("MATCH (n) RETURN n")).unwrap();
    let input = source(vec![sample(1, Some(CanonicalScalar::Int(1)), true)]);
    let signals = Arc::clone(&input.signals);
    let mut cursor = AsyncVertexScanCursor::new(
        input,
        compiled.clone(),
        GqlQueryPolicy::new(0, 10, 1000, 1000),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        ready(cursor.next()),
        Some(Err(GqlQueryError::Rows(_)))
    ));
    assert_eq!(signals.reads.get(), 0);
    assert_eq!(signals.reservations.get(), 0);
    assert_eq!(cursor.row_stats().snapshot_records, 0);
    assert!(signals.dropped.get());

    for admission in [
        Admission::Missing,
        Admission::Repeated,
        Admission::Different,
        Admission::Eof,
    ] {
        let mut input = source(vec![sample(1, None, false)]);
        input.admission = admission;
        let signals = Arc::clone(&input.signals);
        let mut cursor =
            AsyncVertexScanCursor::new(input, compiled.clone(), wide(), || Ok::<_, ()>(()));
        assert!(matches!(
            ready(cursor.next()),
            Some(Err(GqlQueryError::Source(
                VertexScanError::InvalidCandidateAdmission
            )))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(signals.reservations.get(), 0);
        assert!(signals.dropped.get());
        assert!(ready(cursor.next()).is_none());
    }
}

#[test]
fn output_reservation_and_each_evaluator_allocation_refuse_before_delivery() {
    let value = CanonicalScalar::ucs_basic_text(&"payload".repeat(100)).unwrap();
    let logical = plan("MATCH (n) WHERE size(n.p) > 0 RETURN n, n.p");
    let compiled = AsyncVertexScanPlan::compile(&logical).unwrap();
    let input = source(vec![sample(1, Some(value.clone()), true)]);
    let signals = Arc::clone(&input.signals);
    let mut baseline =
        AsyncVertexScanCursor::new(input, compiled.clone(), wide(), || Ok::<_, ()>(()));
    let row = ready(baseline.next()).unwrap().unwrap();
    let allocations = signals.evaluation_events.get();
    assert!(allocations > value.encode().unwrap().len() / 64);
    drop(row);
    for refusal in 1..=allocations {
        let mut input = source(vec![sample(1, Some(value.clone()), true)]);
        input.refuse_evaluation = Some(refusal);
        let signals = Arc::clone(&input.signals);
        let mut cursor =
            AsyncVertexScanCursor::new(input, compiled.clone(), wide(), || Ok::<_, ()>(()));
        assert!(matches!(
            ready(cursor.next()),
            Some(Err(GqlQueryError::Source(VertexScanError::Source(
                "evaluation memory refused"
            ))))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(signals.live_evaluation.get(), 0);
        assert_eq!(signals.live_outputs.get(), 0);
        assert!(signals.dropped.get());
        assert!(ready(cursor.next()).is_none());
    }
    let mut input = source(vec![sample(1, Some(value), true)]);
    input.refuse_output = true;
    let signals = Arc::clone(&input.signals);
    let mut cursor = AsyncVertexScanCursor::new(input, compiled, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        ready(cursor.next()),
        Some(Err(GqlQueryError::Source(VertexScanError::Source(
            "output memory refused"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(signals.live_evaluation.get(), 0);
    assert_eq!(signals.live_outputs.get(), 0);
    assert!(signals.dropped.get());
}

#[test]
fn every_checkpoint_is_terminal_and_budget_is_shared_across_pulls() {
    let compiled = AsyncVertexScanPlan::compile(&plan("MATCH (n) RETURN n, n.p")).unwrap();
    let rows = vec![
        sample(1, Some(CanonicalScalar::Int(1)), true),
        sample(2, Some(CanonicalScalar::Int(2)), true),
    ];
    let calls = Counter::default();
    let mut baseline =
        AsyncVertexScanCursor::new(source(rows.clone()), compiled.clone(), wide(), || {
            calls.set(calls.get() + 1);
            Ok::<_, usize>(())
        });
    while let Some(row) = ready(baseline.next()) {
        drop(row.unwrap());
    }
    let stats = baseline.evaluator_stats();
    for stop in 1..=calls.get() {
        let input = source(rows.clone());
        let signals = Arc::clone(&input.signals);
        let mut at = 0;
        let mut cursor = AsyncVertexScanCursor::new(input, compiled.clone(), wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        loop {
            match ready(cursor.next()).expect("chosen checkpoint is reachable") {
                Ok(row) => drop(row),
                Err(GqlQueryError::Interrupted(actual)) => {
                    assert_eq!(actual, stop);
                    break;
                }
                Err(error) => panic!("unexpected {error:?}"),
            }
        }
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(signals.dropped.get());
        assert_eq!(signals.live_outputs.get(), 0);
        assert_eq!(signals.live_evaluation.get(), 0);
        assert!(ready(cursor.next()).is_none());
    }
    for (work, scratch, succeeds) in [
        (stats.work_units, stats.scratch_entries, true),
        (stats.work_units - 1, stats.scratch_entries, false),
        (stats.work_units, stats.scratch_entries - 1, false),
    ] {
        let mut cursor = AsyncVertexScanCursor::new(
            source(rows.clone()),
            compiled.clone(),
            GqlQueryPolicy::new(2, 2, work, scratch),
            || Ok::<_, ()>(()),
        );
        let mut complete = true;
        while let Some(row) = ready(cursor.next()) {
            if row.is_err() {
                complete = false;
            }
        }
        assert_eq!(complete, succeeds);
    }
}

#[test]
fn dropping_an_inflight_pull_fences_its_partially_consumed_source() {
    let mut input = source(vec![sample(1, Some(CanonicalScalar::Int(1)), true)]);
    input.pending_at = Some(0);
    let signals = Arc::clone(&input.signals);
    let mut cursor = AsyncVertexScanCursor::new(
        input,
        AsyncVertexScanPlan::compile(&plan("MATCH (n) RETURN n")).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    let mut pull = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&pull);
    assert!(
        pull.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(pull);
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(signals.dropped.get());
    assert!(ready(cursor.next()).is_none());
    cursor.close();
    assert_eq!(cursor.state(), VertexScanState::Failed);
}

#[test]
fn source_errors_bad_identity_and_unsupported_profiles_never_become_empty_success() {
    for repeated in [false, true] {
        let mut input = source(vec![
            sample(1, Some(CanonicalScalar::Int(1)), true),
            sample(
                if repeated { 1 } else { 2 },
                Some(CanonicalScalar::Int(2)),
                true,
            ),
        ]);
        if !repeated {
            input.fail_at = Some(1);
        }
        let signals = Arc::clone(&input.signals);
        let mut cursor = AsyncVertexScanCursor::new(
            input,
            AsyncVertexScanPlan::compile(&plan("MATCH (n) RETURN n")).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        drop(ready(cursor.next()).unwrap().unwrap());
        let error = ready(cursor.next()).unwrap().unwrap_err();
        if repeated {
            assert!(matches!(
                error,
                GqlQueryError::Source(VertexScanError::NonIncreasingIdentity)
            ));
        } else {
            assert!(matches!(
                error,
                GqlQueryError::Source(VertexScanError::Source("source failed"))
            ));
        }
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(signals.dropped.get());
        assert!(ready(cursor.next()).is_none());
    }
    for text in [
        "MATCH (n) RETURN n.p",
        "MATCH (n) RETURN n, n.p ORDER BY n.p",
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
    ] {
        assert!(AsyncVertexScanPlan::compile(&plan(text)).is_err(), "{text}");
    }
}
