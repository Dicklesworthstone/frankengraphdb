//! The relation selected by the plan must reach source admission before an
//! incidence lookup. These fixtures model source effects, not authentication.
use super::*;
use crate::algebra::{GlaDirection, GraphValue, GraphValueRow};
use crate::edge_stream::{EdgeScanCursor, EdgeScanPlan};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::RelationId;
use std::cell::Cell;
use std::rc::Rc;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const EDGES: [(EId, VId, VId, RelationId); 2] = [
    (EId(0), VId(0), VId(1), R),
    (EId(1), VId(1), VId(2), S),
];
struct Source {
    after: Option<VId>,
    edge_after: Option<EId>,
    allow_s: bool,
    unavailable: bool,
    lookup: Rc<Cell<usize>>,
}
impl Source {
    fn new(allow_s: bool, unavailable: bool) -> Self {
        Self { after: None, edge_after: None, allow_s, unavailable, lookup: Rc::new(Cell::new(0)) }
    }
    fn seek<C>(&self, endpoint: VId, relation: RelationId, direction: GlaDirection,
        after: Option<EId>, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<&'static str, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        if relation == S && !self.allow_s { return Ok(None); }
        self.lookup.set(self.lookup.get() + 1);
        if self.unavailable { return Err(EdgeExpansionSourceError::Unavailable); }
        // This returns a history superset; the kernel must still check the
        // actual relation, orientation and endpoints of every returned edge.
        Ok(EDGES.iter().find(|&&(eid, a, b, _)| {
            after.is_none_or(|after| eid > after) && match direction {
                GlaDirection::Forward => a == endpoint,
                GlaDirection::Reverse => b == endpoint,
                GlaDirection::Undirected => a == endpoint || b == endpoint,
            }
        }).map(|edge| edge.0))
    }
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(9) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = (0..3).map(VId).find(|id| self.after.is_none_or(|after| *id > after));
        if next.is_some() { self.after = next; }
        Ok(next)
    }
    fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok((id.0 < 3).then_some(VertexScanRow { labels: &[], properties: &[] }))
    }
    fn next_probe_edge<C>(&self, _: VId, _: GlaDirection, _: Option<EId>,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        panic!("probe lost its relation before source admission")
    }
    fn next_probe_edge_for_relation<C>(&self, endpoint: VId, relation: RelationId,
        direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seek(endpoint, relation, direction, after, control)
    }
    fn probe_edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::edge(self, eid, control).map_err(EdgeExpansionSourceError::Read)
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(9) }
    fn next_edge<C>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = EDGES.iter().map(|e| e.0).find(|id| self.edge_after.is_none_or(|after| *id > after));
        if next.is_some() { self.edge_after = next; }
        Ok(next)
    }
    fn edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(EDGES.iter().find(|e| e.0 == eid).map(|&(_, source, target, relation)| {
            EdgeScanRow { source, target, relation, properties: &[] }
        }))
    }
    fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        VertexScanSource::vertex(self, id, &mut |event| control(edge_event(event)))
    }
    fn next_incident_edge<C>(&self, _: VId, _: GlaDirection, _: Option<EId>,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        panic!("joined expansion lost its relation before source admission")
    }
    fn next_incident_edge_for_relation<C>(&self, endpoint: VId, relation: RelationId,
        direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seek(endpoint, relation, direction, after, control)
    }
}
fn pattern(text: &str) -> crate::algebra::PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn plan(text: &str) -> VertexScanPlan<GraphValueRow> {
    VertexScanPlan::compile(pattern(text).plan()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 100_000, 100_000) }
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).unwrap().as_vertex().unwrap()).collect()
}
fn rows(text: &str, allowed: bool) -> (Vec<GraphValueRow>, usize) {
    let source = Source::new(allowed, false);
    let calls = Rc::clone(&source.lookup);
    let result = VertexScanCursor::new(source, plan(text), policy(), || Ok::<_, ()>(()))
        .collect::<Result<Vec<_>, _>>().unwrap();
    (result, calls.get())
}

#[test]
fn fixed_variable_and_zero_length_probes_use_the_exact_relation_before_incidence() {
    for (atom, expected) in [
        ("-[:S]->(b)", vec![VId(1)]),
        ("<-[:S]-(b)", vec![VId(2)]),
        ("-[:S]-(b)", vec![VId(1), VId(2)]),
        ("-[:S*1..3]->(b)", vec![VId(1)]),
        ("<-[:S*1..3]-(b)", vec![VId(2)]),
        ("-[:S*1..3]-(b)", vec![VId(1), VId(2)]),
    ] {
        let text = format!("MATCH (a) WHERE EXISTS {{ MATCH (a){atom} }} RETURN a");
        assert_eq!(ids(&rows(&text, true).0), expected);
        let (none, visits) = rows(&text, false);
        assert!(none.is_empty());
        assert_eq!(visits, 0, "denied relation opened its incidence index");
        let anti = format!("MATCH (a) WHERE NOT EXISTS {{ MATCH (a){atom} }} RETURN a");
        let (all, visits) = rows(&anti, false);
        assert_eq!(ids(&all), vec![VId(0), VId(1), VId(2)]);
        assert_eq!(visits, 0);
    }
    for bounds in ["0..0", "0..3"] {
        let (all, visits) = rows(&format!(
            "MATCH (a) WHERE EXISTS {{ MATCH (a)-[:S*{bounds}]->(b) }} RETURN a"
        ), false);
        assert_eq!(ids(&all), vec![VId(0), VId(1), VId(2)]);
        assert_eq!(visits, 0, "identity paths require vertices, not relation access");
    }
}

#[test]
fn connected_edge_stream_passes_the_appended_relation_without_materializing_neighbors() {
    let query = pattern("MATCH (a)-[e:R]->(b)-[f:S]->(c) RETURN e, a, f, c");
    for allowed in [true, false] {
        let source = Source::new(allowed, false);
        let calls = Rc::clone(&source.lookup);
        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
        let result = EdgeScanCursor::new(source, plan, policy(), || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>().unwrap();
        if allowed {
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].get(1), Some(&GraphValue::Vertex(VId(0))));
            assert_eq!(result[0].get(3), Some(&GraphValue::Vertex(VId(2))));
            assert!(calls.get() > 0);
        } else {
            assert!(result.is_empty());
            assert_eq!(calls.get(), 0);
        }
    }
}

#[test]
fn allowed_missing_index_is_not_absence_and_each_control_refusal_fuses_the_cursor() {
    let text = "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a";
    let mut cursor = VertexScanCursor::new(Source::new(true, true), plan(text), policy(), || Ok::<_, ()>(()));
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(
        VertexScanError::Probe(EdgeScanError::ExpansionUnavailable)
    )))));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:S*1..3]->(b) } RETURN a";
    let mut checkpoints = 0;
    let (expected, stats, counts) = {
        let mut cursor = VertexScanCursor::new(Source::new(true, false), plan(text), policy(), || {
            checkpoints += 1; Ok::<_, usize>(())
        });
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        (rows, cursor.evaluator_stats(), cursor.row_stats())
    };
    for cut in 1..=checkpoints {
        let mut seen = 0;
        let mut cursor = VertexScanCursor::new(Source::new(true, false), plan(text), policy(), || {
            seen += 1; if seen == cut { Err(cut) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => { assert_eq!(at, cut); break; }
                other => panic!("expected exact refusal, got {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let exact = GqlQueryPolicy::new(counts.snapshot_records, counts.result_rows, stats.work_units, stats.scratch_entries);
    let got = VertexScanCursor::new(Source::new(true, false), plan(text), exact, || Ok::<_, ()>(()))
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(got, expected);
}

// Neither default adapter may add controls or reinterpret an unavailable index.
struct Legacy;
impl VertexScanSource for Legacy {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(0) }
    fn next_vertex<C>(&mut self, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> { Ok(None) }
    fn vertex<'a, C>(&'a self, _: VId, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> { Ok(None) }
    fn next_probe_edge<C>(&self, _: VId, _: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::ScratchEntry)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok(after.is_none().then_some(EId(u128::MAX)))
    }
}
impl EdgeScanSource for Legacy {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(0) }
    fn next_edge<C>(&mut self, _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> { Ok(None) }
    fn edge<'a, C>(&'a self, _: EId, _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> { Ok(None) }
    fn vertex<'a, C>(&'a self, _: VId, _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> { Ok(None) }
    fn next_incident_edge<C>(&self, endpoint: VId, direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.next_probe_edge(endpoint, direction, after, control)
    }
}
#[test]
fn relation_aware_defaults_preserve_exact_candidate_and_control_transcripts() {
    for after in [None, Some(EId(u128::MAX))] {
        let mut expected = Vec::new();
        let value = Legacy.next_probe_edge(VId(0), GlaDirection::Forward, after,
            &mut |e| { expected.push(e); Ok::<_, ()>(()) }).unwrap();
        for relation in [R, S] {
            let mut events = Vec::new();
            assert_eq!(Legacy.next_probe_edge_for_relation(VId(0), relation, GlaDirection::Forward, after,
                &mut |e| { events.push(e); Ok::<_, ()>(()) }).unwrap(), value);
            assert_eq!(events, expected);
            events.clear();
            assert_eq!(Legacy.next_incident_edge_for_relation(VId(0), relation, GlaDirection::Forward, after,
                &mut |e| { events.push(e); Ok::<_, ()>(()) }).unwrap(), value);
            assert_eq!(events, expected);
        }
    }
}
