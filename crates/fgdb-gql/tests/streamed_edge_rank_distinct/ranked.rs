//! Top-K is a result-stage operation, after complete aggregation and HAVING.
use super::super::*;
use fgdb_gql::{GraphAggregate, GraphAggregateColumn as Column, GraphAggregateOrder as Order,
    GraphNullPlacement as Nulls, GraphExactAverage, GraphIntegerExpression, GraphIntegerOp as Op,
    GraphIntegerBinary as Binary, GraphSetProjection, GraphSetValue};

fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX,u64::MAX,u64::MAX,u64::MAX) }
fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(s.edges.len() as u64,
        s.vertices.keys().copied(), s.edges.iter().map(|(&id,(a,r,b,_))|(id,*a,*r,*b)),
        |id,tests| Ok::<_, ()>(tests.iter().all(|p|p.matches_borrowed([],s.vertices[&id].iter().map(|(k,v)|(*k,v))))),
        |id,key| Ok(s.vertices[&id].iter().find(|(k,_)|*k==key).map(|(_,v)|v)),
        |id,key| Ok(s.edges[&id].3.iter().find(|(k,_)|*k==key).map(|(_,v)|v)),
        wide(), ||Ok::<_, ()>(())).unwrap().value
}
fn definition(pattern: &str, hops:usize, offset:u64, count:Option<u64>, desc:bool, nulls:Nulls)
    -> PreparedGraphAggregate {
    let end=if hops==1 {"b"} else {"c"};
    let raw=prepare(&format!("MATCH {pattern} RETURN {end} AS key,COUNT(*) AS n,SUM(r.p) AS total,AVG(r.p) AS average GROUP BY {end}"));
    let value=raw.aggregates()[1].argument_column().unwrap();
    PreparedGraphAggregate::prepare(raw.input_pattern().clone(),raw.group_key_columns(),
        &[GraphAggregate::count_rows("n"),GraphAggregate::sum_int("total",value),GraphAggregate::average_int("average",value)],offset,count)
        .unwrap().with_result_clauses(&[],&[
            Order{column:Column::Aggregate(2),descending:desc,nulls},
            Order::descending(Column::Aggregate(0)),
        ]).unwrap()
}
// Complete oriented occurrences are enumerated without production plan slots,
// prefix pruning or heaps. Fractions here are small, so cross multiplication is
// an independent exact comparator. Full vertex IDs break the explicit ties.
fn oracle(s:&Source,dir:usize,hops:usize,offset:usize,count:usize,desc:bool,nulls:Nulls)
    ->Vec<GraphAggregateRow> {
    let edges:Vec<_>=s.edges.values().flat_map(|(a,r,b,p)|{
        let v=integer(p);
        if dir==1 {vec![(*b,*r,*a,v)]} else if dir==2 && a!=b {
            vec![(*a,*r,*b,v),(*b,*r,*a,v)]
        } else {vec![(*a,*r,*b,v)]}
    }).collect();
    let mut groups=BTreeMap::<VId,Vec<Option<i128>>>::new();
    for &(_,rel,end,value) in &edges {
        if rel!=R {continue;}
        if hops==1 {groups.entry(end).or_default().push(value);} else {
            for &(from,rel,to,_) in &edges {
                if from==end && rel==S {groups.entry(to).or_default().push(value);}
            }
        }
    }
    let mut groups:Vec<_>=groups.into_iter().map(|(key,bag)|{
        let n=bag.len() as u64;
        let present:Vec<_>=bag.into_iter().flatten().collect();
        let ratio=(!present.is_empty()).then(||(present.iter().sum::<i128>(),present.len() as u64));
        (key,n,ratio)
    }).collect();
    groups.sort_by(|a,b|{
        let order=match(a.2,b.2){
            (None,None)=>std::cmp::Ordering::Equal,
            (None,Some(_))=>if nulls==Nulls::First{std::cmp::Ordering::Less}else{std::cmp::Ordering::Greater},
            (Some(_),None)=>if nulls==Nulls::First{std::cmp::Ordering::Greater}else{std::cmp::Ordering::Less},
            (Some((a,da)),Some((b,db)))=>{
                let cmp=(a*i128::from(db)).cmp(&(b*i128::from(da)));
                if desc{cmp.reverse()}else{cmp}
            },
        };
        order.then_with(||b.1.cmp(&a.1)).then_with(||a.0.cmp(&b.0))
    });
    groups.into_iter().skip(offset).take(count).map(|(key,n,ratio)|{
        let total=sum(ratio.map(|r|r.0));
        let avg=ratio.map_or_else(||sum(None),|(s,n)|GraphAggregateValue::Average(GraphExactAverage::new(s,n).unwrap()));
        GraphAggregateRow::from_group_values(vec![GraphValue::Vertex(key)],
            vec![GraphAggregateValue::Count(n),total,avg])
    }).collect()
}

#[test]
fn ranked_groups_match_independent_fraction_oracle_and_eager_results() {
    for mask in 0..64 {
        for dir in 0..3 {
            let edge=|name,rel,end|match dir{0=>format!("-[{name}:{rel}]->({end})"),
                1=>format!("<-[{name}:{rel}]-({end})"),_=>format!("-[{name}:{rel}]-({end})")};
            for hops in 1..=2 {
                let mut pattern=format!("(a){}",edge("r","R","b"));
                if hops==2{pattern+=&edge("s","S","c");}
                for desc in [false,true] { for nulls in [Nulls::First,Nulls::Last] {
                    for (offset,count) in [(0,Some(0)),(0,Some(1)),(1,Some(2)),(0,None)] {
                        let q=definition(&pattern,hops,offset,count,desc,nulls);let before=q.canonical_bytes();
                        let s=source(mask);
                        let expected=oracle(&s,dir,hops,offset as usize,count.map_or(usize::MAX,|n|n as usize),desc,nulls);
                        assert_eq!(eager(&q,&s),expected);
                        let mut cursor=run(&q,s,wide());
                        assert_eq!(cursor.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),expected);
                        assert_eq!(cursor.row_stats().result_rows,expected.len() as u64);
                        assert_eq!(cursor.state(),EdgeScanState::Exhausted);assert!(cursor.next().is_none());
                        assert_eq!(q.canonical_bytes(),before);
                    }
                }}
            }
        }
    }
}

#[test]
fn hidden_order_terms_full_keys_and_projection_do_not_change_rank_or_bag_multiplicity() {
    let q=definition("(a)-[r:R]->(b)",1,0,Some(2),true,Nulls::Last);
    for q in [q.clone().with_key_output_columns(&[]).unwrap().with_aggregate_output_prefix(1).unwrap(),
        q.clone().with_key_output_columns(&[0,0]).unwrap(),
        q.with_output_projection(vec![
            GraphSetProjection::new("constant",GraphSetValue::Integer(GraphIntegerExpression::prepare(&[Op::Literal(Some(1))]).unwrap())),
        ]).unwrap()] {
        let s=source(63);let expected=eager(&q,&s);
        assert_eq!(run(&q,s,wide()).collect::<Result<Vec<_>,_>>().unwrap(),expected);
        assert!(!q.supports_incremental_maintenance());
    }
    // Negative full-range values and close fractions must not become floats.
    let q=definition("(a)-[r:R]->(b)",1,0,None,true,Nulls::First);
    let mut s=source(0);
    for (id,end,weight) in [(1,0,i64::MAX),(2,0,i64::MAX-1),(3,1,i64::MAX-1),(4,u128::MAX,i64::MIN)] {
        s.edges.insert(EId(id),(VId(0),R,VId(end),vec![(P,CanonicalScalar::Int(weight))]));
    }
    let rows=run(&q,s,wide()).collect::<Result<Vec<_>,_>>().unwrap();
    assert_eq!(rows.iter().map(|r|r.keys()[0].clone()).collect::<Vec<_>>(),
        vec![GraphValue::Vertex(VId(0)),GraphValue::Vertex(VId(1)),GraphValue::Vertex(VId(u128::MAX))]);
    assert_eq!(rows[0].values()[2].as_average().unwrap().denominator(),2);
    let q=prepare("MATCH (a)-[r:R]->(b) RETURN b,MIN(r) AS first GROUP BY b ORDER BY first DESC");
    let s=source(63);let expected=eager(&q,&s);
    assert_eq!(run(&q,s,wide()).collect::<Result<Vec<_>,_>>().unwrap(),expected);
}

#[test]
fn losing_and_zero_page_groups_still_evaluate_having_and_output_failures() {
    for count in [0,1] {
        let q=prepare(&format!("MATCH (a)-[r:R]->(b) RETURN 1/(COUNT(*)-1) AS value GROUP BY b ORDER BY COUNT(*) DESC LIMIT {count}"));
        let mut s=source(0);
        for (id,to) in [(1,0),(2,0),(3,1)]{s.edges.insert(EId(id),(VId(0),R,VId(to),vec![]));}
        let mut cursor=run(&q,s,wide());
        assert!(matches!(cursor.next(),Some(Err(GqlQueryError::Source(GraphAggregateError::OutputExpression{column:0,..})))));
        assert_eq!(cursor.row_stats().result_rows,0);assert!(cursor.next().is_none());
        let q=prepare(&format!("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n GROUP BY b HAVING MIN(r.p)>0 OR TRUE ORDER BY n DESC LIMIT {count}"));
        let mut s=source(0);
        s.edges.insert(EId(1),(VId(0),R,VId(0),vec![(P,CanonicalScalar::Int(1))]));
        s.edges.insert(EId(2),(VId(0),R,VId(1),vec![(P,CanonicalScalar::Bool(true))]));
        let mut cursor=run(&q,s,wide());
        assert!(matches!(cursor.next(),Some(Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving{..})))));
        assert_eq!(cursor.row_stats().result_rows,0);assert!(cursor.next().is_none());
    }
}

#[test]
fn every_ranked_checkpoint_and_exact_limits_preserve_only_completed_deliveries() {
    let q=definition("(a)-[r:R]->(b)-[s:S]->(c)",2,0,Some(2),true,Nulls::Last);
    let mut calls=0;
    let mut full=EdgeAggregateCursor::new(source(63),EdgeAggregatePlan::compile(&q).unwrap(),wide(),||{calls+=1;Ok::<_,usize>(())});
    let expected=full.by_ref().collect::<Result<Vec<_>,_>>().unwrap();
    let r=full.row_stats();let e=full.evaluator_stats();drop(full);assert!(!expected.is_empty());
    let exact=GqlQueryPolicy::new(r.snapshot_records,r.result_rows,e.work_units,e.scratch_entries);
    assert_eq!(run(&q,source(63),exact).collect::<Result<Vec<_>,_>>().unwrap(),expected);
    for stop in 1..=calls {
        let s=source(63);let drops=s.drops.clone();let mut seen=0;
        let mut cursor=EdgeAggregateCursor::new(s,EdgeAggregatePlan::compile(&q).unwrap(),exact,
            ||{seen+=1;if seen==stop{Err(stop)}else{Ok(())}});
        let mut delivered=Vec::new();
        loop{match cursor.next().expect("injected boundary must be reached"){
            Ok(row)=>delivered.push(row),Err(GqlQueryError::Interrupted(at))=>{assert_eq!(at,stop);break;},
            Err(error)=>panic!("unexpected refusal: {error:?}"),
        }}
        assert!(expected.starts_with(&delivered));assert_eq!(cursor.row_stats().result_rows,delivered.len() as u64);
        assert_eq!(cursor.state(),EdgeScanState::Failed);
        assert_eq!(drops.load(Ordering::SeqCst),1);assert!(cursor.next().is_none());drop(cursor);assert_eq!(seen,stop);
    }
    for p in [GqlQueryPolicy::new(r.snapshot_records-1,r.result_rows,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,r.result_rows-1,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,u64::MAX,e.work_units-1,u64::MAX),
        GqlQueryPolicy::new(u64::MAX,u64::MAX,u64::MAX,e.scratch_entries-1)] {
        let mut cursor=run(&q,source(63),p);assert!(cursor.by_ref().collect::<Result<Vec<_>,_>>().is_err());
        assert_eq!(cursor.state(),EdgeScanState::Failed);assert!(cursor.next().is_none());
    }
}

#[test]
fn ranked_tiny_page_does_not_charge_output_quota_for_offset_or_discarded_groups() {
    let q=prepare("MATCH (a)-[r:R]->(b) RETURN b,SUM(r.p) AS total GROUP BY b ORDER BY total DESC SKIP 3 LIMIT 1");
    let mut s=source(0);
    for id in 0..1024 {s.vertices.insert(VId(id),vec![]);s.edges.insert(EId(id),(VId(0),R,VId(id),vec![(P,CanonicalScalar::Int(id as i64))]));}
    let drops=s.drops.clone();let mut cursor=run(&q,s,GqlQueryPolicy::new(1024,1,u64::MAX,u64::MAX));
    let row=cursor.next().unwrap().unwrap();assert_eq!(row.keys(),&[GraphValue::Vertex(VId(1020))]);
    assert_eq!(row.values()[0].as_integer(),Some(1020));assert_eq!(drops.load(Ordering::SeqCst),1);
    assert_eq!(cursor.row_stats().result_rows,1);assert!(cursor.next().is_none());
    for text in ["MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n ORDER BY n LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n GROUP BY b ORDER BY n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n ORDER BY n SKIP 18446744073709551615"] {
        let q=prepare(text);assert!(run(&q,source(0),GqlQueryPolicy::new(0,0,u64::MAX,u64::MAX)).next().is_none());
    }
}

#[test]
fn close_releases_ranked_page_without_more_source_reads_and_distinct_is_admitted() {
    let q=definition("(a)-[r:R]->(b)",1,0,Some(2),true,Nulls::Last);
    for first in [false,true] {
        let s=source(63);let reads=s.reads.clone();let drops=s.drops.clone();let mut cursor=run(&q,s,wide());
        if first {cursor.next().unwrap().unwrap();assert_eq!(drops.load(Ordering::SeqCst),1);} else {assert_eq!(reads.load(Ordering::SeqCst),0);}
        let before=(cursor.row_stats(),cursor.evaluator_stats(),reads.load(Ordering::SeqCst));cursor.close();cursor.close();
        assert!(cursor.next().is_none());assert_eq!(drops.load(Ordering::SeqCst),1);
        assert_eq!((cursor.row_stats(),cursor.evaluator_stats(),reads.load(Ordering::SeqCst)),before);
    }
    assert!(EdgeAggregatePlan::compile(&q.with_distinct_output(true)).is_ok());
}
