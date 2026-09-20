//! Late reads must retain their call sequence, not just their final values.
use super::*;
use super::tests::pattern;
use crate::algebra::GraphValue;
use std::cell::RefCell;

#[derive(Debug, PartialEq, Eq)]
enum Read { Vertex(VId), Property(VId, PropertyKeyId) }
type Trace = Vec<Read>;
type Cells = Vec<Vec<GraphValue>>;

// Reuse the ordinary projection's borrowed sources, but keep its ordered call
// trace independently of the intersection control/event implementation.
fn execute<'a>(q: &GlaPlan<GraphValueRow>, topology: &BTreeMap<TopologyKey, Multiplicity>,
    values: &'a BTreeMap<VId, CanonicalScalar>, optimized: bool, stop: usize,
) -> (Result<Cells, VisitError<usize, ()>>, Trace) {
    let trace = RefCell::new(Vec::new());
    let mut rows = Vec::new();
    let test = |id, _: &[VertexPredicate]| {
        let mut trace = trace.borrow_mut(); trace.push(Read::Vertex(id));
        if trace.len() == stop { Err(GqlQueryError::Source(GraphAggregateError::Source(stop))) } else { Ok(true) }
    };
    let property = |id, key| {
        let mut trace = trace.borrow_mut(); trace.push(Read::Property(id, key));
        if trace.len() == stop { return Err(GqlQueryError::Source(GraphAggregateError::Source(stop))); }
        Ok(values.get(&id))
    };
    let control = |_| Ok::<_, VisitError<usize, ()>>(());
    let result = if optimized {
        visit_bindings(q, [], topology, test, property, control,
            |columns, ids, property, _| { rows.push(read_cells(columns,ids,property)?); Ok(()) })
    } else {
        q.visit_value_bindings([], topology.keys().copied(), test, property, control,
            |columns, ids, property, _| { rows.push(read_cells(columns,ids,property)?); Ok(()) })
    };
    (result.map(|()| rows), trace.into_inner())
}

fn read_cells<'a, E>(
    columns: &[ValueProjection], ids: &[Option<VId>],
    property: &mut impl FnMut(VId,PropertyKeyId)->Result<Option<&'a CanonicalScalar>,E>,
) -> Result<Vec<GraphValue>,E> {
    let mut cells = Vec::new();
    for column in columns {
        match column {
            ValueProjection::Vertex { slot } => cells.push(ids[slot.ordinal() as usize]
                .map_or(GraphValue::Scalar(CanonicalScalar::Null),GraphValue::Vertex)),
            ValueProjection::Property { slot,key } => {
                let value = match ids[slot.ordinal() as usize] {
                    Some(id) => property(id,*key)?, None => None,
                };
                cells.push(GraphValue::Scalar(value.cloned().unwrap_or(CanonicalScalar::Null)));
            }
            _ => panic!("property fixture"),
        }
    }
    Ok(cells)
}

fn topology(mask: usize, direction: GlaDirection) -> BTreeMap<TopologyKey, Multiplicity> {
    let mut result = BTreeMap::new();
    for a in 0..3 { for b in 0..3 {
        if mask & (1 << (a*3+b)) != 0 {
            result.insert(normalized(VId(a), RelationId(1), VId(b), direction == GlaDirection::Undirected), Multiplicity::ONE);
        }
    }}
    result
}

#[test]
fn late_properties_and_boolean_reads_have_native_order_on_every_complete_binding() {
    let values = BTreeMap::from([(VId(0), CanonicalScalar::Int(-1)), (VId(1), CanonicalScalar::Null)]);
    for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        let arrow = match direction { GlaDirection::Forward => "-[:R]->", GlaDirection::Reverse => "<-[:R]-", _ => "-[:R]-" };
        for predicate in ["", "WHERE a.p=b.p", "WHERE a.p<>c.p",
            "WHERE NOT (a.p=b.p) OR c.p IS NULL", "WHERE a<>b AND b.p=c.p"] {
            let q = pattern(&format!("MATCH (a){arrow}(b){arrow}(c){arrow}(a) {predicate} RETURN c.p,a,b.p,a.p"));
            assert!(Shape::compile(q.plan()).is_some(), "{predicate}");
            for mask in 0..512 {
                let graph = topology(mask,direction);
                // Do not sort either result: callback visitation order itself
                // is required to agree even when projections reorder the cells.
                assert_eq!(execute(q.plan(), &graph, &values, true, usize::MAX),
                    execute(q.plan(), &graph, &values, false, usize::MAX), "mask={mask} {predicate}");
            }
        }
    }
}

#[test]
fn every_property_source_failure_is_the_same_failure_after_the_same_reads() {
    let graph = topology(511,GlaDirection::Forward);
    let values = BTreeMap::from([(VId(0),CanonicalScalar::Int(1)),(VId(1),CanonicalScalar::Int(1))]);
    let q = pattern("MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.p=b.p OR c.p IS NULL RETURN a.p,c.p");
    let (_, reads) = execute(q.plan(), &graph, &values, false, usize::MAX);
    assert!(!reads.is_empty());
    for stop in 1..=reads.len() {
        let ordinary = execute(q.plan(), &graph, &values, false, stop);
        let fast = execute(q.plan(), &graph, &values, true, stop);
        assert_eq!(fast, ordinary);
        assert!(matches!(fast.0, Err(GqlQueryError::Source(GraphAggregateError::Source(at))) if at == stop));
        assert_eq!(fast.1.len(),stop);
    }
}

#[test]
fn interleaved_vertex_filters_keep_fallback_reads_even_when_the_cycle_never_closes() {
    let graph = BTreeMap::from([((VId(0),RelationId(1),VId(1)),Multiplicity::ONE),
        ((VId(1),RelationId(1),VId(2)),Multiplicity::ONE)]);
    for input in [
        "MATCH (a:L)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a.p,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE b.p=1 RETURN a.p,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE c.p IS NULL RETURN a.p,b,c",
    ] {
        let q = pattern(input);
        assert!(Shape::compile(q.plan()).is_none());
        let old = execute(q.plan(),&graph,&BTreeMap::new(),false,1);
        assert!(old.0.is_err(),"prefix callback must run before the later missing closing edge");
        assert_eq!(execute(q.plan(),&graph,&BTreeMap::new(),true,1),old);
    }
}

#[test]
fn changing_property_callbacks_are_not_cached_or_reordered_by_the_kernel() {
    let graph = topology(511,GlaDirection::Forward);
    let q = pattern("MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.p=b.p RETURN c.p,a.p");
    let values = [CanonicalScalar::Int(1),CanonicalScalar::Int(2),CanonicalScalar::Null];
    let run = |optimized| {
        let mut calls = 0;
        let mut answer = Vec::new();
        let property = |_: VId, _: PropertyKeyId| { calls += 1; Ok::<_,VisitError<(),()>>(Some(&values[(calls/2)%3])) };
        if optimized {
            visit_bindings(q.plan(),[],&graph,|_,_|Ok(true),property,|_|Ok(()),
                |columns,ids,property,_| { answer.push(read_cells(columns,ids,property)?); Ok(()) }).unwrap();
        } else {
            q.plan().visit_value_bindings([],graph.keys().copied(),|_,_|Ok(true),property,|_|Ok(()),
                |columns,ids,property,_| { answer.push(read_cells(columns,ids,property)?); Ok(()) }).unwrap();
        }
        (answer,calls)
    };
    assert_eq!(run(true),run(false));
}

#[test]
fn late_property_intersection_keeps_root_contiguity_and_prunes_sparse_wedges() {
    let n = 1024_u128;
    let mut graph = BTreeMap::new(); let mut values = BTreeMap::new();
    for i in 0..n {
        for (a,r,b) in [(i,1,n),(n,2,n+i+1),(n+i+1,3,i)] {
            graph.insert((VId(a),RelationId(r),VId(b)),Multiplicity::ONE);
        }
        values.insert(VId(i),CanonicalScalar::Int(i as i64));
        values.insert(VId(n+i+1),CanonicalScalar::Int(i as i64));
    }
    let q = pattern("MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a.p=c.p RETURN a,c.p");
    assert!(Shape::compile(q.plan()).is_some());
    let mut work = 0; let mut rows = Vec::new(); let mut reads = 0;
    visit_bindings(q.plan(),[],&graph,|_,_|Ok::<_,VisitError<(),()>>(true),
        |id,_| { reads += 1; Ok(values.get(&id)) },
        |event| { work += u64::from(event == GlaExecutionEvent::Work); assert!(work < 800_000); Ok(()) },
        |_,ids,_,_| { rows.push(ids[0].unwrap()); Ok(()) }).unwrap();
    assert_eq!(rows,(0..n).map(VId).collect::<Vec<_>>());
    assert_eq!(reads,2*n);
}

#[test]
fn every_late_read_and_projection_checkpoint_propagates_and_exact_work_limits_replay() {
    let q = pattern("MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.p=b.p RETURN c.p");
    let graph = topology(17 | 256,GlaDirection::Forward);
    let value = CanonicalScalar::Int(1);
    let mut baseline = Vec::new();
    let mut total = 0;
    let mut stats = crate::GlaExecutionStats::default();
    let limits = crate::GlaExecutionLimits::new(u64::MAX,u64::MAX);
    visit_bindings(q.plan(),[],&graph,|_,_|Ok::<_,VisitError<(),usize>>(true),
        |_,_|Ok(Some(&value)), |event| {
            total += 1;
            stats.charge_event(limits,event).map_err(GqlQueryError::Evaluator)
        }, |columns,ids,property,_| { baseline.push(read_cells(columns,ids,property)?); Ok(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0; let mut prefix = Vec::new();
        let result = visit_bindings(q.plan(),[],&graph,|_,_|Ok::<_,VisitError<(),usize>>(true),
            |_,_|Ok(Some(&value)), |_| {
                at += 1; if at == stop { Err(GqlQueryError::Interrupted(stop)) } else { Ok(()) }
            }, |columns,ids,property,_| { prefix.push(read_cells(columns,ids,property)?); Ok(()) });
        assert!(matches!(result,Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(prefix,baseline[..prefix.len()]);
        assert_eq!(at,stop);
    }
    for (work,scratch,ok) in [(stats.work_units,stats.scratch_entries,true),
        (stats.work_units-1,stats.scratch_entries,false), (stats.work_units,stats.scratch_entries-1,false)] {
        let mut current = crate::GlaExecutionStats::default();
        let mut result_rows = Vec::new();
        let result = visit_bindings(q.plan(),[],&graph,|_,_|Ok::<_,VisitError<(),usize>>(true),
            |_,_|Ok(Some(&value)), |event| current.charge_event(crate::GlaExecutionLimits::new(work,scratch),event)
                .map_err(GqlQueryError::Evaluator),
            |columns,ids,property,_| { result_rows.push(read_cells(columns,ids,property)?); Ok(()) });
        assert_eq!(result.is_ok(),ok);
        if ok { assert_eq!(result_rows,baseline); assert_eq!(current,stats); }
    }
}
