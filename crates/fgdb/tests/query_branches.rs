//! Branch routing must change the real immutable source, not merely accept text.
//! The host resolver supplies admitted views; these tests do not invent a
//! durable branch catalog or claim that a retained historical view is a fork.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EmbeddedReadView, MemVfs, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const NAME: PropertyKeyId = PropertyKeyId(1);
const AGE: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "age") => Some(GraphSymbol::Property(AGE)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}

fn refused() -> QueryError {
    QueryError::Unsupported {
        diagnostics: vec!["branch lookup refused".to_owned()],
    }
}

fn row_count(result: &QueryResult) -> usize {
    match result {
        QueryResult::Rows { rows, .. } => rows.len(),
        QueryResult::Write { .. } => panic!("read-only branch routing returned a write"),
    }
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> EmbeddedReadView {
    let mut batch = WriteBatch::new(R);
    for (id, name, age) in [(1, "Ada", 10), (2, "Grace", 20), (3, "Edsger", 30)] {
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![
                (NAME, CanonicalScalar::ucs_basic_text(name).unwrap()),
                (AGE, CanonicalScalar::Int(age)),
            ],
        );
    }
    for (id, source, destination) in [(10, 1, 2), (11, 2, 3), (12, 1, 3)] {
        batch.add_edge(EId(id), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db.read_session().unwrap()
}

async fn advance(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(4),
        vec![PERSON],
        vec![
            (NAME, CanonicalScalar::ucs_basic_text("Barbara").unwrap()),
            (AGE, CanonicalScalar::Int(40)),
        ],
    );
    batch.add_edge(EId(13), VId(3), VId(4), vec![]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn branch_views_drive_properties_aggregates_paths_and_compound_reads() {
    let ((), report) = run_async_under_lab(0xb2a0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let selected = seed(&mut db, &commit).await;
        advance(&mut db, &commit).await;
        let params = GqlParameters::new();
        let statements = [
            "MATCH (n:Person) RETURN n.name",
            "MATCH (n:Person) RETURN count(*) AS total, sum(n.age) AS ages",
            "MATCH ALL (a {age:10})-[:R]->{1,3}(b) RETURN b.name",
            "MATCH SHORTEST (a {age:10})-[:R]->{1,3}(b) RETURN b.name",
            "MATCH (n:Person) RETURN n.name AS name UNION ALL MATCH (m:Person) RETURN m.name AS name",
        ];
        for statement in statements {
            let expected = selected
                .query(&cx, statement, &params, symbols, policy())
                .unwrap();
            let current = db
                .query(&cx, statement, &params, symbols, policy())
                .unwrap();
            assert_ne!(
                expected, current,
                "fixture must distinguish sources: {statement}"
            );
            let mut calls = 0;
            let result = db
                .query_with_branch_resolver(
                    &cx,
                    &format!("AT BRANCH baseline {statement}"),
                    &params,
                    symbols,
                    |name| {
                        calls += 1;
                        assert_eq!(name, "baseline");
                        Ok(selected.clone())
                    },
                    policy(),
                )
                .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(result, expected, "{statement}");
        }
        let summary = db
            .query_with_branch_resolver(
                &cx,
                "MATCH (n:Person) AT BRANCH baseline RETURN count(*) AS total, sum(n.age) AS ages",
                &params,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = summary else {
            panic!("read became a write")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_count(), Some(3));
        assert_eq!(rows[0][1].as_integer(), Some(60));

        let current = db.read_session().unwrap();
        drop(db);
        let text = "MATCH (n:Person) RETURN n.name AT BRANCH baseline";
        let result = current
            .query_with_branch_resolver(
                &cx,
                text,
                &params,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .unwrap();
        assert_eq!(row_count(&result), 3);
        assert_eq!(
            row_count(
                &current
                    .query(
                        &cx,
                        "MATCH (n:Person) RETURN n.name",
                        &params,
                        symbols,
                        policy(),
                    )
                    .unwrap()
            ),
            4
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn branch_parameters_are_values_and_native_argument_validation_is_preserved() {
    let ((), report) = run_async_under_lab(0xb2a0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let selected = seed(&mut db, &commit).await;
        advance(&mut db, &commit).await;
        let opaque = "baseline' RETURN secrets; MATCH (x)";
        let params = GqlParameters::new()
            .with_text("branch", opaque)
            .unwrap()
            .with_int64("floor", 20)
            .unwrap();
        let original = params.clone();
        let text = "AT BRANCH $branch MATCH (n:Person) WHERE n.age >= $floor RETURN n.name";
        let result = db
            .query_with_branch_resolver(
                &cx,
                text,
                &params,
                symbols,
                |name| {
                    assert_eq!(name, opaque);
                    Ok(selected.clone())
                },
                policy(),
            )
            .unwrap();
        assert_eq!(row_count(&result), 2);
        assert_eq!(params, original);

        let reused = GqlParameters::new().with_text("branch", "Ada").unwrap();
        let result = db
            .query_with_branch_resolver(
                &cx,
                "MATCH AT BRANCH $branch (n:Person) WHERE n.name = $branch RETURN n.name",
                &reused,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = result else {
            panic!("read became a write")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0].as_value().and_then(GraphValue::as_scalar),
            Some(&CanonicalScalar::ucs_basic_text("Ada").unwrap())
        );

        let extra = params.clone().with_int64("unused", 1).unwrap();
        assert!(
            db.query_with_branch_resolver(
                &cx,
                text,
                &extra,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .is_err(),
            "routing must not filter unknown query parameters"
        );
        let missing = GqlParameters::new()
            .with_text("branch", "baseline")
            .unwrap();
        assert!(
            db.query_with_branch_resolver(
                &cx,
                text,
                &missing,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .is_err(),
            "routing must not satisfy missing native arguments"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn default_reads_never_resolve_and_branch_misses_never_fall_back() {
    let ((), report) = run_async_under_lab(0xb2a0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let view = seed(&mut db, &commit).await;
        let params = GqlParameters::new();
        let text = "MATCH (n:Person) RETURN n.name";
        let expected = view.query(&cx, text, &params, symbols, policy()).unwrap();
        assert_eq!(
            db.query_with_branch_resolver(
                &cx,
                text,
                &params,
                symbols,
                |_| panic!("unqualified read resolved a branch"),
                policy(),
            )
            .unwrap(),
            expected
        );
        assert_eq!(
            view.query_with_branch_resolver(
                &cx,
                text,
                &params,
                symbols,
                |_| panic!("unqualified view resolved a branch"),
                policy(),
            )
            .unwrap(),
            expected
        );

        let mut calls = 0;
        let failure = db
            .query_with_branch_resolver(
                &cx,
                "AT BRANCH missing MATCH (n:Person) RETURN n.name LIMIT 0",
                &params,
                symbols,
                |_| {
                    calls += 1;
                    Err(refused())
                },
                policy(),
            )
            .unwrap_err();
        assert_eq!(calls, 1);
        assert!(matches!(failure, QueryError::Unsupported { diagnostics }
            if diagnostics == vec!["branch lookup refused"]));
        assert!(
            db.query(
                &cx,
                "AT BRANCH missing MATCH (n) RETURN n",
                &params,
                symbols,
                policy()
            )
            .is_err()
        );

        for text in [
            "AT BRANCH a MATCH (n) RETURN n AT BRANCH b",
            "AT BRANCH $b MATCH (n) RETURN n",
            "AT BRANCH '' MATCH (n) RETURN n",
        ] {
            assert!(
                db.query_with_branch_resolver(
                    &cx,
                    text,
                    &params,
                    symbols,
                    |_| panic!("invalid selector reached branch resolution"),
                    policy(),
                )
                .is_err()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_and_budget_refusals_belong_to_the_selected_view_and_writes_stay_refused() {
    let ((), report) = run_async_under_lab(0xb2a0_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let selected = seed(&mut db, &commit).await;
        advance(&mut db, &commit).await;
        let params = GqlParameters::new();
        let at_selected = format!(
            "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.name",
            selected.frontier().0,
        );
        let expected = selected
            .query(&cx, &at_selected, &params, symbols, policy())
            .unwrap();
        assert_eq!(
            db.query_with_branch_resolver(
                &cx,
                &format!("AT BRANCH baseline {at_selected}"),
                &params,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .unwrap(),
            expected
        );
        let future = format!(
            "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.name",
            db.frontier().unwrap().0,
        );
        assert!(db.query(&cx, &future, &params, symbols, policy()).is_ok());
        assert!(
            db.query_with_branch_resolver(
                &cx,
                &format!("AT BRANCH baseline {future}"),
                &params,
                symbols,
                |_| Ok(selected.clone()),
                policy(),
            )
            .is_err(),
            "selected history must not borrow a later default frontier"
        );

        let before = db.frontier().unwrap();
        for text in [
            "AT BRANCH baseline MATCH (n:Person) SET n.age = 99",
            "AT BRANCH baseline MATCH (n:Person) DELETE n",
            "AT BRANCH baseline INSERT (n:Person {age:99})",
        ] {
            assert!(
                db.query_with_branch_resolver(
                    &cx,
                    text,
                    &params,
                    symbols,
                    |_| Ok(selected.clone()),
                    policy(),
                )
                .is_err(),
                "read-only branch facade accepted a write"
            );
        }
        assert_eq!(db.frontier().unwrap(), before);
        let limited = GqlQueryPolicy::new(1_000, 0, 1_000_000, 1_000_000);
        let plain = "MATCH (n:Person) RETURN n.name";
        let expected_error = selected
            .query(&cx, plain, &params, symbols, limited)
            .unwrap_err();
        let branch_error = db
            .query_with_branch_resolver(
                &cx,
                &format!("AT BRANCH baseline {plain}"),
                &params,
                symbols,
                |_| Ok(selected.clone()),
                limited,
            )
            .unwrap_err();
        assert_eq!(format!("{branch_error:?}"), format!("{expected_error:?}"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
