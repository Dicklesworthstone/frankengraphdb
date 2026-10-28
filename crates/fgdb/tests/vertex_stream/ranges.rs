use super::*;

fn range(
    constraints: &[(CanonicalScalar, IntegerComparison)],
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    for (value, comparison) in constraints {
        builder
            .filter(
                "n",
                VertexPredicate::ScalarProperty {
                    key: KEY,
                    predicate: ScalarPredicate::new(value.clone(), *comparison).unwrap(),
                },
            )
            .unwrap();
    }
    builder.prepare("n", offset, count).unwrap()
}

fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}

#[test]
fn numeric_range_streams_match_storage_and_eager_oracles_at_exact_boundaries() {
    let ((), report) = run_async_under_lab(0x51ca_0201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let values = vec![
            CanonicalScalar::Int(i64::MIN),
            float(-9_223_372_036_854_775_808.0),
            float(f64::NEG_INFINITY),
            CanonicalScalar::Int(-7),
            float(-7.5),
            CanonicalScalar::Int(0),
            float(-0.0),
            CanonicalScalar::Int(7),
            float(7.0),
            float(7.5),
            CanonicalScalar::Int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
            CanonicalScalar::Int(i64::MAX),
            float(9_223_372_036_854_775_808.0),
            float(f64::INFINITY),
            float(f64::NAN),
        ];
        let mut batch = WriteBatch::new(R);
        for (at, value) in values.iter().enumerate() {
            // Scalar order and identity order deliberately disagree.
            batch.create_vertex(
                VId((values.len() - at) as u128),
                vec![],
                vec![(KEY, value.clone())],
            );
        }
        batch.create_vertex(VId(0), vec![], vec![(KEY, CanonicalScalar::Null)]);
        batch.create_vertex(VId(100), vec![], vec![(KEY, CanonicalScalar::Bool(true))]);
        batch.create_vertex(
            VId(101),
            vec![],
            vec![(KEY, CanonicalScalar::ucs_basic_text("7").unwrap())],
        );
        batch.create_vertex(VId(u128::MAX), vec![], vec![]);
        let at = db.write(&commit, batch).await.unwrap();
        let rows = db.vertices_at(at).unwrap();
        let mut cases = Vec::new();
        for literal in &values {
            for comparison in [
                IntegerComparison::Less,
                IntegerComparison::LessOrEqual,
                IntegerComparison::Greater,
                IntegerComparison::GreaterOrEqual,
            ] {
                cases.push(vec![(literal.clone(), comparison)]);
            }
        }
        cases.push(vec![
            (CanonicalScalar::Int(-7), IntegerComparison::GreaterOrEqual),
            (float(7.5), IntegerComparison::Less),
        ]);
        cases.push(vec![
            (float(7.5), IntegerComparison::Greater),
            (CanonicalScalar::Int(7), IntegerComparison::LessOrEqual),
        ]);
        for constraints in cases {
            let expected: Vec<_> = rows
                .iter()
                .filter(|row| {
                    let actual = row
                        .props
                        .iter()
                        .find(|(key, _)| *key == KEY)
                        .map(|(_, value)| value.as_scalar().expect("scalar property"));
                    constraints.iter().all(|(literal, comparison)| {
                        comparison.accepts_scalar_pair(actual, Some(literal))
                    })
                })
                .map(|row| row.vid)
                .collect();
            let query = range(&constraints, 0, None);
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, &query, wide())
                    .unwrap()
                    .value,
                expected,
                "constraints={constraints:?}",
            );
            let mut cursor = db
                .stream_graph_vertices_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(expected.len() as u64, u64::MAX, u64::MAX, 64),
                )
                .unwrap();
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "constraints={constraints:?}",
            );
            assert_eq!(cursor.row_stats().snapshot_records, expected.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn range_streams_keep_pinned_winners_and_candidate_budgets_across_both_routes() {
    let ((), report) = run_async_under_lab(0x51ca_0202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 100..356_u128 {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(KEY, CanonicalScalar::Int(id as i64))],
            );
        }
        for (vid, value) in [
            (VId(0), CanonicalScalar::Int(5)),
            (VId(3), float(7.5)),
            (VId(u128::MAX), CanonicalScalar::Int(9)),
        ] {
            batch.create_vertex(vid, vec![], vec![(KEY, value)]);
        }
        let first = db.write(&commit, batch).await.unwrap();
        let constraints = [
            (CanonicalScalar::Int(5), IntegerComparison::GreaterOrEqual),
            (CanonicalScalar::Int(10), IntegerComparison::Less),
        ];
        let query = range(&constraints, 0, None);
        let pinned = db
            .stream_graph_vertices_governed(&cx, &query, GqlQueryPolicy::new(3, 3, 10_000, 64))
            .unwrap();
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), KEY, Some(float(6.5)));
        changes.set_vertex_property(VId(3), KEY, Some(CanonicalScalar::Int(10)));
        changes.delete_vertex(VId(u128::MAX));
        changes.create_vertex(VId(2), vec![], vec![(KEY, float(9.5))]);
        let second = db.write(&commit, changes).await.unwrap();
        let pinned_second = db
            .stream_graph_vertices_governed(&cx, &query, GqlQueryPolicy::new(4, 4, 10_000, 64))
            .unwrap();
        let mut latest = second;
        for broad in [false, true] {
            if broad {
                let mut additions = WriteBatch::new(R);
                for id in 0..81_u128 {
                    additions.create_vertex(
                        VId(1000 + id),
                        vec![],
                        vec![(KEY, float(5.0 + id as f64 / 100.0))],
                    );
                }
                db.write(&commit, additions).await.unwrap();
                let mut retirements = WriteBatch::new(R);
                for id in 0..81_u128 {
                    retirements.delete_vertex(VId(1000 + id));
                }
                latest = db.write(&commit, retirements).await.unwrap();
            }
            let candidates = if broad { 85 } else { 4 };
            for compact in [false, true] {
                if compact {
                    db.compact(&commit).await.unwrap();
                }
                for (at, expected) in [
                    (CommitSeq::ORIGIN, vec![]),
                    (first, vec![VId(0), VId(3), VId(u128::MAX)]),
                    (second, vec![VId(0), VId(2)]),
                    (latest, vec![VId(0), VId(2)]),
                ] {
                    let mut cursor = db
                        .stream_graph_vertices_governed_at(
                            &cx,
                            &query,
                            at,
                            GqlQueryPolicy::new(candidates, candidates, 100_000, 64),
                        )
                        .unwrap();
                    assert_eq!(
                        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                        expected,
                        "broad={broad}, compact={compact}, at={at:?}",
                    );
                    assert_eq!(cursor.row_stats().snapshot_records, candidates);
                    assert_eq!(
                        db.execute_graph_pattern_governed_at(&cx, &query, at, wide())
                            .unwrap()
                            .value,
                        expected,
                    );
                    if broad {
                        assert!(
                            (1..=47).contains(&cursor.evaluator_stats().scratch_entries),
                            "the broad route retains only its governed AVL spine",
                        );
                    }
                }
            }
            let one = range(&constraints, 0, Some(1));
            let mut limited = db
                .stream_graph_vertices_governed(&cx, &one, GqlQueryPolicy::new(1, 1, 10_000, 64))
                .unwrap();
            assert_eq!(limited.next().unwrap().unwrap(), VId(0));
            assert_eq!(limited.row_stats().snapshot_records, 1);
            assert!(limited.next().is_none());
            let mut refused = db
                .stream_graph_vertices_governed(&cx, &one, GqlQueryPolicy::new(1, 1, 10_000, 0))
                .unwrap();
            assert!(matches!(
                refused.next(),
                Some(Err(GqlQueryError::Evaluator(_)))
            ));
            assert_eq!(refused.row_stats().snapshot_records, 0);
            assert_eq!(refused.state(), VertexScanState::Failed);
            assert!(refused.next().is_none());
        }
        let zero = range(&constraints, u64::MAX, Some(0));
        let mut empty = db
            .stream_graph_vertices_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(empty.next().is_none());
        // A zero page drives no work (30b917aa).
        assert_eq!(empty.evaluator_stats().work_units, 0);
        assert_eq!(empty.evaluator_stats().scratch_entries, 0);
        assert_eq!(empty.row_stats().snapshot_records, 0);
        let mut closed = db
            .stream_graph_vertices_governed(&cx, &query, wide())
            .unwrap();
        assert_eq!(closed.next().unwrap().unwrap(), VId(0));
        let stats = (closed.row_stats(), closed.evaluator_stats());
        closed.close();
        assert!(closed.next().is_none());
        assert_eq!((closed.row_stats(), closed.evaluator_stats()), stats);
        drop(db);
        assert_eq!(
            pinned.collect::<Result<Vec<_>, _>>().unwrap(),
            vec![VId(0), VId(3), VId(u128::MAX)],
        );
        assert_eq!(
            pinned_second.collect::<Result<Vec<_>, _>>().unwrap(),
            vec![VId(0), VId(2)],
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_range_bounds_cannot_hide_a_prior_fallible_predicate() {
    let ((), report) = run_async_under_lab(0x51ca_0203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![(KEY, CanonicalScalar::Int(0))]);
        db.write(&commit, batch).await.unwrap();
        for text in [
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p >= 7 RETURN n",
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p < 0 RETURN n",
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p >= 9 AND n.p < 3 RETURN n",
        ] {
            let prepared =
                fgdb_gql::PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
                    (fgdb_gql::GraphSymbolKind::Property, "p") => {
                        Some(fgdb_gql::GraphSymbol::Property(KEY))
                    }
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&fgdb_gql::GqlParameters::new())
                .unwrap();
            let mut cursor = db
                .stream_graph_values_governed(&cx, &prepared, wide())
                .unwrap();
            assert!(
                matches!(
                    cursor.next(),
                    Some(Err(GqlQueryError::Data(error)))
                        if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                ),
                "{text}"
            );
            assert_eq!(cursor.row_stats().snapshot_records, 1);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
