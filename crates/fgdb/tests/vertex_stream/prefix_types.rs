use super::*;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::stream::VertexScanPlan;
use fgdb_gql::{GqlParameters, GraphIntegerErrorKind, PreparedGraphText};

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn prepared(statement: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(statement, |kind, name: &str| match (kind, name) {
        (fgdb_gql::GraphSymbolKind::Property, "p") => Some(fgdb_gql::GraphSymbol::Property(KEY)),
        (fgdb_gql::GraphSymbolKind::Property, "q") => {
            Some(fgdb_gql::GraphSymbol::Property(PropertyKeyId(8)))
        }
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn prefix(value: &str, limit: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let mut statement = format!(
        "MATCH (n) WHERE n.p STARTS WITH '{}' RETURN n AS id",
        value.replace('\'', "''"),
    );
    if let Some(limit) = limit {
        statement.push_str(&format!(" LIMIT {limit}"));
    }
    prepared(&statement)
}

fn identities(ids: &[VId]) -> Vec<GraphValueRow> {
    ids.iter()
        .map(|vid| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(*vid)]))
        .collect()
}

#[test]
fn prefix_streams_and_native_aggregates_seek_mixed_type_properties_exactly() {
    let ((), report) = run_async_under_lab(0x51ca_0301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let values = vec![
            text(""),
            text("pre"),
            text("prefix"),
            text("prelude"),
            text("outside"),
            text("é"),
            text("e\u{301}"),
            text("\u{d7ff}"),
            text("\u{d7ff}z"),
            text("\u{e000}"),
            text("\u{10ffff}"),
            text("\u{10ffff}z"),
            CanonicalScalar::Null,
            CanonicalScalar::Int(7),
            CanonicalScalar::Bool(false),
            CanonicalScalar::bytes(vec![1, 2]).unwrap(),
        ];
        let mut batch = WriteBatch::new(R);
        for (at, value) in values.iter().enumerate() {
            batch.create_vertex(
                VId((values.len() - at) as u128),
                vec![],
                vec![
                    (KEY, value.clone()),
                    (PropertyKeyId(8), CanonicalScalar::Int(1)),
                ],
            );
        }
        batch.create_vertex(VId(0), vec![], vec![(KEY, text("prefix"))]);
        batch.create_vertex(VId(u128::MAX), vec![], vec![]);
        let at = db.write(&commit, batch).await.unwrap();
        let rows = db.vertices_at(at).unwrap();
        for spelling in ["", "pre", "é", "\u{d7ff}", "\u{10ffff}", "absent"] {
            let ids: Vec<_> = rows
                .iter()
                .filter(|row| {
                    row.props.iter().any(|(key, value)| {
                        *key == KEY
                            && matches!(value, CanonicalScalar::Text(value)
                        if value.as_str().starts_with(spelling))
                    })
                })
                .map(|row| row.vid)
                .collect();
            let expected = identities(&ids);
            let query = prefix(spelling, None);
            let scan = VertexScanPlan::compile(query.plan()).unwrap();
            assert_eq!(scan.leading_root_prefix(), Some((KEY, spelling)));
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, &query, wide())
                    .unwrap()
                    .value,
                expected,
            );
            let mut cursor = db
                .stream_graph_values_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(ids.len() as u64, ids.len() as u64, 1_000_000, 10_000),
                )
                .unwrap();
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(cursor.row_stats().snapshot_records, ids.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        let symbols = || RelationBind::new().with_property("p", KEY);
        for (spelling, expected) in [("pre", 4), ("absent", 0)] {
            let statement =
                format!("MATCH (n) WHERE n.p STARTS WITH '{spelling}' RETURN COUNT(*) AS total");
            let args = GqlParameters::new();
            let fgdb::QueryResult::Rows { rows, .. } =
                db.query(&cx, &statement, &args, symbols(), wide()).unwrap()
            else {
                panic!("aggregate returned a write");
            };
            let mut cursor = db
                .query_aggregate_stream(
                    &cx,
                    &statement,
                    &args,
                    symbols(),
                    GqlQueryPolicy::new(expected, 1, 1_000_000, 10_000),
                )
                .unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let result = cursor.next().unwrap().unwrap();
            assert_eq!(result.values()[0].as_count(), Some(expected));
            assert_eq!(rows, vec![result.values().to_vec()]);
            assert_eq!(cursor.row_stats().snapshot_records, expected);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prefix_eligibility_never_hides_arithmetic_in_either_conjunct_order() {
    let ((), report) = run_async_under_lab(0x51ca_0303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(
            VId(0),
            vec![],
            vec![
                (KEY, text("outside")),
                (PropertyKeyId(8), CanonicalScalar::Int(0)),
            ],
        );
        batch.create_vertex(
            VId(1),
            vec![],
            vec![
                (KEY, text("prefix")),
                (PropertyKeyId(8), CanonicalScalar::Int(1)),
            ],
        );
        db.write(&commit, batch).await.unwrap();
        for value in [
            text("outside"),
            CanonicalScalar::Null,
            CanonicalScalar::Int(7),
        ] {
            let mut changes = WriteBatch::new(R);
            changes.set_vertex_property(VId(0), KEY, Some(value));
            db.write(&commit, changes).await.unwrap();
            for statement in [
                "MATCH (n) WHERE 10 / n.q > 0 AND n.p STARTS WITH 'pre' RETURN n",
                "MATCH (n) WHERE n.p STARTS WITH 'pre' AND 10 / n.q > 0 RETURN n",
            ] {
                let query = prepared(statement);
                assert!(
                    VertexScanPlan::compile(query.plan())
                        .unwrap()
                        .leading_root_prefix()
                        .is_none()
                );
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &query, wide()),
                    Err(GqlQueryError::Data(error))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero
                ));
                let mut cursor = db
                    .stream_graph_values_governed(&cx, &query, wide())
                    .unwrap();
                assert!(
                    matches!(
                        cursor.next(),
                        Some(Err(GqlQueryError::Data(error)))
                            if error.kind == GraphIntegerErrorKind::DivisionByZero
                    ),
                    "{statement}"
                );
                assert_eq!(cursor.row_stats().snapshot_records, 1);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
            }
        }
        let disjunction = prepared("MATCH (n) WHERE n.p STARTS WITH 'pre' OR n.q = 0 RETURN n");
        assert!(
            VertexScanPlan::compile(disjunction.plan())
                .unwrap()
                .leading_root_prefix()
                .is_none()
        );
        assert_eq!(
            db.stream_graph_values_governed(&cx, &disjunction, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            identities(&[VId(0), VId(1)]),
        );
        let negated = prepared("MATCH (n) WHERE NOT (n.p STARTS WITH 'absent') RETURN n");
        assert!(
            VertexScanPlan::compile(negated.plan())
                .unwrap()
                .leading_root_prefix()
                .is_none()
        );
        assert_eq!(
            db.stream_graph_values_governed(&cx, &negated, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            identities(&[VId(1)]),
        );
        // Mixed types are UNKNOWN, not a user-visible NonText error.
        assert_eq!(
            db.stream_graph_values_governed(&cx, &prefix("pre", None), wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            identities(&[VId(1)]),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retired_numeric_values_keep_native_prefix_candidate_admission_bounded() {
    let ((), report) = run_async_under_lab(0x51ca_0304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![(KEY, CanonicalScalar::Int(0))]);
        batch.create_vertex(VId(1), vec![], vec![(KEY, text("pre-old"))]);
        for id in 100..228_u128 {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(KEY, CanonicalScalar::Int(id as i64))],
            );
        }
        batch.create_vertex(
            VId(u128::MAX),
            vec![],
            vec![(KEY, CanonicalScalar::bytes(vec![1]).unwrap())],
        );
        let first = db.write(&commit, batch).await.unwrap();
        let query = prefix("pre", None);
        let pinned = db
            .stream_graph_values_governed(&cx, &query, GqlQueryPolicy::new(1, 1, 100_000, 1000))
            .unwrap();
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), KEY, Some(text("pre-new")));
        changes.set_vertex_property(VId(1), KEY, Some(text("outside")));
        changes.delete_vertex(VId(u128::MAX));
        let second = db.write(&commit, changes).await.unwrap();
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            for (at, ids) in [
                (CommitSeq::ORIGIN, vec![]),
                (first, vec![VId(1)]),
                (second, vec![VId(0)]),
            ] {
                let expected = identities(&ids);
                let mut cursor = db
                    .stream_graph_values_governed_at(
                        &cx,
                        &query,
                        at,
                        GqlQueryPolicy::new(2, 2, 100_000, 1000),
                    )
                    .unwrap();
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected,
                    "compact={compact}, at={at:?}",
                );
                assert_eq!(cursor.row_stats().snapshot_records, 2);
                assert_eq!(
                    db.execute_graph_pattern_governed_at(&cx, &query, at, wide())
                        .unwrap()
                        .value,
                    expected,
                );
            }
        }
        drop(db);
        assert_eq!(
            pinned.collect::<Result<Vec<_>, _>>().unwrap(),
            identities(&[VId(1)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
