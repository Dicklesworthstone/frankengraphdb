//! Current maintained selection compared with independent complete storage reads.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZSet};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GqlScalarParameter, GraphSetOperand,
    GraphSetPredicateOp, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, IntegerComparison, PreparedGraphPattern};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphValue>, i128>;
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0xa1;32], DatabaseSecurityNamespaceId([0xa2;32]), [0xa3;32]) }
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,100_000,20_000_000,20_000_000) }
fn bounded(rows: u64) -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,rows,20_000_000,20_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind,name) { (GraphSymbolKind::Property,"p") => Some(GraphSymbol::Property(P)), _ => None }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text,symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn gt(column: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare { left: GraphSetOperand::Column(column), comparison: IntegerComparison::Greater,
        right: GraphSetOperand::Literal(GqlScalarParameter::new(CanonicalScalar::Int(0)).unwrap()) }
}
fn add(batch: &mut WriteBatch, id: u128, value: Option<CanonicalScalar>) {
    batch.create_vertex(VId(id),vec![],value.map(|v| vec![(P,v)]).unwrap_or_default());
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter().map(|(r,w)| (r.values().to_vec(),w.to_i128().unwrap())).collect()
}
fn difference(new: &Bag, old: &Bag) -> Bag {
    let mut delta=new.clone();
    for (r,w) in old { *delta.entry(r.clone()).or_default()-=w; }
    delta.retain(|_,w| *w!=0); delta
}
fn expected(db: &Database<MemVfs>, mode: usize) -> Bag {
    let mut out=Bag::new();
    for v in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let value=v.props.iter().find(|(p,_)| *p==P).map_or(CanonicalScalar::Null,|(_,v)|v.clone());
        let keep=match mode {
            0 => matches!(&value,CanonicalScalar::Int(n) if *n>0),
            1 => matches!(&value,CanonicalScalar::Int(n) if *n<=0),
            _ => matches!(&value,CanonicalScalar::Null),
        };
        if keep { *out.entry(vec![GraphValue::Scalar(value)]).or_default()+=1; }
    }
    out
}

#[test]
fn filters_follow_mixed_commits_and_compose_with_shared_sets_and_joins() {
    let ((),report)=run_async_under_lab(0x464c_0101,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));
        for (id,value) in [(1,Some(CanonicalScalar::Int(2))),(2,Some(CanonicalScalar::Int(2))),
            (3,Some(CanonicalScalar::Int(-1))),(4,None),(5,Some(CanonicalScalar::Bool(true))),
            (u128::MAX,Some(CanonicalScalar::Int(3)))] { add(&mut seed,id,value); }
        db.write(&commit,seed).await.unwrap();
        let source=db.register_standing_rows(&cx,pattern("MATCH (n) RETURN n.p AS p"),policy()).unwrap();
        let codes=[vec![gt(0)],vec![gt(0),GraphSetPredicateOp::Not],vec![GraphSetPredicateOp::IsNull {
            operand:GraphSetOperand::Column(0),is_null:true }]];
        let filters:Vec<_>=codes.iter().map(|code|db.register_standing_filter(&cx,&source,code,policy()).unwrap()).collect();
        let duplicate=db.register_standing_set(&cx,&filters[0],&filters[0],SetOperation::UnionAll,policy()).unwrap();
        let again=db.register_standing_filter(&cx,&duplicate,&[gt(0)],policy()).unwrap();
        let join=db.register_standing_join(&cx,&again,&filters[0],&[(0,0)],policy()).unwrap();
        let filtered_join=db.register_standing_filter(&cx,&join,&[gt(1)],policy()).unwrap();
        let mut previous:Vec<_>=filters.iter().map(|h|plain(db.standing_filter(&cx,h).unwrap().rows())).collect();
        for h in &filters { assert!(db.standing_filter_delta(&cx,h).unwrap().is_none()); }
        for step in 0..5 {
            let mut change=WriteBatch::new(RelationId(1));
            match step {
                0 => { change.set_vertex_property(VId(2),P,Some(CanonicalScalar::Int(-1)));
                    change.set_vertex_property(VId(3),P,Some(CanonicalScalar::Int(2))); }
                1 => { change.set_vertex_property(VId(4),P,Some(CanonicalScalar::Int(2)));
                    change.set_vertex_property(VId(5),P,None); }
                2 => { change.delete_vertex(VId(1)); change.delete_vertex(VId(u128::MAX)); }
                3 => { add(&mut change,10,Some(CanonicalScalar::Int(4))); }
                _ => { change.set_vertex_property(VId(10),PropertyKeyId(99),Some(CanonicalScalar::Int(1))); }
            }
            let at=db.write(&commit,change).await.unwrap();
            for (mode,h) in filters.iter().enumerate() {
                let wanted=expected(&db,mode); let view=db.standing_filter(&cx,h).unwrap();
                assert_eq!(view.frontier(),at); assert_eq!(plain(view.rows()),wanted);
                assert!(view.ordered_rows().is_none());
                assert_eq!(plain(db.standing_filter_delta(&cx,h).unwrap().unwrap().rows()),difference(&wanted,&previous[mode]));
                assert_eq!(db.standing_filter_total(&cx,h).unwrap().to_i128(),Some(wanted.values().sum()));
                assert_eq!(db.standing_filter_columns(&cx,h).unwrap(),&["p"]);
                previous[mode]=wanted;
            }
            let wanted:Bag=expected(&db,0).into_iter().map(|(r,w)|([r.clone(),r].concat(),2*w*w)).collect();
            assert_eq!(plain(db.standing_filter(&cx,&filtered_join).unwrap().rows()),wanted);
        }
        assert_eq!(db.standing_filter(&cx,&again).unwrap().last_maintenance().delta_rows,0);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn final_swaps_fit_quotas_and_failed_views_rebuild_without_undoing_commits() {
    let ((),report)=run_async_under_lab(0x464c_0102,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1)); add(&mut seed,1,Some(CanonicalScalar::Int(2)));
        add(&mut seed,2,Some(CanonicalScalar::Int(-1))); db.write(&commit,seed).await.unwrap();
        let source=db.register_standing_rows(&cx,pattern("MATCH (n) RETURN n.p AS p"),policy()).unwrap();
        let filter=db.register_standing_filter(&cx,&source,&[gt(0)],bounded(1)).unwrap();
        let child=db.register_standing_filter(&cx,&filter,&[gt(0)],policy()).unwrap();
        let sibling=db.register_standing_filter(&cx,&source,&[GraphSetPredicateOp::Truth(Some(false))],bounded(0)).unwrap();
        let mut swap=WriteBatch::new(RelationId(1));
        swap.set_vertex_property(VId(1),P,Some(CanonicalScalar::Int(-1)));
        swap.set_vertex_property(VId(2),P,Some(CanonicalScalar::Int(1)));
        let before=db.write(&commit,swap).await.unwrap();
        assert_eq!(db.standing_filter_total(&cx,&filter).unwrap().to_i128(),Some(1));
        let mut grow=WriteBatch::new(RelationId(1)); grow.set_vertex_property(VId(1),P,Some(CanonicalScalar::Int(3)));
        let at=db.write(&commit,grow).await.unwrap(); assert!(at>before);
        assert!(matches!(db.standing_filter(&cx,&filter),Err(StandingQueryError::Unavailable {
            frontier,reason:StandingQueryFailure::ResultBudget }) if frontier==before));
        assert!(matches!(db.standing_filter(&cx,&child),Err(StandingQueryError::Unavailable {
            reason:StandingQueryFailure::DependencyUnavailable,.. })));
        assert_eq!(db.standing_filter(&cx,&sibling).unwrap().frontier(),at);
        assert_eq!(db.standing_rows(&cx,&source).unwrap().frontier(),at);
        assert!(db.rebuild_standing_query(&cx,&filter,bounded(1)).is_err());
        assert!(matches!(db.standing_filter(&cx,&filter),Err(StandingQueryError::Unavailable {frontier,..}) if frontier==before));
        db.rebuild_standing_query(&cx,&filter,bounded(2)).unwrap();
        assert!(db.standing_filter_delta(&cx,&filter).unwrap().is_none());
        db.rebuild_standing_query(&cx,&child,policy()).unwrap();
        assert_eq!(plain(db.standing_filter(&cx,&child).unwrap().rows()),expected(&db,0));
        let mut change=WriteBatch::new(RelationId(1)); change.delete_vertex(VId(1));
        db.write(&commit,change).await.unwrap();
        assert_eq!(db.standing_filter_total(&cx,&child).unwrap().to_i128(),Some(1));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn schema_owner_and_snapshot_limits_survive_compaction_and_reopen() {
    let ((),report)=run_async_under_lab(0x464c_0103,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let vfs=MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let source=db.register_standing_rows(&cx,pattern("MATCH (n) RETURN n AS a, n AS b"),policy()).unwrap();
        assert!(matches!(db.register_standing_filter(&cx,&source,&[gt(0)],policy()),Err(StandingQueryError::FilterSchema(_))));
        let equal=GraphSetPredicateOp::Compare {left:GraphSetOperand::Column(0),comparison:IntegerComparison::Equal,
            right:GraphSetOperand::Column(1)};
        let filter=db.register_standing_filter(&cx,&source,&[equal.clone()],policy()).unwrap();
        let mut seed=WriteBatch::new(RelationId(1)); add(&mut seed,0,None); add(&mut seed,u128::MAX,None);
        db.write(&commit,seed).await.unwrap();
        let wanted=plain(db.standing_filter(&cx,&filter).unwrap().rows());
        assert_eq!(wanted.len(),2);
        assert!(matches!(db.register_standing_filter(&cx,&source,&[equal.clone()],
            GqlQueryPolicy::new(1,100,20_000_000,20_000_000)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        assert!(matches!(db.standing_filter(&cx,&source),Err(StandingQueryError::Unsupported)));
        let other=Database::open_memory(&commit,keys()).await.unwrap();
        assert!(matches!(other.standing_filter(&cx,&filter),Err(StandingQueryError::ForeignHandle)));
        db.compact(&commit).await.unwrap(); assert_eq!(plain(db.standing_filter(&cx,&filter).unwrap().rows()),wanted);
        drop(db);
        let mut db=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert!(matches!(db.standing_filter(&cx,&filter),Err(StandingQueryError::ForeignHandle)));
        let source=db.register_standing_rows(&cx,pattern("MATCH (n) RETURN n AS a, n AS b"),policy()).unwrap();
        let filter=db.register_standing_filter(&cx,&source,&[equal],policy()).unwrap();
        assert_eq!(plain(db.standing_filter(&cx,&filter).unwrap().rows()),wanted);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn filter_maintenance_does_not_traverse_unchanged_parent_results() {
    let ((),report)=run_async_under_lab(0x464c_0104,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let mut work=Vec::new();
        for size in [2,1024] {
            let mut db=Database::open_memory(&commit,keys()).await.unwrap();
            let mut seed=WriteBatch::new(RelationId(1));
            for id in 1..=size { add(&mut seed,id,Some(CanonicalScalar::Int(id as i64))); }
            db.write(&commit,seed).await.unwrap();
            let source=db.register_standing_rows(&cx,pattern("MATCH (n) RETURN n.p AS p"),policy()).unwrap();
            let filter=db.register_standing_filter(&cx,&source,&[gt(0)],policy()).unwrap();
            let mut change=WriteBatch::new(RelationId(1)); change.set_vertex_property(VId(1),P,Some(CanonicalScalar::Int(-1)));
            db.write(&commit,change).await.unwrap();
            let stats=*db.standing_filter(&cx,&filter).unwrap().last_maintenance();
            assert_eq!(stats.delta_rows,2); work.push((stats.work_units,stats.scratch_entries));
        }
        assert_eq!(work[0],work[1]);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
