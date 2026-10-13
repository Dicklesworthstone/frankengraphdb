//! Grouped vertex-MERGE continuations read the chosen vertex after its actions.
//! Direct identity keys carry fields; aggregate representatives do not.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSetExecutionError, GraphSymbol, GraphSymbolKind, GraphVertexReturnBinding,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const V: VId = VId((1_u128 << 100) + 7);
const PREFIX: &str = "MERGE (n {q:9}) ON CREATE SET n.p=3 ON MATCH SET n.p=n.p+1 SET n.p=n.p*2";

type Output = GqlQueryExecution<GraphValueRow>;
type Failure = GqlQueryError<GraphSetExecutionError<()>, ()>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn prepare(tail: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(&format!("{PREFIX} {tail}"), R, symbols)
        .expect(tail)
        .bind_parameters(&GqlParameters::new())
        .expect(tail)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)
}

fn input(query: &PreparedGraphVertexUpsertQuery, p: i64) -> Vec<GraphValue> {
    query
        .bindings()
        .iter()
        .map(|binding| match *binding {
            GraphVertexReturnBinding::Vertex => GraphValue::Vertex(V),
            GraphVertexReturnBinding::Property(key) if key == P => int(p),
            GraphVertexReturnBinding::Property(key) if key == Q => int(9),
            _ => panic!("fixture contains only identity and explicit properties"),
        })
        .collect()
}

fn finish(query: &PreparedGraphVertexUpsertQuery, p: i64, rows: u64) -> Result<Output, Failure> {
    query.project_governed(
        input(query, p),
        GqlExecutionStats::default(),
        GlaExecutionStats::default(),
        policy(rows),
        || Ok(()),
    )
}

fn values(output: Output) -> Vec<Vec<GraphValue>> {
    output
        .value
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn grouped_identity_reads_post_action_fields_and_remaps_multiple_summaries() {
    let query = prepare(
        "WITH sum(n.p) AS total, n AS item, count(*) AS c, count(DISTINCT n.p) AS d \
         WHERE item.q > 0 RETURN item AS identity, item.p AS p, total, c, d, item.q AS q",
    );
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(
        query
            .bindings()
            .iter()
            .filter(|b| { **b == GraphVertexReturnBinding::Property(P) })
            .count(),
        1
    );
    assert_eq!(
        values(finish(&query, 14, 1).unwrap()),
        vec![vec![
            GraphValue::Vertex(V),
            int(14),
            int(14),
            int(1),
            int(1),
            int(9)
        ],]
    );
}

#[test]
fn duplicate_aliases_and_existing_property_keys_keep_hidden_fields_private() {
    let query = prepare(
        "WITH n.p AS known, n AS __fg_boundary_0, n AS other, count(*) AS c \
         WHERE __fg_boundary_0.p > 0 AND other.q > 0 RETURN *",
    );
    assert_eq!(query.columns(), &["known", "__fg_boundary_0", "other", "c"]);
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(
        values(finish(&query, 6, 1).unwrap()),
        vec![vec![
            int(6),
            GraphValue::Vertex(V),
            GraphValue::Vertex(V),
            int(1)
        ],]
    );
}

#[test]
fn grouped_vertex_field_unwind_keeps_the_head_and_final_windows_separate() {
    let query = prepare(
        "WITH n AS item, count(*) AS c LIMIT 1 \
         UNWIND [item.p, item.p+1, item.p+2] AS x RETURN x AS value, c ORDER BY value DESC",
    )
    .with_page(1, Some(1));
    assert_eq!(
        values(finish(&query, 6, 1).unwrap()),
        vec![vec![int(7), int(1)]]
    );
    let zero = prepare("WITH n AS item, count(*) AS c WHERE item.p < 0 RETURN c");
    assert!(finish(&zero, 6, 0).unwrap().value.is_empty());
}

#[test]
fn grouped_vertex_tail_rebinds_without_resolving_the_catalog_again() {
    let calls = Cell::new(0);
    let template = PreparedGraphVertexUpsertQueryText::prepare(
        &format!(
            "{PREFIX} WITH n AS item, count(*) AS c \
                  WHERE item.p > $minimum RETURN item.p + $delta AS value, c"
        ),
        R,
        |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    let args = |minimum| {
        GqlParameters::new()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("delta", 10)
            .unwrap()
    };
    let shown = template.bind_parameters(&args(0)).unwrap();
    let hidden = template.bind_parameters(&args(9)).unwrap();
    assert_eq!(calls.get(), resolved);
    assert_eq!(
        values(finish(&shown, 6, 1).unwrap()),
        vec![vec![int(16), int(1)]]
    );
    assert!(finish(&hidden, 6, 0).unwrap().value.is_empty());
    assert_ne!(shown.canonical_bytes(), hidden.canonical_bytes());
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn grouped_vertex_representatives_and_dropped_identities_never_gain_reads() {
    for tail in [
        "WITH min(n) AS item RETURN item.p",
        "WITH max(n) AS item RETURN item.p",
        "WITH count(*) AS c RETURN n.p",
        "WITH n AS item, count(*) AS c WITH c RETURN item.p",
        "WITH n AS item, count(*) AS c UNWIND [1] AS x RETURN item.p",
        "WITH n AS item, count(*) AS c WHERE item.p > 0 RETURN __fg_boundary_0",
    ] {
        let calls = Cell::new(0);
        let result = PreparedGraphVertexUpsertQueryText::prepare(
            &format!("{PREFIX} {tail}"),
            R,
            |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        );
        assert!(result.is_err(), "{tail}");
        assert_eq!(calls.get(), 0, "{tail}");
    }
}

#[test]
fn grouped_vertex_tail_failures_survive_both_text_and_api_limit_zero() {
    for tail in [
        "WITH n AS item, count(*) AS c RETURN 12/item.p AS value LIMIT 0",
        "WITH n AS item, count(*) AS c RETURN 12/item.p AS value",
    ] {
        let query = prepare(tail).with_page(0, Some(0));
        assert!(matches!(
            finish(&query, 0, 0),
            Err(GqlQueryError::Source(_))
        ));
    }
}

#[test]
fn grouped_vertex_work_scratch_and_every_checkpoint_remain_cumulative() {
    let query = prepare(
        "WITH n AS item, count(*) AS c \
         UNWIND [item.p,item.p+1] AS x RETURN x, c ORDER BY x",
    );
    let prefix = GlaExecutionStats {
        work_units: 17,
        scratch_entries: 11,
    };
    let mut checkpoints = 0;
    let result = query
        .project_governed::<(), ()>(
            input(&query, 6),
            GqlExecutionStats::default(),
            prefix,
            policy(2),
            || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].values(), &[int(6), int(1)]);
    let used = result.evaluator;
    assert!(used.work_units > prefix.work_units);
    assert!(used.scratch_entries > prefix.scratch_entries);
    for (work, scratch, pass) in [
        (used.work_units, used.scratch_entries, true),
        (used.work_units - 1, used.scratch_entries, false),
        (used.work_units, used.scratch_entries - 1, false),
    ] {
        let output = query.project_governed::<(), ()>(
            input(&query, 6),
            GqlExecutionStats::default(),
            prefix,
            GqlQueryPolicy::new(1_000, 2, work, scratch),
            || Ok(()),
        );
        if pass {
            assert_eq!(output.unwrap().value, result.value);
        } else {
            assert!(matches!(output, Err(GqlQueryError::Evaluator(_))));
        }
    }
    assert!(checkpoints > 1);
    for fail_at in 0..checkpoints {
        let mut reached = 0;
        let output = query.project_governed::<(), usize>(
            input(&query, 6),
            GqlExecutionStats::default(),
            prefix,
            policy(2),
            || {
                let current = reached;
                reached += 1;
                if current == fail_at {
                    Err(current)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(output, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
    }
}
