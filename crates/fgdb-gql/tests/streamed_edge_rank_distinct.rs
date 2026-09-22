//! Independent public-API regressions for ranked and DISTINCT edge aggregates.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue};
use fgdb_gql::edge_stream::aggregate::{EdgeAggregateCursor, EdgeAggregatePlan};
use fgdb_gql::edge_stream::{
    EdgeExpansionSourceError, EdgeScanRow, EdgeScanSource, EdgeScanSourceError, EdgeScanState,
};
use fgdb_gql::stream::VertexScanRow;
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateRow, GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};

mod streamed_edge_rank_distinct {
    pub(super) mod distinct_output;
    pub(super) mod ranked;
}

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId, Vec<(PropertyKeyId, CanonicalScalar)>);
#[derive(Clone)]
struct Source {
    vertices: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>,
    edges: BTreeMap<EId, Edge>,
    incident: BTreeMap<(VId, u8), BTreeSet<EId>>,
    after: Option<EId>,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    fail: Option<EId>,
    unavailable: bool,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let next = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(id, _)| *id);
        if let Some(id) = next {
            self.after = Some(id);
        }
        Ok(next)
    }
    fn edge<C>(
        &self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge unavailable"));
        }
        Ok(self
            .edges
            .get(&eid)
            .map(|(source, relation, target, properties)| EdgeScanRow {
                source: *source,
                target: *target,
                relation: *relation,
                properties,
            }))
    }
    fn vertex<C>(
        &self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow {
            labels: &[],
            properties,
        }))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        let dir = match direction {
            GlaDirection::Forward => 0,
            GlaDirection::Reverse => 1,
            GlaDirection::Undirected => 2,
        };
        Ok(self.incident.get(&(endpoint, dir)).and_then(|ids| {
            ids.range((after.map_or(Unbounded, Excluded), Unbounded))
                .next()
                .copied()
        }))
    }
}
fn source(mask: u32) -> Source {
    let ids = [VId(0), VId(1), VId(u128::MAX)];
    let vertices = BTreeMap::from([
        (ids[0], vec![(P, CanonicalScalar::Int(-3))]),
        (ids[1], vec![(P, CanonicalScalar::Null)]),
        (ids[2], vec![(P, CanonicalScalar::Int(7))]),
    ]);
    let mut edges = BTreeMap::new();
    let mut incident: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    let raw = [
        (0, R, 1, Some(5)),
        (0, R, 1, None),
        (1, S, 2, Some(-2)),
        (2, R, 0, Some(1)),
        (1, S, 1, None),
        (0, R, 0, Some(-4)),
    ];
    for (at, (a, rel, b, weight)) in raw.into_iter().enumerate() {
        if mask & (1 << at) == 0 {
            continue;
        }
        let eid = EId(at as u128 + 1);
        let from = ids[a];
        let to = ids[b];
        let properties = weight
            .map(|w| vec![(P, CanonicalScalar::Int(w))])
            .unwrap_or_default();
        edges.insert(eid, (from, rel, to, properties));
        for key in [(from, 0), (to, 1), (from, 2), (to, 2)] {
            incident.entry(key).or_default().insert(eid);
        }
    }
    Source {
        vertices,
        edges,
        incident,
        after: None,
        reads: Arc::new(AtomicUsize::new(0)),
        drops: Arc::new(AtomicUsize::new(0)),
        fail: None,
        unavailable: false,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name: &str| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(if name == "S" { S } else { R })),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn run(
    q: &PreparedGraphAggregate,
    s: Source,
    p: GqlQueryPolicy,
) -> EdgeAggregateCursor<Source, impl FnMut() -> Result<(), ()>> {
    EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(q).unwrap(), p, || Ok(()))
}
fn integer(props: &[(PropertyKeyId, CanonicalScalar)]) -> Option<i128> {
    props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == P => Some(i128::from(*value)),
        _ => None,
    })
}
fn sum(value: Option<i128>) -> GraphAggregateValue {
    value.map_or_else(
        || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        GraphAggregateValue::Integer,
    )
}
