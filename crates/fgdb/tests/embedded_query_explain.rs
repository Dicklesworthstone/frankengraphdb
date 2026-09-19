//! EXPLAIN certificate differentials for the native `Database::query` reads.
//!
//! Every EXPLAIN exercises exactly the same native preparation as execution,
//! then proves no snapshot record is read, certificates are value-independent,
//! type-sensitive, snapshot-bound, and cover all seven native read classes.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeExplainCertificate, NativeReadClass, QueryError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::*;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x74; 32],
        DatabaseSecurityNamespaceId([0x75; 32]),
        [0x76; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

/// Two independently opened databases sharing one identical seeded history.
async fn seeded_pair(cx: &CommitCx) -> (Database<MemVfs>, Database<MemVfs>) {
    let seed = async |cx: &CommitCx| {
        let mut db = Database::open_memory(cx, keys()).await.unwrap();
        let mut batch = fgdb::WriteBatch::new(R);
        for (id, p) in [(1, 10), (2, 20), (3, 30)] {
            batch.create_vertex(VId(id), vec![PERSON], vec![(P, CanonicalScalar::Int(p))]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
        db
    };
    let first = seed(cx).await;
    let second = seed(cx).await;
    (first, second)
}

/// The seven native read classes with >=2 representative statements each,
/// mirroring the grammar-valid forms proven by the entrypoint differentials.
const READS: [(&str, NativeReadClass, [&str; 2]); 7] = [
    (
        "pattern",
        NativeReadClass::Pattern,
        [
            "MATCH (a)-[:R]->(b) RETURN b.p AS score ORDER BY score DESC",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN ALL a,c.p AS score",
        ],
    ),
    (
        "aggregate",
        NativeReadClass::Aggregate,
        [
            "MATCH (n) RETURN COUNT(*) AS count,SUM(n.p) AS total,AVG(n.p) AS mean",
            "MATCH (n) RETURN ABS(n.p) AS bucket,SUM(n.p*n.p) AS total,COUNT(DISTINCT n.p) AS different GROUP BY ABS(n.p) ORDER BY bucket DESC",
        ],
    ),
    (
        "pipeline aggregate",
        NativeReadClass::PipelineAggregate,
        [
            "MATCH (n) WITH n.p AS x RETURN SUM_INT(x) AS total,AVG_INT(x) AS mean",
            "MATCH (n) WITH n.p AS x ORDER BY x DESC LIMIT 2 RETURN COUNT(*) AS count,SUM(x) AS total,AVG(x) AS mean",
        ],
    ),
    (
        "set",
        NativeReadClass::Set,
        [
            "MATCH (a) WHERE a.p >= 2 RETURN a.p AS p UNION MATCH (b) WHERE b.p <= 3 RETURN b.p AS p ORDER BY p DESC",
            "MATCH (a) RETURN a.p AS p EXCEPT ALL MATCH (b) WHERE b.p = 2 RETURN b.p AS p ORDER BY p",
        ],
    ),
    (
        "temporal pattern",
        NativeReadClass::TemporalPattern,
        [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p ORDER BY p",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= 2 RETURN n.p AS p ORDER BY p DESC",
        ],
    ),
    (
        "temporal set",
        NativeReadClass::TemporalSet,
        [
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p*2 AS p UNION DISTINCT MATCH (b) WHERE b.p <= 3 RETURN b.p*2 AS p ORDER BY p",
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p AS p INTERSECT MATCH (b) WHERE b.p <= 3 RETURN b.p AS p EXCEPT MATCH (c) WHERE c.p = 2 RETURN c.p AS p ORDER BY p",
        ],
    ),
    (
        "temporal aggregate",
        NativeReadClass::TemporalAggregate,
        [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS c,SUM(n.p*2) AS s",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN ABS(n.p) AS bucket,COUNT(*) AS c GROUP BY ABS(n.p) ORDER BY bucket DESC",
        ],
    ),
];
fn explain(
    db: &Database<MemVfs>,
    text: &str,
    params: &GqlParameters,
    certificate: bool,
) -> Result<(Vec<(String, String)>, Option<NativeExplainCertificate>), QueryError> {
    let (rows, cert) = db.explain(text, params, symbols, certificate)?;
    Ok((
        rows.into_iter()
            .map(|row| (row.operator, row.detail))
            .collect(),
        cert,
    ))
}
#[test]
fn explain_refuses_writes_and_covers_every_read_class() {
    let ((), report) = run_async_under_lab(0x9a19_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (db, _) = seeded_pair(&commit).await;
        let cx = contexts.query();
        let params = GqlParameters::new()
            .with_int64("floor", 15)
            .unwrap()
            .with_uint64("at", 1)
            .unwrap();
        for (_name, class, statements) in READS {
            for (statement_index, text) in statements.into_iter().enumerate() {
                let (rows, cert) =
                    explain(&db, text, &params, true).expect("read statement must EXPLAIN");
                let prepared = fgdb::PreparedNativeRead::prepare(text, &params, symbols).unwrap();
                assert_eq!(prepared.facade_class(), class, "{text}");
                assert!(cert.unwrap().verifies(&prepared), "{text}");
                assert!(rows.iter().any(|(operator, _)| operator == "NativeRead"));
                assert!(!rows.iter().any(|(operator, _)| operator == "Template"));
                let expected_operator = match class {
                    NativeReadClass::Pattern | NativeReadClass::TemporalPattern => "Project",
                    NativeReadClass::Aggregate
                    | NativeReadClass::PipelineAggregate
                    | NativeReadClass::TemporalAggregate => "Aggregate",
                    NativeReadClass::Set | NativeReadClass::TemporalSet => {
                        ["Union", "Except"][statement_index]
                    }
                };
                assert!(
                    rows.iter()
                        .any(|(operator, _)| operator == expected_operator),
                    "missing {expected_operator} in {class:?}: {rows:?}"
                );
                if class == NativeReadClass::TemporalSet && statement_index == 1 {
                    assert!(rows.iter().any(|(operator, _)| operator == "Intersect"));
                }
                assert_eq!(db.explain(text, &params, symbols, false).unwrap().1, None);
                for prefix in ["EXPLAIN", "EXPLAIN (CERTIFICATE)"] {
                    let result = db
                        .query(&cx, &format!("{prefix} {text}"), &params, symbols, policy())
                        .unwrap();
                    let fgdb::QueryResult::Rows {
                        columns,
                        rows: result_rows,
                    } = result
                    else {
                        unreachable!("EXPLAIN is a read-only prefix; got {result:?}");
                    };
                    assert_eq!(columns, ["operator", "detail"]);
                    let expected: Vec<Vec<GraphAggregateValue>> = rows
                        .iter()
                        .map(|(operator, detail)| {
                            [operator, detail]
                                .into_iter()
                                .map(|text| {
                                    GraphAggregateValue::Value(
                                        fgdb_gql::algebra::GraphValue::Scalar(
                                            CanonicalScalar::ucs_basic_text(text).unwrap(),
                                        ),
                                    )
                                })
                                .collect()
                        })
                        .collect();
                    assert_eq!(&result_rows[..expected.len()], expected.as_slice());
                    assert_eq!(
                        result_rows.len(),
                        expected.len() + if prefix == "EXPLAIN" { 0 } else { 2 }
                    );
                }
            }
        }
        let err = db
            .explain(
                "INSERT (n:Person) SET n.p = 1",
                &GqlParameters::new(),
                symbols,
                false,
            )
            .unwrap_err();
        assert!(matches!(err, QueryError::Refused { .. }), "{err:?}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
#[test]
fn explain_never_executes_and_never_charges_the_budget() {
    let ((), report) = run_async_under_lab(0x9a19_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (db, _) = seeded_pair(&commit).await;
        let params = GqlParameters::new();
        let tight = GqlQueryPolicy::new(1, 1, 1, 1);
        let (rows, cert) = db
            .explain("MATCH (n:Person) RETURN n.p AS p", &params, symbols, true)
            .unwrap();
        let refused = db
            .query(
                &cx,
                "MATCH (n:Person) RETURN n.p AS p",
                &params,
                symbols,
                tight,
            )
            .unwrap_err();
        assert!(matches!(refused, QueryError::Pattern(_)), "{refused:?}");
        assert!(!rows.is_empty() && cert.is_some());
        for prefix in ["explain", " EXPLAIN ( certificate )"] {
            let result = db
                .query(
                    &cx,
                    &format!("{prefix} MATCH (n:Person) RETURN n.p AS p"),
                    &params,
                    symbols,
                    tight,
                )
                .unwrap();
            assert!(matches!(result, fgdb::QueryResult::Rows { .. }));
        }
        for text in [
            "EXPLAIN INSERT (n:Person) SET n.p = 1",
            "EXPLAIN (CERTIFICATE) INSERT (n:Person) SET n.p = 1",
            "EXPLAIN (UNKNOWN) MATCH (n) RETURN n",
            "EXPLAIN (CERTIFICATE",
        ] {
            let error = db.query(&cx, text, &params, symbols, tight).unwrap_err();
            let correct = if text.contains("INSERT") {
                matches!(error, QueryError::Refused { .. })
            } else {
                matches!(error, QueryError::Unsupported { .. })
            };
            assert!(correct, "{text}: {error:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn certificates_are_deterministic_value_free_and_type_sensitive() {
    let ((), report) = run_async_under_lab(0x9a19_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (left, right) = seeded_pair(&commit).await;
        let text = "MATCH (n:Person) WHERE n.p >= $floor RETURN n.p AS p";
        let params = GqlParameters::new().with_int64("floor", 15).unwrap();
        let (_, left_cert) = left.explain(text, &params, symbols, true).unwrap();
        let (_, right_cert) = right.explain(text, &params, symbols, true).unwrap();
        let left_cert = left_cert.unwrap();
        let right_cert = right_cert.unwrap();
        assert_eq!(
            left_cert.digest(),
            right_cert.digest(),
            "identical histories must certify identically"
        );
        assert_eq!(left_cert.canonical_bytes(), right_cert.canonical_bytes());
        assert!(left_cert.verifies_at(
            &fgdb::PreparedNativeRead::prepare(text, &params, symbols).unwrap(),
            left.frontier().unwrap()
        ));

        // Same text, different values: identical digest.
        let (_, changed_value) = left
            .explain(
                text,
                &GqlParameters::new().with_int64("floor", 99).unwrap(),
                symbols,
                true,
            )
            .unwrap();
        assert_eq!(left_cert.digest(), changed_value.unwrap().digest());

        // Same text, different declared type: different digest.
        let string_params = GqlParameters::new()
            .with_scalar("floor", CanonicalScalar::ucs_basic_text("15").unwrap())
            .unwrap();
        let (_, changed_type) = left.explain(text, &string_params, symbols, true).unwrap();
        assert_ne!(left_cert.digest(), changed_type.unwrap().digest());

        // Semantically different statement: different digest.
        let other = "MATCH (n:Person) WHERE n.p >= 15 RETURN n.p AS p";
        let (_, changed_plan) = left
            .explain(other, &GqlParameters::new(), symbols, true)
            .unwrap();
        assert_ne!(left_cert.digest(), changed_plan.unwrap().digest());
        let (_, changed_literal) = left
            .explain(
                "MATCH (n:Person) WHERE n.p >= 16 RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                true,
            )
            .unwrap();
        assert_ne!(
            changed_plan.unwrap().digest(),
            changed_literal.unwrap().digest()
        );

        // Snapshot binding: refuses a different frontier.
        assert!(left_cert.snapshot_seq() == left.frontier().unwrap());
        assert!(!left_cert.verifies_at(
            &fgdb::PreparedNativeRead::prepare(text, &params, symbols).unwrap(),
            CommitSeq(left_cert.snapshot_seq().0 + 1)
        ));
        // Mutated prepared statement fails verification.
        let mutated = fgdb::PreparedNativeRead::prepare(
            "MATCH (n:Person) WHERE n.p >= 16 RETURN n.p AS p",
            &GqlParameters::new(),
            symbols,
        )
        .unwrap();
        assert!(!left_cert.verifies(&mutated));
        // A write statement under EXPLAIN refuses; a read row listing stays deterministic.
        assert!(matches!(
            left.explain(
                "INSERT (n:Person) SET n.p = 1",
                &GqlParameters::new(),
                symbols,
                true
            ),
            Err(QueryError::Refused { .. })
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn certificates_bind_resolved_symbols_not_query_spelling() {
    let args = GqlParameters::new().with_int64("floor", 15).unwrap();
    let text = "MATCH (n:Person) WHERE n.p >= $floor RETURN n.p AS p";
    let original = fgdb::PreparedNativeRead::prepare(text, &args, symbols).unwrap();
    let certificate = NativeExplainCertificate::new(&original, CommitSeq(1));
    let changed =
        fgdb::PreparedNativeRead::prepare(text, &args, |kind: GraphSymbolKind, name: &str| {
            if kind == GraphSymbolKind::Property && name == "p" {
                Some(GraphSymbol::Property(PropertyKeyId(2)))
            } else {
                symbols(kind, name)
            }
        })
        .unwrap();
    assert!(
        !certificate.verifies(&changed),
        "same spelling must not certify a different resolved property"
    );
    let reformatted = fgdb::PreparedNativeRead::prepare(
        "MATCH  (n:Person)  WHERE n.p >= $floor  RETURN n.p AS p",
        &args,
        symbols,
    )
    .unwrap();
    assert!(
        certificate.verifies(&reformatted),
        "whitespace does not change the resolved plan"
    );
}

#[test]
fn certificates_distinguish_ordering_columns() {
    let args = GqlParameters::new();
    let left = fgdb::PreparedNativeRead::prepare(
        "MATCH (a)-[:R]->(b) RETURN a.p AS x,b.p AS y ORDER BY x",
        &args,
        symbols,
    )
    .unwrap();
    let right = fgdb::PreparedNativeRead::prepare(
        "MATCH (a)-[:R]->(b) RETURN a.p AS x,b.p AS y ORDER BY y",
        &args,
        symbols,
    )
    .unwrap();
    assert_eq!(left.facade_class(), NativeReadClass::Pattern);
    assert_eq!(right.facade_class(), NativeReadClass::Pattern);
    assert!(!NativeExplainCertificate::new(&left, CommitSeq(1)).verifies(&right));
}

#[test]
fn every_read_certificate_binds_resolved_symbols_not_formatting() {
    let args = GqlParameters::new().with_uint64("at", 1).unwrap();
    for (_, class, statements) in READS {
        for text in statements {
            let prepared = fgdb::PreparedNativeRead::prepare(text, &args, symbols).unwrap();
            assert_eq!(prepared.facade_class(), class);
            let certificate = NativeExplainCertificate::new(&prepared, CommitSeq(1));
            let changed = fgdb::PreparedNativeRead::prepare(
                text,
                &args,
                |kind: GraphSymbolKind, name: &str| {
                    if kind == GraphSymbolKind::Property && name == "p" {
                        Some(GraphSymbol::Property(PropertyKeyId(2)))
                    } else {
                        symbols(kind, name)
                    }
                },
            )
            .unwrap();
            assert!(
                !certificate.verifies(&changed),
                "resolved property in {class:?}: {text}"
            );
            let formatted = text.replace(' ', "  ");
            let reformatted =
                fgdb::PreparedNativeRead::prepare(&formatted, &args, symbols).unwrap();
            assert!(
                certificate.verifies(&reformatted),
                "formatting in {class:?}: {text}"
            );
        }
    }
}

#[test]
fn pipeline_explain_lists_only_present_having() {
    let params = GqlParameters::new();
    for (text, has_having) in [
        ("MATCH (n) WITH n.p AS x RETURN SUM_INT(x) AS total", false),
        (
            "MATCH (n) WITH n.p AS x RETURN SUM_INT(x) AS total HAVING total > 1",
            true,
        ),
    ] {
        let prepared = fgdb::PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::PipelineAggregate);
        let fgdb::PreparedNativeRead::PipelineAggregate(plan) = prepared else {
            unreachable!("class checked above");
        };
        assert_eq!(
            plan.template_operators().contains(&"SelectHaving"),
            has_having
        );
    }
}
