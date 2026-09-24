use super::*;
use crate::algebra::{GlaDirection, GraphValue, GraphValueRow};
use crate::edge_stream::*;
use crate::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_delta_types::LabelId;
use fgdb_types::{CommitSeq, EId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const EDGES: [(EId, VId, VId); 4] = [
    (EId(0), VId(0), VId(1)),
    (EId(1), VId(1), VId(2)),
    (EId(2), VId(1), VId(1)),
    (EId(u128::MAX), VId(0), VId(1)),
];
struct Masked {
    at: usize,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
    denied: bool,
    fail: bool,
}
impl Masked {
    fn new() -> Self {
        Self {
            at: 0,
            props: vec![
                (P, CanonicalScalar::Int(7)),
                (
                    H,
                    CanonicalScalar::ucs_basic_text(&"private".repeat(100)).unwrap(),
                ),
            ],
            denied: false,
            fail: false,
        }
    }
    fn raw(&self, id: EId) -> Option<EdgeScanRow<'_>> {
        EDGES
            .iter()
            .find(|(eid, _, _)| *eid == id)
            .map(|&(_, source, target)| EdgeScanRow {
                source,
                target,
                relation: R,
                properties: &self.props,
            })
    }
}
impl EdgeScanSource for Masked {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(1)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        assert!(!self.denied, "denied root relation opened its index");
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = EDGES.get(self.at).map(|&(id, _, _)| id);
        self.at += usize::from(next.is_some());
        Ok(next)
    }
    fn next_edge_for_relation<C>(
        &mut self,
        relation: RelationId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.denied || relation != R {
            Ok(None)
        } else {
            self.next_edge(control)
        }
    }
    fn edge<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        // The joined topology route must not be mistaken for full fields.
        Ok(self.raw(id).map(|edge| EdgeScanRow {
            properties: &[],
            ..edge
        }))
    }
    fn edge_record<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRecord<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.raw(id)
            .map(|row| {
                EdgeScanRecord::copy_masked(row, |key| key == P, control)
                    .map_err(EdgeScanSourceError::Control)
            })
            .transpose()
    }
    fn edge_property<'a, C>(
        &'a self,
        id: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail {
            return Err(EdgeScanSourceError::Source("scoped edge property refused"));
        }
        Ok(self.raw(id).map(|_| (key == P).then_some(&self.props[0].1)))
    }
    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        panic!("edge cursor bypassed owned masked endpoint records")
    }
    fn vertex_record<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if id.0 > 2 {
            return Ok(None);
        }
        VertexScanRecord::copy_masked(
            VertexScanRow {
                labels: &[LabelId(1), LabelId(99)],
                properties: &self.props,
            },
            |key| key == LabelId(1),
            |key| key == P,
            &mut |event| {
                control(match event {
                    crate::stream::VertexScanEvent::Work => GlaExecutionEvent::Work,
                    crate::stream::VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                })
            },
        )
        .map(Some)
        .map_err(EdgeScanSourceError::Control)
    }
    fn vertex_property<'a, C>(
        &'a self,
        id: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok((id.0 <= 2).then(|| (key == P).then_some(&self.props[0].1)))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        Ok(EDGES
            .iter()
            .find(|&&(id, a, b)| {
                after.is_none_or(|after| id > after)
                    && match direction {
                        GlaDirection::Forward => a == endpoint,
                        GlaDirection::Reverse => b == endpoint,
                        GlaDirection::Undirected => a == endpoint || b == endpoint,
                    }
            })
            .map(|&(id, _, _)| id))
    }
}
fn plan(text: &str) -> EdgeScanPlan {
    let pattern = PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    EdgeScanPlan::compile(pattern.plan()).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10000, 10000, 1_000_000, 1_000_000)
}
fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}
fn integer() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(7))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

#[test]
fn masked_edge_and_endpoint_fields_preserve_orientation_multiplicity_and_pages() {
    for (arrow, direction) in [
        ("-[r:R]->", GlaDirection::Forward),
        ("<-[r:R]-", GlaDirection::Reverse),
        ("-[r:R]-", GlaDirection::Undirected),
    ] {
        let mut expected = Vec::new();
        for &(eid, a, b) in &EDGES {
            let orientations = match direction {
                GlaDirection::Forward => vec![(a, b)],
                GlaDirection::Reverse => vec![(b, a)],
                _ if a == b => vec![(a, b)],
                _ => vec![(a.min(b), a.max(b)), (a.max(b), a.min(b))],
            };
            for (a, b) in orientations {
                expected.push(row(vec![
                    GraphValue::Edge(eid),
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                    integer(),
                    null(),
                    integer(),
                    null(),
                ]));
            }
        }
        for distinct in ["ALL", "DISTINCT"] {
            for (skip, take) in [(0, 20), (1, 2), (2, 0), (100, 3)] {
                let query = format!(
                    "MATCH (a){arrow}(b) WHERE r.hidden IS NULL AND a.hidden IS NULL AND r.p = 7 RETURN {distinct} r, a, b, r.p AS ep, r.hidden AS eh, a.p AS ap, b.hidden AS bh SKIP {skip} LIMIT {take}"
                );
                let actual =
                    EdgeScanCursor::new(Masked::new(), plan(&query), policy(), || Ok::<_, ()>(()));
                assert_eq!(
                    actual.collect::<Result<Vec<_>, _>>().unwrap(),
                    expected
                        .iter()
                        .skip(skip)
                        .take(take)
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}

const JOIN: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE r.hidden IS NULL AND s.p = 7 RETURN r, a, s, c, r.p AS ep, s.hidden AS eh";
#[test]
fn connected_join_fields_use_the_scoped_accessor_not_redacted_topology_properties() {
    let mut expected = Vec::new();
    for &(r, a, b) in &EDGES {
        for &(s, from, c) in &EDGES {
            if b == from {
                expected.push(row(vec![
                    GraphValue::Edge(r),
                    GraphValue::Vertex(a),
                    GraphValue::Edge(s),
                    GraphValue::Vertex(c),
                    integer(),
                    null(),
                ]));
            }
        }
    }
    let actual = EdgeScanCursor::new(Masked::new(), plan(JOIN), policy(), || Ok::<_, ()>(()));
    assert_eq!(actual.collect::<Result<Vec<_>, _>>().unwrap(), expected);
    let mut failed = Masked::new();
    failed.fail = true;
    let mut cursor = EdgeScanCursor::new(failed, plan(JOIN), policy(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "scoped edge property refused"
        ))))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.next().is_none());
    let mut denied = Masked::new();
    denied.denied = true;
    assert!(
        EdgeScanCursor::new(denied, plan(JOIN), policy(), || Ok::<_, ()>(()))
            .next()
            .is_none()
    );
}

#[test]
fn every_masked_cursor_checkpoint_fuses_and_inclusive_native_limits_are_unchanged() {
    for text in [
        JOIN,
        "MATCH (a)-[r:R]->(b) RETURN r, a, r.p AS ep, b.hidden AS bh",
    ] {
        let mut calls = 0;
        let (expected, stats, rows) = {
            let mut cursor = EdgeScanCursor::new(Masked::new(), plan(text), policy(), || {
                calls += 1;
                Ok::<_, usize>(())
            });
            let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            (expected, cursor.evaluator_stats(), cursor.row_stats())
        };
        for cut in 1..=calls {
            let mut seen = 0;
            let mut cursor = EdgeScanCursor::new(Masked::new(), plan(text), policy(), || {
                seen += 1;
                if seen == cut { Err(cut) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => {
                        assert_eq!(at, cut);
                        break;
                    }
                    other => panic!("expected injected refusal: {other:?}"),
                }
            }
            assert!(expected.starts_with(&prefix));
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            stats.work_units,
            stats.scratch_entries,
        );
        assert_eq!(
            EdgeScanCursor::new(Masked::new(), plan(text), exact, || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
        for budget in [
            GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                stats.work_units - 1,
                stats.scratch_entries,
            ),
            GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                stats.work_units,
                stats.scratch_entries - 1,
            ),
        ] {
            assert!(
                EdgeScanCursor::new(Masked::new(), plan(text), budget, || Ok::<_, ()>(()))
                    .collect::<Result<Vec<_>, _>>()
                    .is_err()
            );
        }
    }
}

#[test]
fn hidden_payloads_add_no_copy_work_and_partial_records_do_not_escape() {
    let source = Masked::new();
    let mut tiny = Masked::new();
    tiny.props[1].1 = CanonicalScalar::Int(0);
    let copy = |source: &Masked, stop: usize| {
        let mut calls = 0;
        let result =
            EdgeScanRecord::copy_masked(source.raw(EId(0)).unwrap(), |key| key == P, &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
        (result, calls)
    };
    let (record, calls) = copy(&source, usize::MAX);
    assert_eq!(record.unwrap().as_row().properties, &source.props[..1]);
    assert_eq!(copy(&tiny, usize::MAX).1, calls);
    for cut in 1..=calls {
        assert!(matches!(copy(&source, cut), (Err(at), seen) if at == cut && seen == cut));
    }
}

#[test]
fn default_record_and_relation_routes_preserve_borrowed_data_and_event_order() {
    struct Borrowed(Masked);
    impl EdgeScanSource for Borrowed {
        type Error = &'static str;
        fn snapshot_seq(&self) -> CommitSeq {
            CommitSeq(1)
        }
        fn next_edge<C>(
            &mut self,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
        ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
            self.0.next_edge(control)
        }
        fn edge<'a, C>(
            &'a self,
            id: EId,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
        ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            Ok(self.0.raw(id))
        }
        fn vertex<'a, C>(
            &'a self,
            _: VId,
            _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
            Ok(None)
        }
    }
    let mut source = Borrowed(Masked::new());
    let mut events = Vec::new();
    let record = source
        .edge_record(EId(0), &mut |e| {
            events.push(e);
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    assert!(matches!(record, EdgeScanRecord::Borrowed(_)));
    assert!(std::ptr::eq(
        record.as_row().properties.as_ptr(),
        source.0.props.as_ptr()
    ));
    assert_eq!(events, vec![GlaExecutionEvent::Work]);
    let mut direct = Vec::new();
    let _ = source
        .edge(EId(0), &mut |e| {
            direct.push(e);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(direct, events);
    drop(record);
    let property = source
        .edge_property(EId(0), P, &mut |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(std::ptr::eq(property, &source.0.props[0].1));
    assert!(
        source
            .edge_property(EId(50), P, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        source
            .next_edge_for_relation(R, &mut |_| Ok::<_, ()>(()))
            .unwrap(),
        Some(EId(0))
    );
}
