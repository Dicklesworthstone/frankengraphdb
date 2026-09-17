//! Native text expressions use committed graph values, historical snapshots and
//! the canonical transaction workspace, rather than an alternate interpreter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationPolicy,
    GraphSymbol, GraphSymbolKind, PreparedGraphMutationText, PreparedGraphSet,
    PreparedGraphSetText, PreparedTemporalGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const T: PropertyKeyId = PropertyKeyId(1);
const U: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "t") => Some(GraphSymbol::Property(T)),
        (GraphSymbolKind::Property, "u") => Some(GraphSymbol::Property(U)),
        _ => None,
    }
}

fn query(statement: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn cells(rows: &[GraphValueRow]) -> Vec<Vec<CanonicalScalar>> {
    rows.iter()
        .map(|row| {
            row.values()[1..]
                .iter()
                .map(|value| {
                    if value.is_null() {
                        CanonicalScalar::Null
                    } else {
                        value.as_scalar().expect("scalar projection").clone()
                    }
                })
                .collect()
        })
        .collect()
}

fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.values()[0].as_vertex().unwrap())
        .collect()
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, t, u) in [
        (1, text("Alphaβ"), text("Alpha")),
        (2, text("alphaβ"), text("β")),
        (3, text(""), text("")),
        (4, CanonicalScalar::Null, text("x")),
        (5, text("x"), CanonicalScalar::Null),
    ] {
        batch.create_vertex(VId(id), vec![], vec![(T, t), (U, u)]);
    }
    batch.create_vertex(VId(6), vec![], vec![]);
    db.write(cx, batch).await.unwrap()
}

#[test]
fn every_text_operator_filters_and_projects_committed_properties() {
    let ((), report) = run_async_under_lab(0x7e87_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        use CanonicalScalar::{Bool, Int, Null};
        for (expression, expected, predicate, selected) in [
            (
                "n.t STARTS WITH 'Al'",
                vec![
                    Bool(true),
                    Bool(false),
                    Bool(false),
                    Null,
                    Bool(false),
                    Null,
                ],
                "n.t STARTS WITH 'Al'",
                vec![VId(1)],
            ),
            (
                "n.t ENDS WITH 'β'",
                vec![Bool(true), Bool(true), Bool(false), Null, Bool(false), Null],
                "n.t ENDS WITH 'β'",
                vec![VId(1), VId(2)],
            ),
            (
                "n.t CONTAINS 'Alpha'",
                vec![
                    Bool(true),
                    Bool(false),
                    Bool(false),
                    Null,
                    Bool(false),
                    Null,
                ],
                "n.t CONTAINS 'Alpha'",
                vec![VId(1)],
            ),
            (
                "n.t STARTS WITH n.u",
                vec![Bool(true), Bool(false), Bool(true), Null, Null, Null],
                "n.t STARTS WITH n.u",
                vec![VId(1), VId(3)],
            ),
            (
                "n.t ENDS WITH n.u",
                vec![Bool(false), Bool(true), Bool(true), Null, Null, Null],
                "n.t ENDS WITH n.u",
                vec![VId(2), VId(3)],
            ),
            (
                "n.t CONTAINS n.u",
                vec![Bool(true), Bool(true), Bool(true), Null, Null, Null],
                "n.t CONTAINS n.u",
                vec![VId(1), VId(2), VId(3)],
            ),
            (
                "UPPER(n.t)",
                vec![
                    text("ALPHAΒ"),
                    text("ALPHAΒ"),
                    text(""),
                    Null,
                    text("X"),
                    Null,
                ],
                "UPPER(n.t) = 'ALPHAΒ'",
                vec![VId(1), VId(2)],
            ),
            (
                "LOWER(n.t)",
                vec![
                    text("alphaβ"),
                    text("alphaβ"),
                    text(""),
                    Null,
                    text("x"),
                    Null,
                ],
                "LOWER(n.t) = 'alphaβ'",
                vec![VId(1), VId(2)],
            ),
            (
                "TRIM(n.t)",
                vec![
                    text("Alphaβ"),
                    text("alphaβ"),
                    text(""),
                    Null,
                    text("x"),
                    Null,
                ],
                "TRIM(n.t) = 'Alphaβ'",
                vec![VId(1)],
            ),
            (
                "SUBSTRING(n.t,2,3)",
                vec![text("lph"), text("lph"), text(""), Null, text(""), Null],
                "SUBSTRING(n.t,2,3) = 'lph'",
                vec![VId(1), VId(2)],
            ),
            (
                "CHAR_LENGTH(n.t)",
                vec![Int(6), Int(6), Int(0), Null, Int(1), Null],
                "CHAR_LENGTH(n.t) = 6",
                vec![VId(1), VId(2)],
            ),
            (
                "n.t || n.u",
                vec![
                    text("AlphaβAlpha"),
                    text("alphaββ"),
                    text(""),
                    Null,
                    Null,
                    Null,
                ],
                "n.t || n.u = 'alphaββ'",
                vec![VId(2)],
            ),
            (
                "n.t IN ['Alphaβ','x']",
                vec![Bool(true), Bool(false), Bool(false), Null, Bool(true), Null],
                "n.t IN ['Alphaβ','x']",
                vec![VId(1), VId(5)],
            ),
        ] {
            let projection = query(&format!(
                "MATCH (n) RETURN n,{expression} AS value ORDER BY n"
            ));
            let result = db
                .execute_graph_set_governed(&cx, &projection, policy())
                .unwrap();
            assert_eq!(ids(&result.value), (1..=6).map(VId).collect::<Vec<_>>());
            assert_eq!(
                cells(&result.value),
                expected
                    .into_iter()
                    .map(|value| vec![value])
                    .collect::<Vec<_>>(),
                "{expression}"
            );
            let filtered = query(&format!("MATCH (n) WHERE {predicate} RETURN n ORDER BY n"));
            assert_eq!(
                ids(&db
                    .execute_graph_set_governed(&cx, &filtered, policy())
                    .unwrap()
                    .value),
                selected,
                "{predicate}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn null_on_either_side_stays_unknown_in_return_and_is_not_true_in_where() {
    let ((), report) = run_async_under_lab(0x7e87_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for expression in [
            "NULL STARTS WITH n.t",
            "n.t STARTS WITH NULL",
            "NULL ENDS WITH n.t",
            "n.t ENDS WITH NULL",
            "NULL CONTAINS n.t",
            "n.t CONTAINS NULL",
            "NULL || n.t",
            "n.t || NULL",
            "UPPER(NULL)",
            "LOWER(NULL)",
            "TRIM(NULL)",
            "CHAR_LENGTH(NULL)",
            "SUBSTRING(NULL,1,1)",
            "SUBSTRING(n.t,NULL,1)",
            "SUBSTRING(n.t,1,NULL)",
            "NULL IN ['Alphaβ','x']",
            "n.t IN [NULL]",
        ] {
            let projection = query(&format!(
                "MATCH (n) RETURN n,{expression} AS value ORDER BY n"
            ));
            assert_eq!(
                cells(
                    &db.execute_graph_set_governed(&cx, &projection, policy())
                        .unwrap()
                        .value
                ),
                vec![vec![CanonicalScalar::Null]; 6],
                "{expression}"
            );
            // Boolean-valued expressions enter WHERE directly (UNKNOWN never
            // survives); scalar-valued ones compare against one literal. Both
            // stay UNKNOWN for a NULL operand, so every row is filtered out.
            let boolean_valued = matches!(expression, e if e.contains("STARTS WITH")
                || e.contains("ENDS WITH") || e.contains("CONTAINS") || e.contains("IN ["));
            let filtered = if boolean_valued {
                query(&format!("MATCH (n) WHERE ({expression}) RETURN n"))
            } else {
                query(&format!("MATCH (n) WHERE ({expression}) = 'x' RETURN n"))
            };
            assert_eq!(
                ids(&db
                    .execute_graph_set_governed(&cx, &filtered, policy())
                    .unwrap()
                    .value),
                vec![],
                "{expression}"
            );
        }
        // A matching member dominates UNKNOWN; a nonmatch with NULL remains
        // UNKNOWN. This preserves the preexisting property-list IN contract.
        let membership = query(
            "MATCH (n) RETURN n,n.t IN ['Alphaβ',NULL] AS member,n.t NOT IN ['Alphaβ',NULL] AS nonmember ORDER BY n",
        );
        assert_eq!(
            cells(
                &db.execute_graph_set_governed(&cx, &membership, policy())
                    .unwrap()
                    .value
            ),
            vec![
                vec![CanonicalScalar::Bool(true), CanonicalScalar::Bool(false)],
                vec![CanonicalScalar::Null, CanonicalScalar::Null],
                vec![CanonicalScalar::Null, CanonicalScalar::Null],
                vec![CanonicalScalar::Null, CanonicalScalar::Null],
                vec![CanonicalScalar::Null, CanonicalScalar::Null],
                vec![CanonicalScalar::Null, CanonicalScalar::Null],
            ]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unicode_scalar_offsets_case_expansion_and_unicode_whitespace_are_observable() {
    let ((), report) = run_async_under_lab(0x7e87_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![(T, text("Aé🦀e\u{301}Z"))]);
        batch.create_vertex(VId(2), vec![], vec![(T, text("\u{2003}Straße\u{a0}"))]);
        batch.create_vertex(VId(3), vec![], vec![(T, text("Iİß"))]);
        db.write(&commit, batch).await.unwrap();
        let scalar = query(
            "MATCH (n) RETURN n,CHAR_LENGTH(n.t) AS size,SUBSTRING(n.t,2,3) AS middle,SUBSTRING(n.t,20,1) AS beyond,SUBSTRING(n.t,1,0) AS empty ORDER BY n",
        );
        assert_eq!(
            cells(
                &db.execute_graph_set_governed(&cx, &scalar, policy())
                    .unwrap()
                    .value
            ),
            vec![
                vec![CanonicalScalar::Int(6), text("é🦀e"), text(""), text("")],
                vec![CanonicalScalar::Int(8), text("Str"), text(""), text("")],
                vec![CanonicalScalar::Int(3), text("İß"), text(""), text("")],
            ]
        );
        // Locale-independent Unicode mapping, not Turkish locale rules or case
        // folding: I -> i, İ -> i + COMBINING DOT ABOVE, ß -> SS on uppercase.
        // Combining marks count separately from their base scalar, not as one
        // grapheme; SUBSTRING uses 1-based scalar offsets, not UTF-8 byte offsets.
        let casing = query(
            "MATCH (n) RETURN n,UPPER(TRIM(n.t)) AS upper,LOWER(TRIM(n.t)) AS lower ORDER BY n",
        );
        assert_eq!(
            cells(
                &db.execute_graph_set_governed(&cx, &casing, policy())
                    .unwrap()
                    .value
            ),
            vec![
                vec![text("AÉ🦀E\u{301}Z"), text("aé🦀e\u{301}z")],
                vec![text("STRASSE"), text("straße")],
                vec![text("IİSS"), text("ii\u{307}ß")],
            ]
        );
        let filtered =
            query("MATCH (n) WHERE TRIM(n.t) = 'Straße' AND CHAR_LENGTH(TRIM(n.t)) = 6 RETURN n");
        assert_eq!(
            ids(&db
                .execute_graph_set_governed(&cx, &filtered, policy())
                .unwrap()
                .value),
            vec![VId(2)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_text_predicates_and_projections_use_the_selected_committed_version() {
    let ((), report) = run_async_under_lab(0x7e87_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let first = seed(&mut db, &commit).await;
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), T, Some(text("changed")));
        let second = db.write(&commit, change).await.unwrap();
        let template = PreparedTemporalGraphSetText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.t CONTAINS 'Alpha' RETURN n,UPPER(n.t) || ':' || SUBSTRING(n.t,2,2) AS value,CHAR_LENGTH(n.t) AS size ORDER BY n",
            symbols,
        ).unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (seq, expected_ids, expected_cells) in [
            (
                first,
                vec![VId(1)],
                vec![vec![text("ALPHAΒ:lp"), CanonicalScalar::Int(6)]],
            ),
            (second, vec![], vec![]),
        ] {
            let bound = template
                .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
                .unwrap();
            let result = db
                .execute_temporal_graph_set_text_governed(&cx, &bound, policy())
                .unwrap();
            assert_eq!(ids(&result.value), expected_ids);
            assert_eq!(cells(&result.value), expected_cells);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn query_selected_text_assignments_are_frozen_staged_and_durably_committed() {
    let ((), report) = run_async_under_lab(0x7e87_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let read = query("MATCH (n) RETURN n,n.t AS t,n.u AS u ORDER BY n");
        let before = db
            .execute_graph_set_governed(&cx, &read, policy())
            .unwrap()
            .value;
        let mutation = PreparedGraphMutationText::prepare(
            "MATCH (n) WHERE LOWER(n.t) STARTS WITH 'alpha' SET n.t=UPPER(TRIM(n.t)) || '!',n.u=SUBSTRING(n.t,2,2)",
            R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let result = txn
            .execute_graph_mutation_governed(
                &mut db,
                &cx,
                &mutation,
                GraphMutationPolicy::new(policy(), 100),
            )
            .unwrap();
        assert_eq!(result.target_vertices, 2);
        assert_eq!(result.effects, 4);
        let expected = vec![
            vec![text("ALPHAΒ!"), text("lp")],
            vec![text("ALPHAΒ!"), text("lp")],
            vec![text(""), text("")],
            vec![CanonicalScalar::Null, text("x")],
            vec![text("x"), CanonicalScalar::Null],
            vec![CanonicalScalar::Null, CanonicalScalar::Null],
        ];
        assert_eq!(
            cells(
                &txn.execute_graph_set_governed(&db, &cx, &read, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &read, policy())
                .unwrap()
                .value,
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            cells(
                &db.execute_graph_set_governed(&cx, &read, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            db.execute_graph_set_governed_at(&cx, &read, basis, policy())
                .unwrap()
                .value,
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn governed_rows_and_string_growth_alone_refuse_at_their_typed_limits() {
    let ((), report) = run_async_under_lab(0x7e87_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![(T, text("ß"))]);
        db.write(&commit, batch).await.unwrap();
        let short = query("MATCH (n) RETURN n,CHAR_LENGTH(n.t || 'x') AS value");
        let long = query(&format!(
            "MATCH (n) RETURN n,CHAR_LENGTH(n.t || '{}') AS value",
            "x".repeat(2048)
        ));
        let small = db
            .execute_graph_set_governed(&cx, &short, policy())
            .unwrap();
        assert_eq!(cells(&small.value), vec![vec![CanonicalScalar::Int(2)]]);
        assert!(matches!(
            db.execute_graph_set_governed(
                &cx,
                &short,
                GqlQueryPolicy::new(10_000, 0, 10_000_000, 10_000_000)
            ),
            Err(GqlQueryError::Rows(_))
        ));
        // Fixed stored input, tree shape, instruction count and integer result
        // shape. Only the constructed intermediate text length changes.
        let large = db.execute_graph_set_governed(&cx, &long, policy()).unwrap();
        assert_eq!(cells(&large.value), vec![vec![CanonicalScalar::Int(2049)]]);
        assert_eq!(large.rows, small.rows);
        for (dimension, allowance) in [
            (
                GlaLimitDimension::WorkUnits,
                GqlQueryPolicy::new(10_000, 10_000, small.evaluator.work_units, 10_000_000),
            ),
            (
                GlaLimitDimension::ScratchEntries,
                GqlQueryPolicy::new(10_000, 10_000, 10_000_000, small.evaluator.scratch_entries),
            ),
        ] {
            assert_eq!(
                db.execute_graph_set_governed(&cx, &short, allowance)
                    .unwrap(),
                small
            );
            let error = db
                .execute_graph_set_governed(&cx, &long, allowance)
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Evaluator(limit) if limit.dimension == dimension),
                "{dimension:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
