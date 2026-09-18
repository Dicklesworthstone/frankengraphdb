//! Incremental existential scopes use witness presence, never witness multiplicity.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, StandingQueryError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeSet;

const OWNER: LabelId = LabelId(1);
const AMOUNT: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x61;32], DatabaseSecurityNamespaceId([0x62;32]), [0x63;32]) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind,name) {
        (GraphSymbolKind::Relation,"R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label,"Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property,"amount") => Some(GraphSymbol::Property(AMOUNT)),
        _ => None,
    }
}
fn parsed(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text,symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}

#[test]
fn existential_standing_results_partition_roots_after_every_committed_transition() {
    let ((), report) = run_async_under_lab(0x8f11, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut queries = Vec::new();
        for direction in 0..3 {
            let pattern = match direction { 0 => "(a)-[:R]->(b)", 1 => "(a)<-[:R]-(b)", _ => "(a)-[:R]-(b)" };
            for anti in [false,true] {
                let not = if anti { "NOT " } else { "" };
                let query = parsed(&format!("MATCH (a:Owner) WHERE {not}EXISTS {{ MATCH {pattern} WHERE a.amount < b.amount }} RETURN a,COUNT(*) AS hits,SUM(a.amount) AS amount GROUP BY a"));
                let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
                queries.push((direction,anti,query,handle));
            }
        }
        for step in 0..11 {
            let mut batch = WriteBatch::new(R);
            match step {
                0 => {
                    for (id, amount) in [(1,Some(1)),(2,Some(3)),(3,None),(4,Some(4))] {
                        batch.create_vertex(VId(id),vec![OWNER],vec![(AMOUNT,amount.map_or(CanonicalScalar::Null,CanonicalScalar::Int))]);
                    }
                    for (id,src,dst) in [(1,1,2),(2,1,2),(3,2,4),(4,1,1)] {
                        batch.add_edge(EId(id),VId(src),VId(dst),vec![]);
                    }
                }
                1 => { batch.delete_edge(EId(1)); }
                2 => { batch.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(7))); }
                3 => { batch.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Int(9))); }
                4 => {
                    batch.delete_edge(EId(2));
                    batch.add_edge(EId(5),VId(1),VId(3),vec![]);
                    batch.set_vertex_property(VId(3),AMOUNT,Some(CanonicalScalar::Int(12)));
                }
                5 => { batch.set_vertex_label(VId(1),OWNER,false); }
                6 => { batch.set_vertex_label(VId(1),OWNER,true); }
                7 => { batch.delete_vertex(VId(3)); }
                8 => { batch.delete_vertex(VId(2)); }
                9 => { batch.delete_vertex(VId(1)); }
                10 => { batch.delete_vertex(VId(4)); }
                _ => unreachable!(),
            }
            let at = db.write(&commit,batch).await.unwrap();
            let vertices = db.vertices().unwrap(); let edges = db.edges().unwrap();
            let value = |vid| vertices.iter().find(|row| row.vid==vid).and_then(|row|
                row.props.iter().find_map(|(key,value)| match value {
                    CanonicalScalar::Int(n) if *key==AMOUNT => Some(*n), _ => None,
                }));
            let all: BTreeSet<_> = vertices.iter().filter(|row|row.labels.contains(&OWNER)).map(|row|row.vid).collect();
            let mut partitions = [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];
            for (direction,anti,query,handle) in &queries {
                let expected: BTreeSet<_> = all.iter().copied().filter(|root| {
                    let exists = edges.iter().any(|edge| {
                        if edge.entry.relation!=R { return false; }
                        let (src,dst) = (edge.entry.src,edge.entry.dst);
                        let other = match direction {
                            0 if src==*root => Some(dst), 1 if dst==*root => Some(src),
                            2 if src==*root => Some(dst), 2 if dst==*root => Some(src), _ => None,
                        };
                        matches!((value(*root),other.and_then(&value)), (Some(left),Some(right)) if left<right)
                    });
                    exists != *anti
                }).collect();
                let full = db.execute_graph_aggregate_governed(&cx,query,policy()).unwrap().value;
                let actual: BTreeSet<_> = full.iter().map(|row|row.keys()[0].as_vertex().unwrap()).collect();
                assert_eq!(actual,expected,"step={step} direction={direction} anti={anti}");
                assert!(partitions[*direction].is_disjoint(&actual));
                partitions[*direction].extend(actual);
                let view = db.standing_query(&cx,handle).unwrap();
                assert_eq!(view.frontier(),at); assert_eq!(view.rows().len(),full.len());
                for row in &full {
                    assert_eq!(row.get(0).unwrap().as_count(),Some(1));
                    assert_eq!(view.rows().weight(row),Some(&ZWeight::ONE));
                }
            }
            for partition in partitions { assert_eq!(partition,all); }
            if step==7 { db.compact(&commit).await.unwrap(); }
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn silent_witness_updates_advance_counts_and_last_deletion_flips_presence_once() {
    let ((), report) = run_async_under_lab(0x8f12, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let commit = contexts.commit(); let cx = contexts.query();
        let vfs=fgdb::MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let mut seed=WriteBatch::new(R);
        seed.create_vertex(VId(1),vec![OWNER],vec![(AMOUNT,CanonicalScalar::Int(5))]);
        // NULL property is still a real witness when no property predicate exists.
        seed.create_vertex(VId(2),vec![],vec![(AMOUNT,CanonicalScalar::Null)]);
        db.write(&commit,seed).await.unwrap();
        let mut queries=Vec::new();
        for anti in [false,true] {
            let not=if anti {"NOT "} else {""};
            let query=parsed(&format!("MATCH (a:Owner) WHERE {not}EXISTS {{ MATCH (a)-[:R]->(b) }} RETURN COUNT(*) AS hits,SUM(a.amount) AS total"));
            let handle=db.register_standing_query(&cx,query.clone(),policy()).unwrap();
            queries.push((anti,query,handle));
        }
        for step in 0..5 {
            let mut batch=WriteBatch::new(R);
            match step {
                0 => {batch.add_edge(EId(1),VId(1),VId(2),vec![]);}
                1 => {batch.add_edge(EId(2),VId(1),VId(2),vec![]);}
                2 => {batch.delete_edge(EId(1));}
                3 => {batch.delete_edge(EId(2));batch.add_edge(EId(3),VId(1),VId(2),vec![]);}
                _ => {batch.delete_edge(EId(3));}
            }
            let at=db.write(&commit,batch).await.unwrap();
            for (anti,query,handle) in &queries {
                let full=db.execute_graph_aggregate_governed(&cx,query,policy()).unwrap().value;
                assert_eq!(full[0].get(0).unwrap().as_count(),Some(u64::from((step<4)!=*anti)));
                let view=db.standing_query(&cx,handle).unwrap();
                assert_eq!(view.frontier(),at); assert_eq!(view.rows().weight(&full[0]),Some(&ZWeight::ONE));
                if step==3 {assert_eq!(view.last_maintenance().affected_edges,2);}
            }
        }
        drop(db);
        let mut reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        for (_,query,old) in queries {
            assert!(matches!(reopened.standing_query(&cx,&old),Err(StandingQueryError::ForeignHandle)));
            let fresh=reopened.register_standing_query(&cx,query.clone(),policy()).unwrap();
            let full=reopened.execute_graph_aggregate_governed(&cx,&query,policy()).unwrap().value;
            assert_eq!(reopened.standing_query(&cx,&fresh).unwrap().rows().weight(&full[0]),Some(&ZWeight::ONE));
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
