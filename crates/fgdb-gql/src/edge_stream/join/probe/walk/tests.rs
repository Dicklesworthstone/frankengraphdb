use super::*;
use crate::algebra::{GlaPlan, GraphValue, GraphValueRow, VertexPredicate};
use crate::stream::VertexScanPlan;
use crate::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitSeq};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::rc::Rc;

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const MODES: [GraphWalkSearch; 6] = [GraphWalkSearch::All, GraphWalkSearch::AllShortest,
    GraphWalkSearch::AnyShortest, GraphWalkSearch::Trail, GraphWalkSearch::Acyclic, GraphWalkSearch::Simple];
struct Source {
    edges: BTreeMap<EId, (VId, VId)>,
    ids: BTreeSet<VId>,
    properties: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<EId>,
    bad: Option<EId>,
    repeat: bool,
    unavailable: bool,
    records: Rc<Cell<u64>>,
}
impl Source {
    fn new(edges: impl IntoIterator<Item = (u128, u128, u128)>) -> Self {
        let mut s = Self { edges: BTreeMap::new(), ids: (0..4).map(VId).collect(), properties: BTreeMap::new(),
            outgoing: BTreeMap::new(), incoming: BTreeMap::new(), after: None, bad: None, repeat: false,
            unavailable: false, records: Rc::new(Cell::new(0)) };
        for (eid, a, b) in edges {
            let (eid, a, b) = (EId(eid), VId(a), VId(b));
            s.edges.insert(eid, (a, b)); s.ids.extend([a, b]);
            s.outgoing.entry(a).or_default().insert(eid);
            s.incoming.entry(b).or_default().insert(eid);
        }
        s.properties.insert(VId(0), vec![(P, CanonicalScalar::Int(1))]);
        s.properties.insert(VId(1), vec![(P, CanonicalScalar::Null)]);
        s.properties.insert(VId(2), vec![(P, CanonicalScalar::Int(7))]);
        s
    }
    fn small(mask: usize) -> Self {
        Self::new([(0,0,0),(1,0,1),(2,0,1),(3,1,2),(4,2,0),(u128::MAX,2,2)]
            .into_iter().enumerate().filter(|(at,_)| mask & (1 << at) != 0).map(|(_,e)|e))
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(1) }
    fn next_edge<C>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeScanSourceError<Self::Error,C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self.edges.range((self.after.map_or(Unbounded,Excluded),Unbounded)).next().map(|(id,_)|*id);
        if next.is_some() { self.after = next; }
        Ok(next)
    }
    fn edge<'a,C>(&'a self, eid:EId, control:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)
        -> Result<Option<EdgeScanRow<'a>>,EdgeScanSourceError<Self::Error,C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.bad == Some(eid) { return Err(EdgeScanSourceError::Source("edge refused")); }
        Ok(self.edges.get(&eid).map(|&(source,target)| EdgeScanRow { source,target,relation:R,properties:&[] }))
    }
    fn vertex<'a,C>(&'a self, vid:VId, control:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)
        -> Result<Option<VertexScanRow<'a>>,EdgeScanSourceError<Self::Error,C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.ids.contains(&vid).then_some(VertexScanRow {
            labels:&[], properties:self.properties.get(&vid).map_or(&[],Vec::as_slice) }))
    }
    fn next_incident_edge<C>(&self, endpoint:VId, direction:GlaDirection, after:Option<EId>,
        control:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)
        -> Result<Option<EId>,EdgeExpansionSourceError<Self::Error,C>> {
        control(GlaExecutionEvent::Work).map_err(|e|EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        if self.unavailable { return Err(EdgeExpansionSourceError::Unavailable); }
        if self.repeat && after.is_some() { return Ok(after); }
        let next = |map:&BTreeMap<VId,BTreeSet<EId>>| map.get(&endpoint)
            .and_then(|s|s.range((after.map_or(Unbounded,Excluded),Unbounded)).next().copied());
        Ok(match direction { GlaDirection::Forward=>next(&self.outgoing), GlaDirection::Reverse=>next(&self.incoming),
            GlaDirection::Undirected=>match (next(&self.outgoing),next(&self.incoming)) {
                (Some(a),Some(b))=>Some(a.min(b)),(a,b)=>a.or(b) } })
    }
}
fn expansion(direction:GlaDirection)->Expansion { Expansion{source:0,relation:R,direction} }
fn wide()->GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX,u64::MAX,u64::MAX,u64::MAX) }

// Enumerate all bounded walks, then apply whole-route laws. This oracle has no
// depth-state pruning, active membership sets, endpoint caching or index seeks.
fn oracle(s:&Source, start:VId, d:GlaDirection, lo:u32, hi:u32, mode:GraphWalkSearch)->BTreeSet<VId> {
    let mut layer=vec![(vec![start],Vec::<EId>::new())]; let mut endpoints=BTreeSet::new();
    for depth in 0..=hi {
        if depth>=lo { for (vs,es) in &layer {
            let valid=match mode {
                GraphWalkSearch::Trail=>es.iter().collect::<BTreeSet<_>>().len()==es.len(),
                GraphWalkSearch::Acyclic=>vs.iter().collect::<BTreeSet<_>>().len()==vs.len(),
                GraphWalkSearch::Simple=> {
                    let n=vs.len()-usize::from(vs.len()>1 && vs.last()==Some(&start));
                    vs[..n].iter().collect::<BTreeSet<_>>().len()==n
                }, _=>true,
            };
            if valid { endpoints.insert(*vs.last().unwrap()); }
        } }
        if depth==hi { break; }
        let mut next=Vec::new();
        for (vs,es) in layer { let from=*vs.last().unwrap(); for (&eid,&(a,b)) in &s.edges {
            let mut to=Vec::new();
            if d!=GlaDirection::Reverse && a==from { to.push(b); }
            if d!=GlaDirection::Forward && b==from && (d!=GlaDirection::Undirected || a!=b) { to.push(a); }
            for to in to { let mut vs=vs.clone();vs.push(to);let mut es=es.clone();es.push(eid);next.push((vs,es)); }
        } }
        layer=next;
    }
    endpoints
}
fn collect(s:&Source,start:VId,d:GlaDirection,lo:u32,hi:u32,mode:GraphWalkSearch)->BTreeSet<VId> {
    let mut control=|_|Ok::<_,GqlQueryError<EdgeScanError<&'static str>,()>>(());
    let mut q=Endpoints::new(start,GraphWalkBounds::new(lo,hi).unwrap(),mode,s,&mut control).unwrap();
    let mut answers=BTreeSet::new();
    while let Some(v)=q.next(expansion(d),s,&mut control,&mut || {
        s.records.set(s.records.get()+1); Ok(())
    }).unwrap() { assert!(answers.insert(v),"duplicate endpoint support"); }
    answers
}
#[test]
fn all_six_modes_match_complete_walk_support_for_every_small_multigraph_interval_and_direction() {
    for mask in 0..64 { let s=Source::small(mask); for mode in MODES {
        for d in [GlaDirection::Forward,GlaDirection::Reverse,GlaDirection::Undirected] {
            for start in (0..4).map(VId) { for hi in 0..=3 { for lo in 0..=hi {
                assert_eq!(collect(&s,start,d,lo,hi,mode),oracle(&s,start,d,lo,hi,mode),"mask={mask}, mode={mode:?}, lo={lo}, hi={hi}");
            } } }
        }
    } }
}
#[test]
fn cycles_lower_bounds_closure_and_atom_local_history_do_not_become_vertex_settlement() {
    let s=Source::new([(1,0,1),(2,1,0),(3,1,2)]);
    for mode in [GraphWalkSearch::All,GraphWalkSearch::AllShortest,GraphWalkSearch::AnyShortest] {
        assert!(collect(&s,VId(0),GlaDirection::Forward,4,4,mode).contains(&VId(2)));
    }
    let s=Source::new([(u128::MAX,0,1)]);
    assert!(collect(&s,VId(0),GlaDirection::Undirected,2,3,GraphWalkSearch::Trail).is_empty());
    assert!(collect(&s,VId(0),GlaDirection::Undirected,2,3,GraphWalkSearch::Acyclic).is_empty());
    assert_eq!(collect(&s,VId(0),GlaDirection::Undirected,2,3,GraphWalkSearch::Simple),BTreeSet::from([VId(0)]));
    assert!(collect(&s,VId(0),GlaDirection::Undirected,3,4,GraphWalkSearch::Simple).is_empty());
    let s=Source::new([(1,0,1),(2,1,3),(3,0,2),(4,2,3),(5,3,1),(6,1,4)]);
    // The first prefix to 3 has already used 1; the other prefix has not.
    assert!(collect(&s,VId(0),GlaDirection::Forward,4,4,GraphWalkSearch::Acyclic).contains(&VId(4)));
}
fn plan(input:&str,mode:GraphWalkSearch)->GlaPlan<GraphValueRow> {
    let q=PreparedGraphText::prepare(input,|kind,name:&str|match (kind,name) {
        (GraphSymbolKind::Relation,"R")=>Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property,"p")=>Some(GraphSymbol::Property(P)),_=>None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    // Exercise all public search semantics on the compiler's unchanged scope,
    // binding and projection layout. No AST execution is used by either path.
    let mut ops=q.plan().operators().to_vec();
    for op in &mut ops { if let GlaOperator::VarLengthExpand{search,..}=op { *search=mode; } }
    GlaPlan::from_operators(ops)
}
fn eager(plan:&GlaPlan<GraphValueRow>,s:&Source)->Vec<GraphValueRow> {
    plan.execute_governed_with_element_properties((s.ids.len()+s.edges.len()) as u64,
        s.ids.iter().copied(),s.edges.iter().map(|(&id,&(a,b))|(id,a,R,b)),
        |v,predicates:&[VertexPredicate]|Ok::<_,()>(predicates.iter().all(|p|p.matches_borrowed([],s.properties.get(&v).into_iter().flatten().map(|(k,v)|(*k,v))))),
        |v,k|Ok(s.properties.get(&v).and_then(|p|p.iter().find(|(key,_)|*key==k)).map(|(_,v)|v)),
        |_,_|Ok(None),wide(),||Ok::<_,()>(())).unwrap().value
}
#[test]
fn variable_atoms_compose_with_fixed_edges_branches_predicates_and_backtracking_in_both_stream_lanes() {
    let bodies=[
        "MATCH (b)-[:R*0..3]->(x) WHERE x.p > 5",
        "MATCH (b)-[:R*1..2]->(x)-[:R*0..2]->(y) WHERE y = a",
        "MATCH (b)-[:R*0..2]->(x), (b)-[:R*1..2]->(y) WHERE x <> y",
        "MATCH (b)-[:R*0..2]->(x)-[:R]->(a) WHERE x.p IS NULL",
    ];
    for mode in MODES { for mask in [0,15,31,63] { for body in bodies { for anti in [false,true] {
        let input=format!("MATCH (a)-[r:R]->(b) WHERE {}EXISTS {{ {body} }} RETURN r,a,b",if anti{"NOT "}else{""});
        let p=plan(&input,mode); let s=Source::small(mask); let want=eager(&p,&s);
        let got=EdgeScanCursor::new(s,EdgeScanPlan::compile(&p).unwrap(),wide(),||Ok::<_,()>(()));
        assert_eq!(got.collect::<Result<Vec<_>,_>>().unwrap(),want,"{input} {mode:?}");
        // Vertex compilation must admit the same scope, retaining isolates.
        let p=plan(&format!("MATCH (b) WHERE {}EXISTS {{ MATCH (b)-[:R*0..3]->(x) WHERE x.p > 5 }} RETURN b",if anti{"NOT "}else{""}),mode);
        assert!(VertexScanPlan::compile(&p).is_ok());
    } } } }
    // Reusing the same real edge in different atoms is legal even in TRAIL.
    let p=plan("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R*1..1]-(x)-[:R*1..1]-(b) } RETURN r,a,b",GraphWalkSearch::Trail);
    let want=eager(&p,&Source::new([(0,0,1)])); assert!(!want.is_empty());
    assert_eq!(EdgeScanCursor::new(Source::new([(0,0,1)]),EdgeScanPlan::compile(&p).unwrap(),wide(),||Ok::<_,()>(())).collect::<Result<Vec<_>,_>>().unwrap(),want);
}
#[test]
fn unrestricted_absence_coalesces_exponential_parallel_walks_and_positive_existence_stops_early() {
    for mode in [GraphWalkSearch::All,GraphWalkSearch::AllShortest,GraphWalkSearch::AnyShortest] {
        let p=plan("MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*40..40]->(x) WHERE x = a } RETURN r,a,b LIMIT 1",mode);
        let s=Source::new(std::iter::once((0,9999,0)).chain((0..40).flat_map(|i|[(2*i+1,i,i+1),(2*i+2,i,i+1)])));
        let mut c=EdgeScanCursor::new(s,EdgeScanPlan::compile(&p).unwrap(),GqlQueryPolicy::new(81,1,100_000,10_000),||Ok::<_,()>(()));
        assert_eq!(c.next().unwrap().unwrap().values()[0],GraphValue::Edge(EId(0)));
        assert_eq!(c.row_stats().snapshot_records,81); assert!(c.next().is_none());
    }
    let p=plan("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R*40..40]->(x) } RETURN r,a,b LIMIT 1",GraphWalkSearch::All);
    let mut s=Source::new(std::iter::once((0,9999,0)).chain((0..40).flat_map(|i|[(2*i+1,i,i+1),(2*i+2,i,i+1)])));
    s.bad=Some(EId(2));
    let mut c=EdgeScanCursor::new(s,EdgeScanPlan::compile(&p).unwrap(),GqlQueryPolicy::new(41,1,100_000,10_000),||Ok::<_,()>(()));
    assert!(c.next().unwrap().is_ok()); assert_eq!(c.row_stats().snapshot_records,41);
    assert!(c.next().is_none());
}
#[test]
fn all_cancellation_sites_and_exact_limits_are_terminal_and_retryable() {
    for mode in MODES {
        let p=plan("MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*2..3]->(x) WHERE x.p > 50 } RETURN r,a,b LIMIT 2",mode);
        let scan=EdgeScanPlan::compile(&p).unwrap(); let calls=Cell::new(0);
        let mut c=EdgeScanCursor::new(Source::small(63),scan.clone(),wide(),|| {calls.set(calls.get()+1);Ok::<_,usize>(())});
        let want=c.by_ref().collect::<Result<Vec<_>,_>>().unwrap(); let rows=c.row_stats();let stats=c.evaluator_stats();drop(c);
        for stop in 1..=calls.get() {
            let at=Cell::new(0);let mut c=EdgeScanCursor::new(Source::small(63),scan.clone(),wide(),|| {
                at.set(at.get()+1);if at.get()==stop{Err(stop)}else{Ok(())}
            });
            let mut prefix=Vec::new();
            loop { match c.next() { Some(Ok(row))=>prefix.push(row), Some(Err(GqlQueryError::Interrupted(n)))=>{assert_eq!(n,stop);break;},
                other=>panic!("interruption became {other:?}") } }
            assert_eq!(prefix,want[..prefix.len()]);assert_eq!(c.row_stats().result_rows,prefix.len() as u64);
            assert_eq!(c.state(),EdgeScanState::Failed);assert!(c.next().is_none());assert_eq!(at.get(),stop);
        }
        let exact=GqlQueryPolicy::new(rows.snapshot_records,rows.result_rows,stats.work_units,stats.scratch_entries);
        let run=|policy|EdgeScanCursor::new(Source::small(63),scan.clone(),policy,||Ok::<_,()>(())).collect::<Result<Vec<_>,_>>();
        assert_eq!(run(exact).unwrap(),want);
        for policy in [GqlQueryPolicy::new(rows.snapshot_records-1,u64::MAX,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,rows.result_rows-1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,u64::MAX,stats.work_units-1,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,u64::MAX,u64::MAX,stats.scratch_entries-1)] { assert!(run(policy).is_err()); }
        assert_eq!(run(exact).unwrap(),want);
    }
}
#[test]
fn zero_hops_null_anchors_and_broken_sources_cannot_fabricate_anti_join_success() {
    let p=plan("MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*1..3]->(x) WHERE x.p > 50 } RETURN r,a,b",GraphWalkSearch::All);
    for fault in 0..3 {
        let mut s=Source::small(63);s.bad=(fault==0).then_some(EId(1));s.repeat=fault==1;s.unavailable=fault==2;
        let mut c=EdgeScanCursor::new(s,EdgeScanPlan::compile(&p).unwrap(),wide(),||Ok::<_,()>(()));
        assert!(c.next().unwrap().is_err());assert_eq!(c.row_stats().result_rows,0);assert!(c.next().is_none());
    }
    let s=Source::new([]);
    for mode in MODES { assert_eq!(collect(&s,VId(3),GlaDirection::Forward,0,0,mode),BTreeSet::from([VId(3)])); }
    let p=plan("MATCH (a) WHERE EXISTS { MATCH (a)-[:R*0..0]->(x) } RETURN a",GraphWalkSearch::All);
    let at=p.operators().iter().position(|op|matches!(op,GlaOperator::Probe{..})).unwrap();
    let (probe,_)=Probe::compile(p.operators(),at,1).unwrap();
    assert!(!probe.accepts(&[None],&s,&mut |_|Ok::<_,GqlQueryError<EdgeScanError<&'static str>,()>>(()),&mut ||Ok(())).unwrap());
}
#[test]
fn maximum_depth_and_full_width_edge_ids_need_no_recursion_or_identity_arithmetic() {
    let hi=crate::MAX_GRAPH_WALK_HOPS;
    let s=Source::new((0..u128::from(hi)).map(|i|(u128::MAX-i,i,i+1)));
    for mode in MODES {
        let ends=collect(&s,VId(0),GlaDirection::Forward,hi,hi,mode);
        assert_eq!(ends,BTreeSet::from([VId(u128::from(hi))]));
    }
}
