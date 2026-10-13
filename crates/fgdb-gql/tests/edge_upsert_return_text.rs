//! Parser/binder regressions use the shared RETURN kernels, not a mock matcher.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryPolicy, GraphEdgeReturnBinding,
    GraphSymbol, GraphSymbolKind, PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
    PreparedGraphEdgeUpsertText,
};
use fgdb_types::{CanonicalScalar, EId};

const PREFIX: &str = "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b)";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn prepare(suffix: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(&format!("{PREFIX} {suffix}"), RelationId(2), symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn project(
    query: &PreparedGraphEdgeUpsertQuery,
    input: Option<Vec<GraphValue>>,
) -> Vec<Vec<GraphValue>> {
    let input_rows = u64::from(input.is_some());
    let result = query
        .project_governed::<(), ()>(
            input,
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: input_rows,
            },
            GlaExecutionStats {
                work_units: 10,
                scratch_entries: 2,
            },
            GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            || Ok(()),
        )
        .unwrap();
    result
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn action_and_return_parameters_bind_once_and_can_be_rebound_without_resolving() {
    let probes = std::cell::Cell::new(0);
    let text = format!("{PREFIX} SET r.p = $step RETURN r.p + $step AS value LIMIT $page");
    let prepared =
        PreparedGraphEdgeUpsertQueryText::prepare(&text, RelationId(2), |kind, name: &str| {
            probes.set(probes.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
    assert_eq!(prepared.statement(), text);
    assert_eq!(prepared.parameter_schema().len(), 2);
    let resolved = probes.get();
    let args = |step| {
        GqlParameters::new()
            .with_int64("step", step)
            .unwrap()
            .with_uint64("page", 1)
            .unwrap()
    };
    let first = prepared.bind_parameters(&args(3)).unwrap();
    let second = prepared.bind_parameters(&args(5)).unwrap();
    assert_eq!(
        probes.get(),
        resolved,
        "binding never calls the symbol resolver"
    );
    assert_eq!(
        first.bindings(),
        &[GraphEdgeReturnBinding::Property(PropertyKeyId(1))]
    );
    assert_eq!(project(&first, Some(vec![int(14)])), vec![vec![int(17)]]);
    assert_eq!(project(&second, Some(vec![int(14)])), vec![vec![int(19)]]);
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(
        first.canonical_bytes(),
        prepared
            .bind_parameters(&args(3))
            .unwrap()
            .canonical_bytes()
    );
    assert!(prepared.bind_parameters(&GqlParameters::new()).is_err());
    assert!(!format!("{prepared:?}").contains("$step"));
}

#[test]
fn identity_expressions_grouping_and_paging_use_native_return_semantics() {
    let query = prepare(
        "ON CREATE SET r.p = 7 RETURN r AS relationship, r.p AS value ORDER BY value LIMIT 1;",
    );
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(PropertyKeyId(1))
        ]
    );
    assert_eq!(
        project(&query, Some(vec![GraphValue::Edge(EId(9)), int(7)])),
        vec![vec![GraphValue::Edge(EId(9)), int(7)]]
    );
    let grouped =
        prepare("RETURN count(*) AS rows,sum(r.p) AS total,collect(DISTINCT r.p) AS values");
    assert_eq!(
        project(&grouped, Some(vec![int(7)])),
        vec![vec![
            int(1),
            int(7),
            GraphValue::List(vec![int(7)].into_boxed_slice())
        ]]
    );
    let paged = prepare("RETURN DISTINCT r.p AS value SKIP 1 LIMIT 0");
    assert!(project(&paged, Some(vec![int(7)])).is_empty());
}

#[test]
fn no_input_preserves_empty_projection_and_the_global_aggregate_empty_group() {
    assert!(project(&prepare("RETURN 42 AS value"), None).is_empty());
    assert_eq!(
        project(&prepare("RETURN count(*) AS rows"), None),
        vec![vec![int(0)]]
    );
    assert!(project(&prepare("RETURN r.p AS key,count(*) AS rows"), None).is_empty());
}

#[test]
fn unsupported_scopes_and_broken_suffixes_cannot_be_ignored_or_resolved_early() {
    for suffix in [
        "RETURN a",
        "RETURN b.p",
        "RETURN *",
        "RETURN r{.*}",
        "RETURN r.p AS value; CREATE (n)",
        "SET r.p = 1 RETURN r.p +",
        "RETURN r.p RETURN r",
    ] {
        let probes = std::cell::Cell::new(0);
        let result = PreparedGraphEdgeUpsertQueryText::prepare(
            &format!("{PREFIX} {suffix}"),
            RelationId(2),
            |kind, name: &str| {
                probes.set(probes.get() + 1);
                symbols(kind, name)
            },
        );
        assert!(result.is_err(), "{suffix}");
        assert_eq!(
            probes.get(),
            0,
            "whole syntax admission precedes catalog access: {suffix}"
        );
    }
    assert!(
        PreparedGraphEdgeUpsertText::prepare(
            &format!("{PREFIX} SET r.p = 1 RETURN r.p"),
            RelationId(2),
            symbols
        )
        .is_err(),
        "the no-RETURN API never drops a suffix"
    );
    assert!(
        PreparedGraphEdgeUpsertQueryText::prepare(
            &format!("{PREFIX} SET r.p = 1"),
            RelationId(2),
            symbols
        )
        .is_err()
    );
}

#[test]
fn returning_fields_are_checked_even_when_pagination_would_hide_them() {
    let query = prepare("RETURN r.p AS value LIMIT 0");
    let result = query.project_governed::<(), ()>(
        Some(vec![GraphValue::Edge(EId(9))]),
        GqlExecutionStats {
            snapshot_records: 0,
            result_rows: 1,
        },
        GlaExecutionStats {
            work_units: 0,
            scratch_entries: 0,
        },
        GqlQueryPolicy::new(100, 0, 100_000, 100_000),
        || Ok(()),
    );
    assert!(
        result.is_err(),
        "property inputs cannot be relabeled edge identities"
    );
}
