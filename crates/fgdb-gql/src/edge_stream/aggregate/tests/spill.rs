use super::*;
use crate::spill_aggregate::{EdgeSpillAggregateCursor, SpillAggregatePlan};

#[test]
fn spill_join_occurrences_and_numeric_groups_match_the_existing_reducer() {
    let wide = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
    for mask in 0..64 {
        for pattern in [
            "(a)-[r:R]->(b)",
            "(a)<-[r:R]-(b)",
            "(a)-[r:R]-(b)",
            "(a)-[r:R]->(b)-[s:S]->(c)",
            "(a)-[r:R]-(b)-[s:S]-(c)",
        ] {
            for (key_output, key) in [
                ("b AS target", "b"),
                ("r.p AS bucket", "r.p"),
                ("r AS edge", "r"),
            ] {
                let q = prepare(&format!(
                    "MATCH {pattern} RETURN {key_output}, COUNT(*) AS rows, COUNT(r.p) AS nonnull, SUM(r.p) AS sum, AVG(r.p) AS average, MIN(r.p) AS minimum, MAX(r.p) AS maximum GROUP BY {key}"
                ));
                let expected = run(&q, source(mask), wide)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let SpillAggregatePlan::Edge(plan) = SpillAggregatePlan::compile(&q).unwrap()
                else {
                    panic!("edge input");
                };
                let definition = plan.definition().clone();
                let input = source(mask);
                let drops = input.drops.clone();
                let mut cursor = EdgeSpillAggregateCursor::new(
                    input,
                    plan,
                    GqlQueryPolicy::new(u64::MAX, expected.len() as u64, u64::MAX, u64::MAX),
                    || Ok::<_, ()>(()),
                );
                let mut groups = BTreeMap::new();
                while let Some(row) = cursor.next_input().unwrap() {
                    assert_eq!(cursor.row_stats().result_rows, 0);
                    let key: Vec<_> = definition
                        .group_key_columns()
                        .iter()
                        .map(|&column| row.values()[column].clone())
                        .collect();
                    let state = groups.entry(key).or_insert_with(|| {
                        definition
                            .new_state(&mut |event| cursor.charge(event))
                            .unwrap()
                    });
                    definition
                        .update(state, &row, &mut |event| cursor.charge(event))
                        .unwrap();
                }
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    1,
                    "EOF releases the pinned source before groups are completed"
                );
                let actual = groups
                    .into_iter()
                    .map(|(keys, state)| {
                        let row = definition
                            .finish(keys, state, &mut |event| cursor.charge(event))
                            .unwrap();
                        cursor.finish_result().unwrap();
                        row
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    actual, expected,
                    "mask={mask}, pattern={pattern}, key={key}"
                );
                assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            }
        }
    }
}

#[test]
fn spill_edge_input_error_is_checked_before_zero_result_budget_and_drops_source() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN SUM(r.p) AS sum");
    let SpillAggregatePlan::Edge(plan) = SpillAggregatePlan::compile(&q).unwrap() else {
        panic!("edge input");
    };
    let mut input = source(63);
    input.edges.get_mut(&EId(2)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
    let drops = input.drops.clone();
    let mut cursor = EdgeSpillAggregateCursor::new(
        input,
        plan,
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next_input().unwrap().is_some());
    assert!(matches!(
        cursor.next_input(),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 0
        }))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(cursor.next_input().unwrap().is_none());
}
