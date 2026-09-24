//! The actual indexed probe kernel must never bypass a masked record source.
use super::*;
use crate::algebra::{GlaDirection, GraphValue, GraphValueRow};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::RelationId;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 4] = [VId(0), VId(1), VId(2), VId(u128::MAX)];
const EDGES: [(EId, VId, VId); 4] = [
    (EId(0), VId(0), VId(1)), (EId(1), VId(1), VId(2)),
    (EId(2), VId(2), VId(0)), (EId(u128::MAX), VId(0), VId(1)),
];

#[derive(Clone, Copy)]
enum Fault { None, Property, MissingBoundVertex }
struct Masked {
    after: Option<VId>,
    props: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>,
    fault: Fault,
}
impl Masked {
    fn new(fault: Fault) -> Self {
        Self {
            after: None,
            props: [(VId(0), 1), (VId(1), 2), (VId(2), 0)].into_iter().map(|(id, value)| {
                (id, vec![(P, CanonicalScalar::Int(value)), (H, CanonicalScalar::Int(7))])
            }).collect(),
            fault,
        }
    }
    fn raw(&self, id: VId) -> Option<VertexScanRow<'_>> {
        IDS.contains(&id).then_some(VertexScanRow {
            labels: &[LabelId(1), LabelId(99)],
            properties: self.props.get(&id).map_or(&[], Vec::as_slice),
        })
    }
}
impl VertexScanSource for Masked {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>>
    {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = IDS.into_iter().find(|id| self.after.is_none_or(|after| *id > after));
        if next.is_some() { self.after = next; }
        Ok(next)
    }
    fn vertex<'a, C>(&'a self, _: VId, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>>
    {
        panic!("probe bypassed owned records and requested unmasked metadata")
    }
    fn vertex_record<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<Self::Error, C>>
    {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.raw(id).map(|row| VertexScanRecord::copy_masked(
            row, |label| label == LabelId(1), |key| key == P, control,
        ).map_err(VertexScanSourceError::Control)).transpose()
    }
    fn vertex_property<'a, C>(&'a self, id: VId, key: PropertyKeyId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<Option<&'a CanonicalScalar>>, VertexScanSourceError<Self::Error, C>>
    {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        match self.fault {
            Fault::Property => return Err(VertexScanSourceError::Source("property refused")),
            Fault::MissingBoundVertex => return Ok(None),
            Fault::None => {}
        }
        let Some(row) = self.raw(id) else { return Ok(None); };
        if key != P { return Ok(Some(None)); }
        super::super::seek(row.properties, &key, |entry| entry.0, control)
            .map(|entry| Some(entry.map(|(_, value)| value)))
            .map_err(VertexScanSourceError::Control)
    }
    fn next_probe_vertex<C>(&self, after: Option<VId>, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>>
    {
        control(GlaExecutionEvent::Work).map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok(IDS.into_iter().find(|id| after.is_none_or(|after| *id > after)))
    }
    fn next_probe_edge<C>(&self, endpoint: VId, direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>>
    {
        for (eid, a, b) in EDGES {
            control(GlaExecutionEvent::Work).map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
            let incident = match direction {
                GlaDirection::Forward => a == endpoint,
                GlaDirection::Reverse => b == endpoint,
                GlaDirection::Undirected => a == endpoint || b == endpoint,
            };
            if incident && after.is_none_or(|after| eid > after) { return Ok(Some(eid)); }
        }
        Ok(None)
    }
    fn probe_edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>>
    {
        control(GlaExecutionEvent::Work).map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        Ok(EDGES.into_iter().find(|(id, _, _)| *id == eid).map(|(_, source, target)| {
            EdgeScanRow { source, target, relation: R, properties: &[] }
        }))
    }
}
fn plan(text: &str) -> VertexScanPlan<GraphValueRow> {
    let query = PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    VertexScanPlan::compile(query.plan()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 1000, 1_000_000, 1_000_000) }
fn identities(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| match row.get(0).unwrap() {
        GraphValue::Vertex(id) => *id,
        _ => panic!("identity projection"),
    }).collect()
}

#[test]
fn borrowed_defaults_preserve_lookup_events_and_distinguish_missing_vertices_from_null_fields() {
    struct Borrowed(CanonicalScalar);
    impl VertexScanSource for Borrowed {
        type Error = ();
        fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
        fn next_vertex<C>(&mut self, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
            -> Result<Option<VId>, VertexScanSourceError<(), C>>
        { Ok(None) }
        fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
            -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<(), C>>
        {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            Ok((id == VId(1)).then_some(VertexScanRow { labels: &[], properties: &[] }))
        }
    }
    let source = Borrowed(CanonicalScalar::Null);
    let mut events = Vec::new();
    assert_eq!(source.vertex_property(VId(0), P, &mut |event| {
        events.push(event); Ok::<_, ()>(())
    }).unwrap(), None);
    assert_eq!(events, vec![VertexScanEvent::Work]);
    events.clear();
    assert_eq!(source.vertex_property(VId(1), P, &mut |event| {
        events.push(event); Ok::<_, ()>(())
    }).unwrap(), Some(None));
    assert_eq!(events, vec![VertexScanEvent::Work]);
    assert!(matches!(source.vertex_property(VId(1), P, &mut |_| Err(73)),
        Err(VertexScanSourceError::Control(73))));
    // The edge-family default also preserves the distinction and zero-copy
    // record borrowing when the vertex-root bridge is not involved.
    struct EdgeBorrowed { props: Vec<(PropertyKeyId, CanonicalScalar)> }
    impl EdgeScanSource for EdgeBorrowed {
        type Error = ();
        fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
        fn next_edge<C>(&mut self, _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
            -> Result<Option<EId>, EdgeScanSourceError<(), C>> { Ok(None) }
        fn edge<'a, C>(&'a self, _: EId, _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
            -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<(), C>> { Ok(None) }
        fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
            -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<(), C>>
        {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            Ok((id == VId(1)).then_some(VertexScanRow { labels: &[], properties: &self.props }))
        }
    }
    let source = EdgeBorrowed { props: vec![(P, source.0)] };
    let mut events = Vec::new();
    let value = source.vertex_property(VId(1), P, &mut |event| {
        events.push(event); Ok::<_, ()>(())
    }).unwrap().unwrap().unwrap();
    assert!(core::ptr::eq(value, &source.props[0].1));
    assert_eq!(events, vec![GlaExecutionEvent::Work, GlaExecutionEvent::Work]);
    assert!(matches!(source.vertex_record(VId(1), &mut |_| Ok::<_, ()>(())).unwrap(),
        Some(VertexScanRecord::Borrowed(_))));
    assert_eq!(source.vertex_property(VId(0), P, &mut |_| Ok::<_, ()>(())).unwrap(), None);
    assert_eq!(source.vertex_property(VId(1), H, &mut |_| Ok::<_, ()>(())).unwrap(), Some(None));
}

#[test]
fn owned_probe_records_and_scoped_scalars_cover_correlated_independent_and_walk_bindings() {
    for (body, expected) in [
        ("EXISTS { MATCH (a)-[:R]->(b) WHERE b.p > a.p AND b.hidden IS NULL }", vec![VId(0), VId(2)]),
        ("NOT EXISTS { MATCH (a)-[:R]->(b) WHERE NOT (b.hidden = 7) }", IDS.to_vec()),
        ("EXISTS { MATCH (x) WHERE x.p = a.p }", vec![VId(0), VId(1), VId(2)]),
        ("EXISTS { MATCH (x:H) }", vec![]),
        ("EXISTS { MATCH (a:H) }", vec![]),
        ("EXISTS { MATCH (a)-[:R*2..2]->(b) WHERE b.p > a.p }", vec![VId(2)]),
        ("EXISTS { MATCH (a)-[:R*0..0]->(b) WHERE b = a AND b.hidden IS NULL }", IDS.to_vec()),
    ] {
        let text = format!("MATCH (a) WHERE {body} RETURN a AS id");
        let mut cursor = VertexScanCursor::new(Masked::new(Fault::None), plan(&text), policy(), || Ok::<_, ()>(()));
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(identities(&rows), expected, "{text}");
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    }
}

#[test]
fn owned_probe_sources_preserve_directions_presence_multiplicity_and_pages() {
    for (atom, pairs) in [
        ("-[:R]->", vec![(0, 1), (1, 2), (2, 0), (0, 1)]),
        ("<-[:R]-", vec![(1, 0), (2, 1), (0, 2), (1, 0)]),
        ("-[:R]-", vec![(0, 1), (1, 0), (1, 2), (2, 1), (2, 0), (0, 2), (0, 1), (1, 0)]),
    ] {
        for anti in [false, true] {
            for (skip, take) in [(0, 10), (1, 1), (0, 0)] {
                let text = format!("MATCH (a) WHERE {}EXISTS {{ MATCH (a){atom}(b) WHERE b.p > a.p }} RETURN DISTINCT a AS id SKIP {skip} LIMIT {take}", if anti { "NOT " } else { "" });
                // Enumerate complete primitive pairs rather than calling a
                // production matcher, index, predicate or witness counter.
                let p = |id| match id { 0 => 1, 1 => 2, 2 => 0, _ => -99 };
                let expected: Vec<_> = IDS.into_iter().filter(|id| {
                    pairs.iter().any(|&(a, b)| a == id.0 && p(b) > p(a)) != anti
                }).skip(skip).take(take).collect();
                let rows = VertexScanCursor::new(Masked::new(Fault::None), plan(&text), policy(), || Ok::<_, ()>(()))
                    .collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(identities(&rows), expected, "{text}");
            }
        }
    }
}

#[test]
fn property_errors_and_missing_bound_vertices_never_turn_into_anti_join_success() {
    for fault in [Fault::Property, Fault::MissingBoundVertex] {
        let mut cursor = VertexScanCursor::new(Masked::new(fault), plan(
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.p > a.p } RETURN a AS id",
        ), policy(), || Ok::<_, ()>(()));
        match fault {
            Fault::Property => assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(VertexScanError::Source("property refused")))))),
            Fault::MissingBoundVertex => assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::DanglingEndpoint)))))),
            Fault::None => unreachable!(),
        }
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn every_probe_control_cut_is_terminal_and_native_allowances_remain_cumulative() {
    let text = "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden = a.p } RETURN a AS id";
    let mut calls = 0;
    let (expected, rows, usage) = {
        let mut cursor = VertexScanCursor::new(Masked::new(Fault::None), plan(text), policy(), || { calls += 1; Ok::<_, usize>(()) });
        let result = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        (result, cursor.row_stats(), cursor.evaluator_stats())
    };
    assert_eq!(identities(&expected), IDS);
    for cut in 1..=calls {
        let mut seen = 0;
        let mut cursor = VertexScanCursor::new(Masked::new(Fault::None), plan(text), policy(), || {
            seen += 1; if seen == cut { Err(cut) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(found))) => { assert_eq!(found, cut); break; }
                other => panic!("missing injected failure at {cut}: {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let exact = GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, usage.work_units, usage.scratch_entries);
    assert_eq!(VertexScanCursor::new(Masked::new(Fault::None), plan(text), exact, || Ok::<_, ()>(()))
        .collect::<Result<Vec<_>, _>>().unwrap(), expected);
    for policy in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, rows.result_rows, usage.work_units, usage.scratch_entries),
        GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows - 1, usage.work_units, usage.scratch_entries),
        GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, usage.work_units - 1, usage.scratch_entries),
        GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, usage.work_units, usage.scratch_entries - 1),
    ] {
        let mut cursor = VertexScanCursor::new(Masked::new(Fault::None), plan(text), policy, || Ok::<_, ()>(()));
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed);
    }
}
