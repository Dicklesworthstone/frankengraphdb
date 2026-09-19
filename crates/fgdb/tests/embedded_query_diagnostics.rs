//! Typed refusal selection and robot diagnostics for embedded native reads.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, NativeReadClass, QueryError, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::*;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x4d; 32],
        DatabaseSecurityNamespaceId([0x4e; 32]),
        [0x4f; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p" | "since") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

// Marker ranges identify the actual unsupported construct, not a tolerance
// window around an unrelated token. The source retains its typed error kind.
const CASES: &[(&str, NativeReadClass, &str)] = &[
    (
        "MATCH (a:Person)-[k:R]->(b:Person) RETURN k.since.",
        NativeReadClass::Pattern,
        "since.",
    ),
    (
        "MATCH (a:Person)-[k:R]->(b:Person) WHERE k.since > RETURN a.p",
        NativeReadClass::Pattern,
        "RETURN",
    ),
    (
        "MATCH (a:Person)-[:R]->(m)-[:R]->(b:Person) RETURN b.p.",
        NativeReadClass::Pattern,
        "p.",
    ),
    (
        "MATCH (a:Person)-[r:R]->(b:Person) RETURN r.since.",
        NativeReadClass::Pattern,
        "since.",
    ),
    (
        "MATCH (p:Person) WHERE EXISTS { (p)-[:R]->() } RETURN p.p",
        NativeReadClass::Pattern,
        "EXISTS { (p)-[:R]->() }",
    ),
    (
        "MATCH (p:Person) WHERE NOT EXISTS { (p)-[:R]->() } RETURN p.p",
        NativeReadClass::Pattern,
        "NOT EXISTS { (p)-[:R]->() }",
    ),
    (
        "MATCH (p:Person) RETURN p.p AS x UNION ALL MATCH (q:Person) RETURN q.p AS x ORDER BY missing",
        NativeReadClass::Set,
        "missing",
    ),
    (
        "MATCH (a:Person) INSERT (a)-[:R]->(b:Person), (a)-[:S]->(c:Person)",
        NativeReadClass::Pattern,
        "INSERT",
    ),
    (
        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ nope RETURN n.p",
        NativeReadClass::TemporalPattern,
        "nope",
    ),
    (
        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p FOR SYSTEM_TIME AS OF SEQ 2",
        NativeReadClass::TemporalPattern,
        "FOR SYSTEM_TIME AS OF SEQ 2",
    ),
    (
        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ -1 RETURN n.p",
        NativeReadClass::TemporalPattern,
        "-1",
    ),
];

#[test]
fn refusal_names_furthest_progressing_facade_error() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for &(text, expected, marker) in CASES {
            let error = db
                .query(
                    &contexts.query(),
                    text,
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                )
                .unwrap_err();
            let QueryError::Refused { facade, source } = &error else {
                panic!("{text}: expected typed facade refusal, got {error:?}");
            };
            assert_eq!(*facade, expected, "{text}: {error}");
            let offset = match source.as_ref() {
                QueryError::PatternText(e) => e.offset,
                QueryError::SetText(e) => e.offset,
                QueryError::PipelineText(e) => e.offset,
                QueryError::TemporalText(e) => {
                    assert_ne!(e.kind, GraphTemporalTextErrorKind::MissingSystemTimeClause);
                    e.offset
                }
                QueryError::TemporalSetText(e) => e.offset,
                other => panic!("not a typed parser error: {other:?}"),
            };
            let start = text.find(marker).unwrap();
            assert!(
                offset > 0 && (start..=start + marker.len()).contains(&offset),
                "{text}: offset {offset} outside {marker:?}: {error}"
            );
            assert!(
                error.to_string().contains(&format!("byte {offset}:")),
                "{error}"
            );
        }
    });
}

#[test]
fn accepted_statements_still_accept() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let mut batch = fgdb::WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(7))]);
        batch.create_vertex(VId(2), vec![PERSON], vec![(P, CanonicalScalar::Int(9))]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![(P, CanonicalScalar::Int(11))]);
        batch.add_edge(EId(2), VId(2), VId(1), vec![(P, CanonicalScalar::Int(2))]);
        db.write(&contexts.commit(), batch).await.unwrap();
        let accepted = [
            "MATCH (n:Person) RETURN n.p",
            "MATCH (n:Person) WHERE n.p > 5 RETURN n.p AS value ORDER BY value DESC",
            "MATCH (n:Person) RETURN COUNT(*) AS c",
            "MATCH (a:Person)-[r:R]->(b:Person) RETURN a.p AS ap, b.p AS bp",
            "MATCH (a:Person) RETURN a.p AS p UNION ALL MATCH (b:Person) RETURN b.p AS p",
            "MATCH (n:Person) WITH n.p AS x RETURN SUM_INT(x) AS s",
            "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p",
            "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS c",
            "MATCH (a:Person) FOR SYSTEM_TIME AS OF SEQ 1 RETURN a.p AS p UNION ALL MATCH (b:Person) RETURN b.p AS p",
            "MATCH (p:Person) RETURN labels(p)",
            "MATCH (a:Person)-[r:R]->(b:Person) RETURN type(r)",
        ];
        for (text, column, values) in [
            ("MATCH (a:Person)-[k:R]->(b:Person) RETURN k.since ORDER BY k.since", "since", vec![2, 11]),
            ("MATCH (a:Person)-[k:R]->(b:Person) WHERE k.since > 3 RETURN a.p", "p", vec![7]),
            ("MATCH (a:Person)-[:R]->()-[:R]->(b:Person) RETURN b.p ORDER BY b.p", "p", vec![7, 9]),
        ] {
            let result = db.query(&contexts.query(), text, &GqlParameters::new(), symbols, policy()).unwrap();
            assert_eq!(result, QueryResult::Rows {
                columns: vec![column.to_owned()],
                rows: values.into_iter().map(|n| vec![QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(n)))]).collect(),
            }, "{text}");
        }
        for text in accepted {
            db.query(&contexts.query(), text, &GqlParameters::new(), symbols, policy())
                .unwrap_or_else(|error| panic!("{text} must still accept: {error}"));
        }
    });
}

#[test]
fn robot_error_event_carries_furthest_progress_message() {
    let binary = env!("CARGO_BIN_EXE_fgdb");
    let dir = std::env::temp_dir().join(format!("fgdb-diag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("keys.txt");
    std::fs::write(
        &key,
        format!("{:064x}\n{:064x}\n{:064x}\n", 0x4du64, 0x4eu64, 0x4fu64),
    )
    .unwrap();
    let db_path = dir.join("diag.db");
    let create = std::process::Command::new(binary)
        .args([
            "--robot",
            "create",
            "--db",
            db_path.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(create.status.code(), Some(0), "{create:?}");
    let query = std::process::Command::new(binary)
        .args([
            "--robot",
            "query",
            "--db",
            db_path.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--label",
            "Person=1",
            "--relation",
            "R=1",
            "--property",
            "since=1",
            CASES[0].0,
        ])
        .output()
        .unwrap();
    assert_eq!(query.status.code(), Some(3), "{query:?}");
    let stdout = String::from_utf8(query.stdout).unwrap();
    let line = stdout
        .lines()
        .find(|line| line.contains(r#""event":"error""#))
        .expect("robot error event");
    assert!(line.contains(r#""class":"query""#), "{line}");
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let expected = runtime.block_on(async {
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.query(
            &contexts.query(),
            CASES[0].0,
            &GqlParameters::new(),
            symbols,
            policy(),
        )
        .unwrap_err()
        .to_string()
    });
    let escaped = expected.replace('\\', "\\\\").replace('"', "\\\"");
    assert!(
        line.contains(&format!("\"diagnostics\":[\"{escaped}\"]")),
        "{line}"
    );
}
