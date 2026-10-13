//! Grouped post-write continuations retain final fields of identity keys.
//! These tests supply the native write owner's final-state occurrence rows;
//! they execute the actual grouping and tail operators without another MATCH.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphEdgeReturnBinding, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, EId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const A: EId = EId((1_u128 << 100) + 1);
const B: EId = EId((1_u128 << 100) + 2);
const PREFIX: &str = "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=2 ON MATCH SET r.p=r.p+1 SET r.p=r.p*2";

type Record = (EId, Option<i64>, Option<i64>);
type Output = GqlQueryExecution<GraphValueRow>;
type Failure = GqlQueryError<GraphSetExecutionError<()>, ()>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn cell(value: Option<i64>) -> GraphValue {
    value.map_or(GraphValue::Scalar(CanonicalScalar::Null), int)
}

fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(&format!("{PREFIX} {tail}"), R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)
}

fn inputs(query: &PreparedGraphEdgeUpsertQuery, records: &[Record]) -> Vec<Vec<GraphValue>> {
    records
        .iter()
        .map(|&(edge, p, q)| {
            query
                .bindings()
                .iter()
                .map(|binding| match binding {
                    GraphEdgeReturnBinding::Edge => GraphValue::Edge(edge),
                    GraphEdgeReturnBinding::Property(key) if *key == P => cell(p),
                    GraphEdgeReturnBinding::Property(key) => {
                        assert_eq!(*key, Q);
                        cell(q)
                    }
                })
                .collect()
        })
        .collect()
}

fn finish(
    query: &PreparedGraphEdgeUpsertQuery,
    records: &[Record],
    rows: u64,
) -> Result<Output, Failure> {
    query.project_rows_governed(
        inputs(query, records),
        GqlExecutionStats::default(),
        GlaExecutionStats::default(),
        policy(rows),
        || Ok(()),
    )
}

fn values(output: &Output) -> Vec<Vec<GraphValue>> {
    output
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn identity_keys_keep_separate_groups_and_count_every_occurrence() {
    let query = prepare(
        "WITH count(*) AS n, r AS edge WHERE edge.p = 4 \
         RETURN edge, edge.p AS p, n ORDER BY n DESC",
    );
    let output = finish(
        &query,
        &[(A, Some(4), None), (B, Some(4), None), (A, Some(4), None)],
        2,
    )
    .unwrap();
    assert_eq!(query.columns(), &["edge", "p", "n"]);
    assert_eq!(
        values(&output),
        vec![
            vec![GraphValue::Edge(A), int(4), int(2)],
            vec![GraphValue::Edge(B), int(4), int(1)],
        ]
    );
    assert_eq!(output.rows.result_rows, 2);
}

#[test]
fn multiple_hidden_keys_remap_summaries_without_changing_argument_slots() {
    let query = prepare(
        "WITH count(*) AS n, r AS edge, sum(r.p) AS total, r AS twin \
         RETURN n, edge.p AS p, edge.q AS q, total, twin.p AS again",
    );
    let records = [(A, Some(4), Some(9)), (A, Some(4), Some(9))];
    let output = finish(&query, &records, 1).unwrap();
    assert_eq!(
        values(&output),
        vec![vec![int(2), int(4), int(9), int(8), int(4)]]
    );
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(query.columns(), &["n", "p", "q", "total", "again"]);
}

#[test]
fn a_property_already_grouped_is_reused_and_distinct_does_not_erase_counts() {
    for distinct in ["", "DISTINCT "] {
        let query = prepare(&format!(
            "WITH {distinct}r AS edge, r.p AS value, count(*) AS n \
             RETURN edge.p AS p, value, n"
        ));
        let output = finish(&query, &[(A, Some(4), None), (A, Some(4), None)], 1).unwrap();
        assert_eq!(values(&output), vec![vec![int(4), int(4), int(2)]]);
        assert_eq!(query.bindings().len(), 2);
    }
}

#[test]
fn grouped_head_pages_precede_property_unwind_and_final_aggregation() {
    let query = prepare(
        "WITH r AS edge, count(*) AS n ORDER BY n DESC LIMIT 1 \
         UNWIND [edge.p,n] AS x RETURN sum(x) AS total",
    );
    let records = [(B, Some(99), None), (A, Some(4), None), (A, Some(4), None)];
    let output = finish(&query, &records, 1).unwrap();
    assert_eq!(values(&output), vec![vec![int(6)]]);
    assert_eq!(output.rows.result_rows, 1);
    assert!(finish(&query, &records, 0).is_err());
}

#[test]
fn null_fields_are_not_empty_groups_and_hidden_fields_never_reach_star() {
    for alias in ["edge", "__fg_boundary_0"] {
        let query = prepare(&format!(
            "WITH r AS {alias}, count(*) AS n WHERE {alias}.p IS NULL RETURN *"
        ));
        assert_eq!(query.columns(), &[alias, "n"]);
        let records = [(A, None, None), (A, None, None), (B, Some(4), None)];
        assert_eq!(
            values(&finish(&query, &records, 1).unwrap()),
            vec![vec![GraphValue::Edge(A), int(2)]]
        );
        assert!(finish(&query, &[], 0).unwrap().value.is_empty());
    }
    let query = prepare("WITH r AS edge, count(*) AS n RETURN edge.p AS p, n");
    assert_eq!(
        values(&finish(&query, &[(A, None, None)], 1).unwrap()),
        vec![vec![cell(None), int(1)]]
    );
}

#[test]
fn grouped_tail_parameters_rebind_without_repreparing_or_changing_group_keys() {
    let text = format!(
        "{PREFIX} WITH r AS edge, count(*) AS n \
         WHERE edge.p >= $minimum RETURN edge.p+$extra AS value, n"
    );
    let template = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols).unwrap();
    assert_eq!(template.parameter_schema().len(), 2);
    let arguments = |minimum| {
        GqlParameters::new()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("extra", 3)
            .unwrap()
    };
    let kept = template.bind_parameters(&arguments(4)).unwrap();
    let dropped = template.bind_parameters(&arguments(5)).unwrap();
    let records = [(A, Some(4), None), (A, Some(4), None)];
    assert_eq!(
        values(&finish(&kept, &records, 1).unwrap()),
        vec![vec![int(7), int(2)]]
    );
    assert!(finish(&dropped, &records, 0).unwrap().value.is_empty());
    assert_ne!(kept.canonical_bytes(), dropped.canonical_bytes());
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn summaries_and_dropped_identities_do_not_acquire_property_read_authority() {
    for tail in [
        "WITH min(r) AS edge RETURN edge.p AS p",
        "WITH count(*) AS n RETURN r.p AS p",
        "WITH r.p AS edge, count(*) AS n RETURN edge.q AS q",
        "WITH r AS edge, count(*) AS n WITH n RETURN edge.p AS p",
        "WITH r AS edge, count(*) AS n UNWIND [edge.p] AS x RETURN edge.p AS p",
    ] {
        let text = format!("{PREFIX} {tail}");
        let result = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols)
            .and_then(|template| template.bind_parameters(&GqlParameters::new()));
        assert!(result.is_err(), "{tail}");
    }
}

#[test]
fn grouped_final_field_errors_still_refuse_under_limit_zero() {
    let query = prepare("WITH r AS edge, count(*) AS n RETURN 12/(edge.p-4) AS value LIMIT 0");
    assert!(matches!(
        finish(&query, &[(A, Some(4), None)], 0),
        Err(GqlQueryError::Source(_))
    ));
}

#[test]
fn grouped_hidden_fields_share_work_scratch_and_every_cancellation_checkpoint() {
    let query = prepare(
        "WITH r AS edge, count(*) AS n WHERE edge.q > 0 \
         UNWIND [edge.p,n] AS x RETURN count(*) AS n, sum(x) AS total",
    );
    let records = [(A, Some(4), Some(9)), (A, Some(4), Some(9))];
    let prefix = GlaExecutionStats {
        work_units: 17,
        scratch_entries: 11,
    };
    let mut checkpoints = 0_usize;
    let output = query
        .project_rows_governed::<(), usize>(
            inputs(&query, &records),
            GqlExecutionStats::default(),
            prefix,
            policy(1),
            || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(values(&output), vec![vec![int(2), int(6)]]);
    let work = output.evaluator.work_units;
    let scratch = output.evaluator.scratch_entries;
    assert!(work > prefix.work_units);
    assert!(scratch > prefix.scratch_entries);
    let exact = query
        .project_rows_governed::<(), ()>(
            inputs(&query, &records),
            GqlExecutionStats::default(),
            prefix,
            GqlQueryPolicy::new(1_000, 1, work, scratch),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(output.value, exact.value);
    for (work, scratch) in [(work - 1, scratch), (work, scratch - 1)] {
        assert!(matches!(
            query.project_rows_governed::<(), ()>(
                inputs(&query, &records),
                GqlExecutionStats::default(),
                prefix,
                GqlQueryPolicy::new(1_000, 1, work, scratch),
                || Ok(()),
            ),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
    assert!(checkpoints > 1);
    for fail_at in 0..checkpoints {
        let mut at = 0;
        let result = query.project_rows_governed::<(), usize>(
            inputs(&query, &records),
            GqlExecutionStats::default(),
            prefix,
            policy(1),
            || {
                let current = at;
                at += 1;
                if current == fail_at {
                    Err(fail_at)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(index)) if index == fail_at));
    }
}
