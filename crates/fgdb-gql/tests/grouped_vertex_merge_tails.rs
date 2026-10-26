//! Grouped vertex-MERGE continuations read the chosen vertex after its actions.
//! Direct identity keys carry fields; aggregate representatives do not.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSetExecutionError, GraphSymbol, GraphSymbolKind, GraphVertexMergeError,
    GraphVertexReturnBinding, GraphWriteChainStep, PreparedGraphVertexMergeRows,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
    PreparedGraphWriteChainText, WithElementReads,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::{Cell, RefCell};

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
    // RETURN *'s columns are in name order (fgdb-7jja3).
    assert_eq!(query.columns(), &["__fg_boundary_0", "c", "known", "other"]);
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(
        values(finish(&query, 6, 1).unwrap()),
        vec![vec![
            GraphValue::Vertex(V),
            int(1),
            int(6),
            GraphValue::Vertex(V)
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

type MatchedInput = Result<Output, GqlQueryError<GraphVertexMergeError<&'static str, ()>, usize>>;

/// The fixture only supplies graph records. MATCH, the owned-row join,
/// projection, hidden property reads and UNWIND all run the native GLA.
fn matched_input(
    merge: &PreparedGraphVertexMergeRows,
    allowance: GqlQueryPolicy,
    reader: Option<Result<i64, &'static str>>,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> (MatchedInput, usize, Vec<(ElementId, PropertyKeyId)>) {
    let vertices = [1, 2, 3].map(|id| VId((1_u128 << 100) + id));
    let properties =
        [1, 2, 3].map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)));
    let pattern_calls = Cell::new(0);
    let point_calls = RefCell::new(Vec::new());
    let checkpoint = RefCell::new(checkpoint);
    let mut patterns = |pattern: &PreparedGraphPattern<GraphValueRow>, remaining| {
        pattern_calls.set(pattern_calls.get() + 1);
        pattern.plan().execute_governed_with_properties(
            vertices.len() as u64,
            vertices,
            std::iter::empty::<(VId, RelationId, VId)>(),
            |vertex, predicates| {
                let at = vertices
                    .iter()
                    .position(|&candidate| candidate == vertex)
                    .unwrap();
                Ok::<_, &'static str>(
                    predicates
                        .iter()
                        .all(|predicate| predicate.matches_borrowed([], [(P, &properties[at])])),
                )
            },
            |vertex, key| {
                let at = vertices
                    .iter()
                    .position(|&candidate| candidate == vertex)
                    .unwrap();
                Ok((key == P).then_some(&properties[at]))
            },
            remaining,
            || (checkpoint.borrow_mut())(),
        )
    };
    // This is the exact one-column row schema exported by the CREATE's WITH.
    // Its full-width identity is deliberately absent from the MATCH fixture.
    let owned = Some(vec![GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(V),
    ])]);
    let result = if let Some(value) = reader {
        let mut source = WithElementReads {
            source: patterns,
            elements: |element, key| {
                point_calls.borrow_mut().push((element, key));
                assert_eq!((element, key), (ElementId::Vertex(V), P));
                value.map(|number| {
                    Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(number)))
                })
            },
        };
        merge.select_governed_with_source::<&'static str, (), usize>(
            owned,
            allowance,
            &mut source,
            || (checkpoint.borrow_mut())(),
        )
    } else {
        // Omitting a reader keeps the existing pattern-only API's typed
        // refusal; an owned identity must never silently acquire a property.
        merge.select_governed::<&'static str, (), usize>(owned, allowance, &mut patterns, || {
            (checkpoint.borrow_mut())()
        })
    };
    (result, pattern_calls.get(), point_calls.into_inner())
}

const MATCHED_MERGE: &str = "CREATE (audit) WITH audit MATCH (n) WHERE n.p >= $minimum \
    WITH audit,n UNWIND [0,0] AS occurrence \
    MERGE (made {q:n.p+audit.p+$delta}) RETURN made";

fn matched_arguments(delta: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("minimum", 2)
        .unwrap()
        .with_int64("delta", delta)
        .unwrap()
}

#[test]
fn post_write_match_merge_source_reads_owned_properties_and_late_parameters() {
    let catalog_calls = Cell::new(0);
    assert!(PreparedGraphWriteChainText::is_chain(MATCHED_MERGE).unwrap());
    let template = PreparedGraphWriteChainText::prepare(
        MATCHED_MERGE,
        R,
        |kind: GraphSymbolKind, name: &str| {
            catalog_calls.set(catalog_calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = catalog_calls.get();
    assert!(resolved > 0);
    assert!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("minimum", 2).unwrap())
            .is_err()
    );
    for delta in [5, 9] {
        let chain = template.bind_parameters(&matched_arguments(delta)).unwrap();
        assert_eq!(
            catalog_calls.get(),
            resolved,
            "rebinding does not resolve symbols again"
        );
        let [_, GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
            panic!("the complete mandatory MATCH must feed one native MERGE");
        };
        let merge = query.merge();
        let (result, patterns, points) =
            matched_input(merge, policy(4), Some(Ok(10)), &mut || Ok(()));
        let result = result.unwrap();
        assert_eq!(
            patterns, 1,
            "one native graph selection for the full input bag"
        );
        assert!(
            !points.is_empty(),
            "the unrematched carried identity uses the point reader"
        );
        assert_eq!(result.rows.snapshot_records, 3);
        assert_eq!(result.rows.result_rows, 4);
        assert_eq!(merge.property_keys().collect::<Vec<_>>(), vec![Q]);
        for (row_at, (row, property)) in result.value.iter().zip([2, 2, 3, 3]).enumerate() {
            assert_eq!(
                &row.values()[..3],
                &[
                    GraphValue::Vertex(V),
                    GraphValue::Vertex(VId((1_u128 << 100) + property as u128)),
                    int(0),
                ]
            );
            let key = merge
                .stored_tuple::<&'static str, (), usize>(row_at, row, &mut |_| Ok(()))
                .unwrap();
            assert_eq!(
                key,
                vec![CanonicalPropertyValue::Scalar(CanonicalScalar::Int(
                    property + 10 + delta
                ))]
            );
        }
    }
}

#[test]
fn post_write_match_merge_source_enforces_exact_cumulative_limits() {
    let chain = PreparedGraphWriteChainText::prepare(MATCHED_MERGE, R, symbols)
        .unwrap()
        .bind_parameters(&matched_arguments(5))
        .unwrap();
    let [_, GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
        panic!("native MERGE query");
    };
    let merge = query.merge();
    let measured = matched_input(merge, policy(4), Some(Ok(10)), &mut || Ok(()))
        .0
        .unwrap();
    let exact = GqlQueryPolicy::new(
        measured.rows.snapshot_records,
        4,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    let exact_result = matched_input(merge, exact, Some(Ok(10)), &mut || Ok(()))
        .0
        .unwrap();
    assert_eq!(exact_result, measured);
    for dimension in 0..4 {
        let mut tight = exact;
        match dimension {
            0 => {
                tight.rows =
                    fgdb_gql::GqlExecutionBudget::new(measured.rows.snapshot_records - 1, 4)
            }
            1 => tight.evaluator.max_work_units -= 1,
            2 => tight.evaluator.max_scratch_entries -= 1,
            _ => tight.rows = fgdb_gql::GqlExecutionBudget::new(measured.rows.snapshot_records, 3),
        }
        let error = matched_input(merge, tight, Some(Ok(10)), &mut || Ok(()))
            .0
            .unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)),
            "dimension {dimension}: {error:?}"
        );
    }
}

#[test]
fn post_write_match_merge_source_preserves_reader_refusals_and_cancellation() {
    let chain = PreparedGraphWriteChainText::prepare(MATCHED_MERGE, R, symbols)
        .unwrap()
        .bind_parameters(&matched_arguments(5))
        .unwrap();
    let [_, GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
        panic!("native MERGE query");
    };
    let merge = query.merge();
    let (denied, patterns, points) =
        matched_input(merge, policy(4), Some(Err("denied")), &mut || Ok(()));
    assert!(matches!(
        denied,
        Err(GqlQueryError::Source(GraphVertexMergeError::InputRelation(
            GraphSetExecutionError::Source("denied")
        )))
    ));
    assert_eq!(patterns, 1);
    assert_eq!(points, vec![(ElementId::Vertex(V), P)]);
    let missing = matched_input(merge, policy(4), None, &mut || Ok(())).0;
    assert!(matches!(
        missing,
        Err(GqlQueryError::Source(GraphVertexMergeError::InputRelation(
            GraphSetExecutionError::Projection { .. }
        )))
    ));
    let mut checkpoints = 0;
    matched_input(merge, policy(4), Some(Ok(10)), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .0
    .unwrap();
    assert!(checkpoints > 1);
    for fail_at in [0, checkpoints / 2, checkpoints - 1] {
        let mut at = 0;
        let result = matched_input(merge, policy(4), Some(Ok(10)), &mut || {
            let current = at;
            at += 1;
            if current == fail_at {
                Err(current)
            } else {
                Ok(())
            }
        })
        .0;
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
    }
}
