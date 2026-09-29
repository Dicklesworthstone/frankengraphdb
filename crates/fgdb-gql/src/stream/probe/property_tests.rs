//! A topology-only source must retain its separate, fallible field reader.
//! These fixtures exercise source contracts; they do not simulate Warden.
use super::*;
use crate::algebra::{GlaDirection, GraphValueRow};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::RelationId;
use std::cell::Cell;
use std::rc::Rc;

const KEY: PropertyKeyId = PropertyKeyId(7);
const R: RelationId = RelationId(2);
const EDGES: [(EId, VId, VId); 3] = [
    (EId(1), VId(0), VId(1)),
    (EId(2), VId(0), VId(1)),
    (EId(u128::MAX), VId(1), VId(2)),
];
#[derive(Clone, Copy)]
enum Field {
    Values,
    Masked,
    MissingEdge,
    Unavailable,
    Failed,
}
struct Source {
    after: Option<VId>,
    field: Field,
    values: [CanonicalScalar; 3],
    reads: Rc<Cell<usize>>,
    drops: Rc<Cell<usize>>,
}
impl Source {
    fn new(field: Field) -> Self {
        Self {
            after: None,
            field,
            values: [CanonicalScalar::Int(0), CanonicalScalar::Int(9), CanonicalScalar::Null],
            reads: Rc::new(Cell::new(0)),
            drops: Rc::new(Cell::new(0)),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(11)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = (0..3).map(VId).find(|v| self.after.is_none_or(|p| *v > p));
        if next.is_some() {
            self.after = next;
        }
        Ok(next)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok((vid.0 < 3).then_some(VertexScanRow { labels: &[], properties: &[] }))
    }
    fn next_probe_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok(EDGES.iter().find(|&&(eid, a, b)| {
            after.is_none_or(|p| eid > p) && match direction {
                GlaDirection::Forward => a == endpoint,
                GlaDirection::Reverse => b == endpoint,
                GlaDirection::Undirected => a == endpoint || b == endpoint,
            }
        }).map(|e| e.0))
    }
    fn probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok(EDGES.iter().find(|e| e.0 == eid).map(|&(_, source, target)| EdgeScanRow {
            source, target, relation: R, properties: &[],
        }))
    }
    fn probe_edge_property<'a, C>(
        &'a self,
        eid: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        self.reads.set(self.reads.get() + 1);
        match self.field {
            Field::Unavailable => Err(EdgeExpansionSourceError::Unavailable),
            Field::Failed => Err(EdgeExpansionSourceError::Read(VertexScanSourceError::Source("field failed"))),
            Field::MissingEdge => Ok(None),
            Field::Masked => Ok(Some(None)),
            Field::Values => Ok(EDGES.iter().position(|e| e.0 == eid)
                .map(|at| (key == KEY).then_some(&self.values[at]))),
        }
    }
}
fn plan(anti: bool, predicate: &str) -> VertexScanPlan<GraphValueRow> {
    let text = format!("MATCH (a) WHERE {}EXISTS {{ MATCH (a)-[e:R]->(b) WHERE {predicate} }} RETURN a", if anti { "NOT " } else { "" });
    let prepared = PreparedGraphText::prepare(&text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(KEY)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    VertexScanPlan::compile(prepared.plan()).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 100_000, 100_000)
}
fn identities(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).unwrap().as_vertex().unwrap()).collect()
}

#[test]
fn private_edge_predicates_use_the_field_route_not_empty_topology_payloads() {
    for (field, predicate, exists, absent) in [
        (Field::Values, "e.p = 9", vec![VId(0)], vec![VId(1), VId(2)]),
        (Field::Values, "e.p IS NULL", vec![VId(1)], vec![VId(0), VId(2)]),
        (Field::Masked, "e.p = 9", vec![], vec![VId(0), VId(1), VId(2)]),
        (Field::Masked, "e.p IS NULL", vec![VId(0), VId(1)], vec![VId(2)]),
    ] {
        for (anti, expected) in [(false, exists), (true, absent)] {
            let source = Source::new(field);
            let reads = Rc::clone(&source.reads);
            let rows = VertexScanCursor::new(source, plan(anti, predicate), policy(), || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(identities(&rows), expected);
            assert!(reads.get() > 0, "a private capture bypassed its field source");
        }
    }
}

#[test]
fn missing_bound_edges_and_field_failures_are_not_successful_anti_probes() {
    for field in [Field::MissingEdge, Field::Unavailable, Field::Failed] {
        let source = Source::new(field);
        let drops = Rc::clone(&source.drops);
        let mut cursor = VertexScanCursor::new(source, plan(true, "e.p IS NULL"), policy(), || Ok::<_, ()>(()));
        let error = cursor.next().expect("one terminal error").unwrap_err();
        match field {
            Field::MissingEdge => assert!(matches!(error, GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::BoundEdgeUnavailable)))),
            Field::Unavailable => assert!(matches!(error, GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::ExpansionUnavailable)))),
            Field::Failed => assert!(matches!(error, GqlQueryError::Source(VertexScanError::Source("field failed")))),
            _ => unreachable!(),
        }
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(drops.get(), 1);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn field_lookup_borrows_values_and_preserves_exact_control_refusal() {
    let source = Source::new(Field::Values);
    let lookup = Lookup(&source);
    let value = lookup.edge_property(EId(2), KEY, &mut |_| Ok::<_, ()>(())).unwrap().unwrap().unwrap();
    assert!(std::ptr::eq(value, &source.values[1]));
    assert_eq!(lookup.edge_property(EId(2), PropertyKeyId(99), &mut |_| Ok::<_, ()>(())).unwrap(), Some(None));
    assert_eq!(lookup.edge_property(EId(44), KEY, &mut |_| Ok::<_, ()>(())).unwrap(), None);
    let reads = source.reads.get();
    assert!(matches!(lookup.edge_property(EId(2), KEY, &mut |_| Err(17)), Err(EdgeScanSourceError::Control(17))));
    assert_eq!(source.reads.get(), reads, "source work continued after refusal");
}

#[test]
fn every_control_cut_fuses_and_all_dimensions_remain_cumulative() {
    for anti in [false, true] {
        let mut calls = 0;
        let (rows, r, e) = {
            let mut c = VertexScanCursor::new(Source::new(Field::Values), plan(anti, "e.p = 9"), policy(), || {
                calls += 1;
                Ok::<_, usize>(())
            });
            let rows = c.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            (rows, c.row_stats(), c.evaluator_stats())
        };
        for cut in 1..=calls {
            let mut seen = 0;
            let source = Source::new(Field::Values);
            let drops = Rc::clone(&source.drops);
            let mut c = VertexScanCursor::new(source, plan(anti, "e.p = 9"), policy(), || {
                seen += 1;
                if seen == cut { Err(cut) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match c.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => { assert_eq!(at, cut); break; }
                    other => panic!("refused property probe returned {other:?}"),
                }
            }
            assert!(rows.starts_with(&prefix));
            assert_eq!(c.state(), VertexScanState::Failed);
            assert!(c.next().is_none());
            assert_eq!(drops.get(), 1);
            drop(c);
            assert_eq!(seen, cut);
        }
        let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
        let mut c = VertexScanCursor::new(Source::new(Field::Values), plan(anti, "e.p = 9"), exact, || Ok::<_, ()>(()));
        assert_eq!(c.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), rows);
        assert_eq!((c.row_stats(), c.evaluator_stats()), (r, e));
        for p in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut c = VertexScanCursor::new(Source::new(Field::Values), plan(anti, "e.p = 9"), p, || Ok::<_, ()>(()));
            assert!(c.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(c.state(), VertexScanState::Failed);
            assert!(c.next().is_none());
        }
    }
}

struct FullRecord(Vec<(PropertyKeyId, CanonicalScalar)>);
impl VertexScanSource for FullRecord {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(11) }
    fn next_vertex<C>(&mut self, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> { Ok(None) }
    fn vertex<'a, C>(&'a self, _: VId, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> { Ok(None) }
    fn probe_edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok((eid == EId(1)).then_some(EdgeScanRow {
            source: VId(0), target: VId(1), relation: R, properties: &self.0,
        }))
    }
}
#[test]
fn default_field_reader_keeps_borrowed_records_and_controlled_binary_search() {
    let source = FullRecord(vec![
        (PropertyKeyId(1), CanonicalScalar::Int(1)),
        (KEY, CanonicalScalar::Int(7)),
        (PropertyKeyId(9), CanonicalScalar::Int(9)),
    ]);
    let lookup = Lookup(&source);
    let mut calls = 0;
    let value = lookup.edge_property(EId(1), KEY, &mut |event| {
        assert_eq!(event, GlaExecutionEvent::Work);
        calls += 1;
        Ok::<_, usize>(())
    }).unwrap().unwrap().unwrap();
    assert!(std::ptr::eq(value, &source.0[1].1));
    assert_eq!(calls, 2, "one record lookup and one middle-key comparison");
    for stop in 1..=calls {
        let mut seen = 0;
        let result = lookup.edge_property(EId(1), KEY, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(EdgeScanSourceError::Control(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    assert_eq!(lookup.edge_property(EId(1), PropertyKeyId(8), &mut |_| Ok::<_, ()>(())).unwrap(), Some(None));
    assert_eq!(lookup.edge_property(EId(99), KEY, &mut |_| Ok::<_, ()>(())).unwrap(), None);
}
