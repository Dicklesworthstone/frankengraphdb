//! Actual committed graphs through scoped edge aggregation, plus hostile sources.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, SchemaEpoch};
use fgdb_gql::algebra::{GlaDirection, GraphValue};
use fgdb_gql::{GraphAggregateValue, GraphExactAverage, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Error as Denied, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x86; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8601), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut g = Grant::read_only("main", 1000, QueryLimits {
        max_nodes: 1_000_000, max_work: 10_000_000, max_rows: 1000,
    });
    g.labels = Scope::only([LabelId(1)]);
    g.relations = Scope::only([RelationId(1)]);
    g.properties = Scope::only([P]);
    g
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10000, 1000, 10_000_000, 10_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x16; 32], NS, [0x26; 32])).await.unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, value, visible) in [(0, Some(7), true), (1, Some(3), true),
        (2, Some(999), false), (3, Some(7), true), (u128::MAX, None, true)]
    {
        let mut properties = Vec::new();
        if let Some(value) = value { properties.push((P, CanonicalScalar::Int(value))); }
        properties.push((H, CanonicalScalar::ucs_basic_text("hidden vertex").unwrap()));
        seed.create_vertex(VId(id), if visible { vec![LabelId(1), LabelId(99)] }
            else { vec![LabelId(99)] }, properties);
    }
    for (id, a, b, value) in [(10, 0, 1, Some(2)), (11, 0, 1, Some(4)),
        (12, 1, 3, Some(8)), (13, 3, 3, None), (14, 0, 2, Some(1000)),
        (15, 2, u128::MAX, Some(1000))]
    {
        let mut properties = Vec::new();
        if let Some(value) = value { properties.push((P, CanonicalScalar::Int(value))); }
        properties.push((H, CanonicalScalar::ucs_basic_text("hidden edge").unwrap()));
        seed.add_edge(EId(id), VId(a), VId(b), properties);
    }
    db.write(cx, seed).await.unwrap();
    let mut denied = WriteBatch::new(RelationId(2));
    denied.add_edge(EId(u128::MAX), VId(3), VId(u128::MAX), vec![(P, CanonicalScalar::Int(64))]);
    db.write(cx, denied).await.unwrap();
    db
}
fn scalar(value: i64) -> QueryValue { QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))) }
fn null() -> QueryValue { QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)) }
fn rows(cursor: &mut AuthorizedAggregateCursor<'_>) -> Vec<Vec<QueryValue>> {
    let slots = cursor.output_slots().to_vec();
    cursor.by_ref().map(|row| {
        let row = row.unwrap();
        slots.iter().map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => QueryValue::Value(row.keys()[at].clone()),
            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        }).collect()
    }).collect()
}
fn eager_rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result { QueryResult::Rows { rows, .. } => rows, _ => panic!("not a read") }
}
macro_rules! context {
    ($runtime:ident, $cx:ident, $commit:ident) => {
        let $runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let root = $runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let $cx = contexts.query();
        let $commit = contexts.commit();
    };
}

#[test]
fn scoped_edges_and_fixed_joins_aggregate_only_admitted_topology_and_fields() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit));
    let issuer = authority(); let token = issuer.issue_at(&grant(), 100).unwrap();
    let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
    let params = GqlParameters::new();
    use GraphAggregateValue::{Average, Count, Integer};
    for (text, expected) in [
        ("MATCH (a)-[e:R]->(b) WHERE e.hidden IS NULL RETURN COUNT(*) AS n,COUNT(e.p) AS present,SUM(e.p) AS total,AVG(e.p) AS average,MIN(e.p) AS lo,MAX(e.p) AS hi,COUNT(DISTINCT e.p) AS distinct_n,SUM(DISTINCT e.p) AS distinct_sum,AVG(DISTINCT e.p) AS distinct_avg,COUNT(e.hidden) AS hidden",
            vec![vec![Count(4), Count(3), Integer(14), Average(GraphExactAverage::new(14,3).unwrap()), scalar(2), scalar(8), Count(3), Integer(14), Average(GraphExactAverage::new(14,3).unwrap()), Count(0)]]),
        ("MATCH (a)-[e:R]->(b)-[f:R]->(c) RETURN COUNT(*) AS n,SUM(f.p) AS total", vec![vec![Count(4), Integer(16)]]),
        ("MATCH (a)-[e:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(x) } RETURN COUNT(*) AS n", vec![vec![Count(4)]]),
        ("MATCH (a)-[e:R]->(b) WHERE NOT (e.hidden = 55) RETURN COUNT(*) AS n", vec![vec![Count(0)]]),
        ("MATCH (a:H)-[e:R]->(b) RETURN COUNT(*) AS n", vec![vec![Count(0)]]),
        ("MATCH (a)-[e:R]->(b) RETURN SUM(e.hidden) AS total", vec![vec![null()]]),
        ("MATCH (a)-[e:S]->(b) RETURN COUNT(*) AS n", vec![vec![Count(0)]]),
        ("MATCH (a)-[e:R]->(b) RETURN a.p AS key,COUNT(*) AS n,SUM(e.p) AS total GROUP BY a.p HAVING COUNT(*) > 1 ORDER BY key DESC LIMIT 1", vec![vec![scalar(7),Count(3),Integer(6)]]),
    ] {
        let prepared = session.prepare(&cx, text, &params).unwrap();
        let eager = eager_rows(session.execute(&cx, &prepared, &params).unwrap());
        let mut cursor = session.stream_aggregate(&cx, &prepared, &params).unwrap();
        assert_eq!(rows(&mut cursor), expected, "{text}");
        assert_eq!(eager, expected, "eager: {text}");
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    }
    let low = GqlParameters::new().with_int64("wanted", 2).unwrap();
    let high = GqlParameters::new().with_int64("wanted", 999).unwrap();
    let prepared = session.prepare(&cx, "MATCH (a)-[e:R]->(b) WHERE e.p > $wanted RETURN COUNT(*) AS n", &low).unwrap();
    for (params, n) in [(&low,2),(&high,0),(&low,2)] {
        assert_eq!(rows(&mut session.stream_aggregate(&cx, &prepared, params).unwrap()), vec![vec![Count(n)]]);
    }
}

#[test]
fn orientation_parallel_edges_and_collection_profiles_keep_native_identity_order() {
    context!(runtime, cx, commit);
    let db = runtime.block_on(database(&commit));
    let issuer = authority(); let token = issuer.issue_at(&grant(),100).unwrap();
    let mut session = db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),||100).unwrap();
    let params = GqlParameters::new();
    for (pattern, count, sum) in [("(a)-[e:R]->(b)",4,14), ("(a)<-[e:R]-(b)",4,14), ("(a)-[e:R]-(b)",7,28)] {
        let prepared = session.prepare(&cx,&format!("MATCH {pattern} RETURN COUNT(*) AS n,SUM(e.p) AS total"),&params).unwrap();
        assert_eq!(rows(&mut session.stream_aggregate(&cx,&prepared,&params).unwrap()),
            vec![vec![QueryValue::Count(count),QueryValue::Integer(sum)]]);
    }
    // Leading EId/source keys prove the collection input order required by
    // the native edge plan, including a NULL property and a real self-loop.
    let text = "MATCH (a)-[e:R]->(b) RETURN e AS edge,a AS start,COLLECT(e.p) AS items,COLLECT(DISTINCT e.p) AS unique_items GROUP BY e,a";
    let prepared = session.prepare(&cx,text,&params).unwrap();
    let eager = eager_rows(session.execute(&cx,&prepared,&params).unwrap());
    let got = rows(&mut session.stream_aggregate(&cx,&prepared,&params).unwrap());
    assert_eq!(got,eager); assert_eq!(got.len(),4);
    for (at,(eid,start,value)) in [(10,0,Some(2)),(11,0,Some(4)),(12,1,Some(8)),(13,3,None)].into_iter().enumerate() {
        let list = QueryValue::Value(GraphValue::List(value.into_iter().map(|value| GraphValue::Scalar(CanonicalScalar::Int(value))).collect()));
        assert_eq!(got[at],vec![QueryValue::Value(GraphValue::Edge(EId(eid))),QueryValue::Value(GraphValue::Vertex(VId(start))),list.clone(),list]);
    }
}

#[test]
fn snapshot_pins_history_and_property_versions_survive_writer_compaction_and_drop() {
    context!(runtime, cx, commit);
    let mut db = runtime.block_on(database(&commit));
    let at = db.frontier().unwrap();
    let issuer = authority(); let token = issuer.issue_at(&grant(),100).unwrap();
    let params = GqlParameters::new();
    let calls = Cell::new(0);
    let mut session = db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),|| { calls.set(calls.get()+1);100 }).unwrap();
    let text = "MATCH (a)-[e:R]->(b) RETURN COUNT(*) AS n,SUM(e.p) AS total";
    let prepared = session.prepare(&cx,text,&params).unwrap();
    let mut cursor = session.stream_aggregate(&cx,&prepared,&params).unwrap();
    drop(prepared);
    let paused = calls.get();
    let mut changes = WriteBatch::new(RelationId(1));
    changes.set_vertex_label(VId(1),LabelId(1),false);
    changes.set_edge_property(EId(13),P,Some(CanonicalScalar::Int(16)));
    runtime.block_on(db.write(&commit,changes)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    assert_eq!(calls.get(),paused);
    assert_eq!(rows(&mut cursor),vec![vec![QueryValue::Count(4),QueryValue::Integer(14)]]);
    drop(cursor);
    let mut fresh = db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),||100).unwrap();
    let prepared = fresh.prepare(&cx,text,&params).unwrap();
    assert_eq!(rows(&mut fresh.stream_aggregate(&cx,&prepared,&params).unwrap()),vec![vec![QueryValue::Count(1),QueryValue::Integer(16)]]);
    let history = format!("MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN COUNT(*) AS n,SUM(e.p) AS total",at.0);
    let prepared = fresh.prepare(&cx,&history,&params).unwrap();
    drop(db);
    assert_eq!(rows(&mut fresh.stream_aggregate(&cx,&prepared,&params).unwrap()),vec![vec![QueryValue::Count(4),QueryValue::Integer(14)]]);
}

#[test]
fn source_and_signed_limits_refuse_without_partial_groups_even_under_zero_limit() {
    context!(runtime, cx, commit);
    let mut db = runtime.block_on(database(&commit));
    let issuer = authority(); let params = GqlParameters::new();
    let token = issuer.issue_at(&grant(),100).unwrap();
    let mut empty_budget = db.authorized_read_session(&cx,&issuer,&token,"main",symbols,GqlQueryPolicy::new(0,100,1_000_000,1_000_000),||100).unwrap();
    let text = "MATCH (a)-[e:R]->(b) RETURN COUNT(*) AS n";
    let prepared = empty_budget.prepare(&cx,text,&params).unwrap();
    { let mut cursor=empty_budget.stream_aggregate(&cx,&prepared,&params).unwrap(); cursor.close(); assert!(cursor.next().is_none()); }
    assert!(matches!(empty_budget.stream_aggregate(&cx,&prepared,&params).unwrap().next(),Some(Err(QueryError::EdgeAggregateStream(GqlQueryError::Rows(_))))));
    let mut g=grant();g.limits.max_rows=1;
    let limited=issuer.issue_at(&g,100).unwrap();
    let mut session=db.authorized_read_session(&cx,&issuer,&limited,"main",symbols,policy(),||100).unwrap();
    let prepared=session.prepare(&cx,"MATCH (a)-[e:R]->(b) RETURN a.p AS key,COUNT(*) AS n GROUP BY a.p",&params).unwrap();
    {
        let mut cursor=session.stream_aggregate(&cx,&prepared,&params).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        assert!(matches!(cursor.next(),Some(Err(QueryError::Authorization(Denied::LimitExceeded(LimitDimension::Rows))))));
        assert_eq!(cursor.state(),VertexScanState::Failed);assert!(cursor.next().is_none());
    }
    assert!(!session.is_closed());
    let mut change=WriteBatch::new(RelationId(1));
    change.set_edge_property(EId(13),P,Some(CanonicalScalar::Bool(true)));
    runtime.block_on(db.write(&commit,change)).unwrap();
    let mut fresh=db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),||100).unwrap();
    for suffix in [""," LIMIT 0"] {
        let prepared=fresh.prepare(&cx,&format!("MATCH (a)-[e:R]->(b) RETURN SUM(e.p) AS total{suffix}"),&params).unwrap();
        let mut cursor=fresh.stream_aggregate(&cx,&prepared,&params).unwrap();
        assert!(matches!(cursor.next(),Some(Err(QueryError::EdgeAggregateStream(GqlQueryError::Source(GraphAggregateError::NonIntegerSum{..}))))));
        assert!(cursor.next().is_none());
    }
}

#[test]
fn expiry_cuts_and_retirement_fuse_the_original_session_guard() {
    context!(runtime, cx, commit);
    let db=runtime.block_on(database(&commit));let issuer=authority();let token=issuer.issue_at(&grant(),100).unwrap();
    let args=GqlParameters::new();let active=Cell::new(false);let calls=Cell::new(0);let stop=Cell::new(usize::MAX);
    let clock=||{if active.get(){calls.set(calls.get()+1)};if active.get()&&calls.get()==stop.get(){1000}else{100}};
    let text="MATCH (a)-[e:R]->(b) RETURN a.p AS key,COUNT(*) AS n GROUP BY a.p";
    let expected={
        let mut session=db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),clock).unwrap();
        let prepared=session.prepare(&cx,text,&args).unwrap();active.set(true);
        session.stream_aggregate(&cx,&prepared,&args).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
    };
    active.set(false);let total=calls.get();assert!(total>0);
    for cut in [1,2,total/2,total.saturating_sub(1),total] {
        calls.set(0);stop.set(cut);
        let mut session=db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),clock).unwrap();
        let prepared=session.prepare(&cx,text,&args).unwrap();active.set(true);
        {
            match session.stream_aggregate(&cx,&prepared,&args) {
                Err(QueryError::Authorization(Denied::Expired))=>{},
                Ok(mut cursor)=>{
                    let mut prefix=Vec::new();loop{match cursor.next(){
                        Some(Ok(row))=>prefix.push(row),Some(Err(QueryError::Authorization(Denied::Expired)))=>break,
                        other=>panic!("expiry cut {cut}: {other:?}"),
                    }}
                    assert!(expected.starts_with(&prefix));assert_eq!(cursor.state(),VertexScanState::Failed);assert!(cursor.next().is_none());
                },
                other=>panic!("open cut {cut}: {other:?}"),
            };
        }
        active.set(false);assert!(session.is_closed());assert_eq!(calls.get(),cut);
    }
    let mut session=db.authorized_read_session(&cx,&issuer,&token,"main",symbols,policy(),||100).unwrap();
    let prepared=session.prepare(&cx,text,&args).unwrap();
    {let mut cursor=session.stream_aggregate(&cx,&prepared,&args).unwrap();assert!(cursor.next().unwrap().is_ok());issuer.retire();
        assert!(matches!(cursor.next(),Some(Err(QueryError::Authorization(Denied::AuthorityRetired)))));assert!(cursor.next().is_none());}
    assert!(session.is_closed());
}

struct NoReads;
impl VertexScanSource for NoReads {
    type Error=ReadError;
    fn snapshot_seq(&self)->CommitSeq{CommitSeq(0)}
    fn next_vertex<C>(&mut self,_:&mut impl FnMut(VertexScanEvent)->Result<(),C>)->Result<Option<VId>,VertexScanSourceError<ReadError,C>>{panic!("denied root read")}
    fn vertex<'a,C>(&'a self,_:VId,_:&mut impl FnMut(VertexScanEvent)->Result<(),C>)->Result<Option<VertexScanRow<'a>>,VertexScanSourceError<ReadError,C>>{panic!("denied vertex read")}
}
impl EdgeScanSource for NoReads {
    type Error=ReadError;
    fn snapshot_seq(&self)->CommitSeq{CommitSeq(0)}
    fn next_edge<C>(&mut self,_:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)->Result<Option<EId>,EdgeScanSourceError<ReadError,C>>{panic!("denied relation opened root index")}
    fn edge<'a,C>(&'a self,_:EId,_:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)->Result<Option<EdgeScanRow<'a>>,EdgeScanSourceError<ReadError,C>>{panic!("denied relation opened history")}
    fn vertex<'a,C>(&'a self,_:VId,_:&mut impl FnMut(GlaExecutionEvent)->Result<(),C>)->Result<Option<VertexScanRow<'a>>,EdgeScanSourceError<ReadError,C>>{panic!("denied relation opened endpoint")}
}
#[test]
fn denied_root_relations_open_no_directory_and_unscoped_access_refuses() {
    context!(runtime,cx,commit);let _=commit;
    let issuer=authority();let token=issuer.issue_at(&grant(),100).unwrap();
    let verified=issuer.verify_at(&token,"main",100).unwrap();
    let execution:Shared<'_>=Rc::new(RefCell::new(Execution::new(&cx,verified.begin_read_at("main",100).unwrap(),Box::new(||100))));
    let mut source=Source{edges:NoReads,vertices:ScopedSource{inner:NoReads,execution:Rc::clone(&execution)},after:None};
    let mut control=|_|execution.borrow_mut().checkpoint();
    assert!(source.next_edge_for_relation(RelationId(2),&mut control).unwrap().is_none());
    assert_eq!(execution.borrow().permit.usage().nodes,0);
    // The default raw incidence implementation is Unavailable, not EOF. The
    // scoped root must also refuse an accidental relation-less enumeration.
    assert!(matches!(source.next_incident_edge(VId(0),GlaDirection::Forward,None,&mut control),Err(EdgeExpansionSourceError::Unavailable)));
    assert!(matches!(source.next_edge(&mut control),Err(EdgeScanSourceError::Source(QueryError::Unsupported{..}))));
    issuer.retire();
    assert!(matches!(source.next_edge_for_relation(RelationId(2),&mut control),Err(EdgeScanSourceError::Control(QueryError::Authorization(Denied::AuthorityRetired)))));
}

#[test]
fn aggregate_edge_error_translation_keeps_terminal_and_arithmetic_causes() {
    for cause in [Denied::Expired,Denied::AuthorityRetired,Denied::ClockWentBackwards,Denied::LimitExceeded(LimitDimension::Nodes)] {
        assert!(matches!(error(GqlQueryError::Source(GraphAggregateError::Source(EdgeScanError::Source(QueryError::Authorization(cause))))),QueryError::Authorization(actual) if actual==cause));
    }
    assert!(matches!(error(GqlQueryError::Source(GraphAggregateError::NonIntegerSum{aggregate:2})),QueryError::EdgeAggregateStream(GqlQueryError::Source(GraphAggregateError::NonIntegerSum{aggregate:2}))));
    assert!(matches!(source_error(EdgeScanError::ExpansionUnavailable),QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable))));
}
