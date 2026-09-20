//! Bound native set trees exercise the production compiler and native delivery.
//! No text grammar is added: preparation already creates these exact nodes.
use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as Op,
    GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000,10_000,1_000_000,1_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x41;32],DatabaseSecurityNamespaceId([0x42;32]),[0x43;32]) }
fn leaf() -> PreparedGraphSet {
    PreparedGraphSet::from(PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", |kind,name:&str| {
        match (kind,name) { (GraphSymbolKind::Property,"p")=>Some(GraphSymbol::Property(P)),_=>None }
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap())
}
fn computed(quantifier: GraphSetQuantifier) -> PreparedGraphSet {
    let sum=leaf().combine(GraphSetOperation::Union,GraphSetQuantifier::All,leaf()).unwrap();
    let parity=GraphIntegerExpression::prepare(&[Op::Column(0),Op::Literal(Some(2)),Op::Binary(Binary::Remainder)]).unwrap();
    let mapped=sum.project(vec![GraphSetProjection::new("p",GraphSetValue::Integer(parity))],quantifier).unwrap();
    let shifted=GraphIntegerExpression::prepare(&[Op::Column(0),Op::Literal(Some(10)),Op::Binary(Binary::Add)]).unwrap();
    mapped.nested().unwrap().project(vec![GraphSetProjection::new("computed",GraphSetValue::Integer(shifted))],
        GraphSetQuantifier::All).unwrap()
}
fn expected<V:Vfs+Clone>(db:&Database<V>,q:GraphSetQuantifier)->(CommitSeq,QueryResult) {
    let at=db.frontier().unwrap();let mut values=Vec::new();
    for row in db.vertices_at(at).unwrap() {
        let value=match row.props.iter().find(|(key,_)|*key==P).map(|(_,v)|v) {
            Some(CanonicalScalar::Int(n))=>CanonicalScalar::Int(n%2+10),
            None|Some(CanonicalScalar::Null)=>CanonicalScalar::Null,
            _=>unreachable!("fixture scalar domain"),
        };
        values.push(GraphValue::Scalar(value.clone()));values.push(GraphValue::Scalar(value));
    }
    values.sort();if q==GraphSetQuantifier::Distinct {values.dedup();}
    (at,QueryResult::Rows {columns:vec!["computed".into()],rows:values.into_iter()
        .map(|v|vec![QueryValue::Value(v)]).collect()})
}

#[test]
fn compiled_projection_set_trees_deliver_exact_native_values_after_each_commit() {
    let ((),report)=run_async_under_lab(0x7072_6420,|root|async move {
        let c=PurposeContexts::narrow_runtime_root(&root);let cx=c.query();let commit=c.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));
        for (id,n) in [(1,Some(0)),(2,Some(2)),(3,Some(3)),(u128::MAX,None)] {
            seed.create_vertex(VId(id),vec![],n.map(|n|vec![(P,CanonicalScalar::Int(n))]).unwrap_or_default());
        }
        db.write(&commit,seed).await.unwrap();
        let all=register(&mut db,&cx,&computed(GraphSetQuantifier::All),policy()).unwrap();
        let distinct=register(&mut db,&cx,&computed(GraphSetQuantifier::Distinct),policy()).unwrap();
        for tick in 0..4 {
            for (handle,q) in [(&all,GraphSetQuantifier::All),(&distinct,GraphSetQuantifier::Distinct)] {
                assert_eq!(db.standing_native_query(&cx,handle,GqlQueryPolicy::new(0,100,1_000_000,1_000_000)).unwrap(),expected(&db,q));
                assert_eq!(db.standing_native_columns(&cx,handle).unwrap(),&["computed"]);
            }
            let mut update=WriteBatch::new(RelationId(1));
            match tick {
                0=>{update.set_vertex_property(VId(2),P,Some(CanonicalScalar::Int(5)));}
                1=>{update.delete_vertex(VId(1));update.set_vertex_property(VId(u128::MAX),P,Some(CanonicalScalar::Int(8)));}
                2=>{update.create_vertex(VId(99),vec![],vec![(P,CanonicalScalar::Int(0))]);}
                _=>{update.delete_vertex(VId(3));}
            }
            db.write(&commit,update).await.unwrap();
        }
        for handle in [&all,&distinct] {db.rebuild_standing_query(&cx,handle,policy()).unwrap();}
        let mut update=WriteBatch::new(RelationId(1));update.delete_vertex(VId(2));
        db.write(&commit,update).await.unwrap();
        assert_eq!(db.standing_native_query(&cx,&all,policy()).unwrap(),expected(&db,GraphSetQuantifier::All));
        assert_eq!(db.standing_native_query(&cx,&distinct,policy()).unwrap(),expected(&db,GraphSetQuantifier::Distinct));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

type Saved=(usize,GqlQueryPolicy,CommitSeq,Option<StandingQueryFailure>,ZSet<GraphValueRow>);
fn saved(queries:&[StandingQuery])->Vec<Saved> {
    queries.iter().map(|query| {
        let rows=sets::rows(query).unwrap();let (policy,at,failure)=query.status();
        (std::ptr::from_ref(rows) as usize,policy,at,failure,
            rows.checked_clone(LimbLimit::new(4),&mut |_|Ok::<_,()>(())).unwrap())
    }).collect()
}
#[test]
fn every_projection_circuit_stage_refusal_preserves_old_nodes_and_rebuild_rebases_dependencies() {
    let ((),report)=run_async_under_lab(0x7072_6421,|root|async move {
        let c=PurposeContexts::narrow_runtime_root(&root);let cx=c.query();let commit=c.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));seed.create_vertex(VId(1),vec![],vec![(P,CanonicalScalar::Int(2))]);
        db.write(&commit,seed).await.unwrap();
        let sibling=register(&mut db,&cx,&leaf(),policy()).unwrap();
        let before=saved(&db.standing_queries);let definition=computed(GraphSetQuantifier::Distinct);
        let mut calls=0;
        {let mut staged=Staging::new(&mut db);staged.compile(&cx,&definition,policy(),&mut ||{calls+=1;Ok(())}).unwrap();}
        assert_eq!(saved(&db.standing_queries),before);
        for stop in 1..=calls {
            let mut seen=0;
            {let mut staged=Staging::new(&mut db);
                assert!(staged.compile(&cx,&definition,policy(),&mut ||{
                    seen+=1;if seen==stop {Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))} else {Ok(())}
                }).is_err());}
            assert_eq!(seen,stop);assert_eq!(saved(&db.standing_queries),before);
        }
        let handle=register(&mut db,&cx,&definition,policy()).unwrap();
        let first=match handle.native.as_deref().unwrap() {Layout::Circuit{first,..}=>*first,_=>unreachable!()};
        let mut calls=0;rebuild_checked(&mut db,&cx,first,handle.index,policy(),&mut ||{calls+=1;Ok(())}).unwrap();
        let accepted=saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen=0;
            assert!(rebuild_checked(&mut db,&cx,first,handle.index,policy(),&mut ||{
                seen+=1;if seen==stop {Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))}else{Ok(())}
            }).is_err());
            assert_eq!(seen,stop);assert_eq!(saved(&db.standing_queries),accepted);
        }
        assert!(db.rebuild_standing_query(&cx,&handle,GqlQueryPolicy::new(10_000,0,1_000_000,1_000_000)).is_err());
        assert_eq!(saved(&db.standing_queries),accepted);
        let mut update=WriteBatch::new(RelationId(1));update.set_vertex_property(VId(1),P,Some(CanonicalScalar::Int(3)));
        db.write(&commit,update).await.unwrap();
        assert_eq!(db.standing_native_query(&cx,&handle,policy()).unwrap(),expected(&db,GraphSetQuantifier::Distinct));
        db.standing_native_query(&cx,&sibling,policy()).unwrap();
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn unsupported_projection_descendants_and_late_expression_failures_leave_no_partial_registration() {
    let ((),report)=run_async_under_lab(0x7072_6422,|root|async move {
        let c=PurposeContexts::narrow_runtime_root(&root);let cx=c.query();let commit=c.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));seed.create_vertex(VId(1),vec![],vec![(P,CanonicalScalar::Int(0))]);
        db.write(&commit,seed).await.unwrap();
        let sibling=register(&mut db,&cx,&leaf(),policy()).unwrap();let before=saved(&db.standing_queries);
        let projected=computed(GraphSetQuantifier::All);
        for query in [projected.clone().with_page(0,Some(0)),
            leaf().combine(GraphSetOperation::Union,GraphSetQuantifier::All,projected.with_page(1,None)).unwrap()] {
            assert!(register(&mut db,&cx,&query,policy()).is_err());
            assert_eq!(saved(&db.standing_queries),before);
        }
        let divide=GraphIntegerExpression::prepare(&[Op::Literal(Some(1)),Op::Column(0),Op::Binary(Binary::Divide)]).unwrap();
        let query=leaf().project(vec![GraphSetProjection::new("reciprocal",GraphSetValue::Integer(divide))],GraphSetQuantifier::All).unwrap();
        assert!(matches!(register(&mut db,&cx,&query,policy()),Err(StandingQueryError::Maintenance(
            StandingQueryFailure::OutputExpression{column:0,..}))));
        assert_eq!(saved(&db.standing_queries),before);
        db.standing_native_query(&cx,&sibling,policy()).unwrap();
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
