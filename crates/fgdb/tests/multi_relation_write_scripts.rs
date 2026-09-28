//! Native multi-relation writes share one durable boundary. Compare complete
//! caller-keyed graphs and GQL answers with separately committed statements.
//! Recovery models both surviving marker bytes and an explicitly torn tail.

use std::collections::BTreeMap;

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, QueryResult, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteIdentityRequest,
    GraphWriteProgramPolicy, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const KNOWS: RelationId = RelationId(1);
const LIVES_IN: RelationId = RelationId(2);
const WORKS_AT: RelationId = RelationId(3);
const NAME: PropertyKeyId = PropertyKeyId(1);
const SINCE: PropertyKeyId = PropertyKeyId(2);

type Properties = Vec<(PropertyKeyId, CanonicalScalar)>;
type LogicalVertex = (CanonicalScalar, Vec<LabelId>, Properties);
type LogicalEdge = (RelationId, CanonicalScalar, CanonicalScalar, Properties);

#[derive(Debug, Default, PartialEq, Eq)]
struct LogicalGraph {
    vertices: Vec<LogicalVertex>,
    edges: Vec<LogicalEdge>,
}

fn keys(byte: u8) -> DatabaseKeys {
    DatabaseKeys::new(
        [byte; 32],
        DatabaseSecurityNamespaceId([byte + 1; 32]),
        [byte + 2; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(KNOWS)),
        (GraphSymbolKind::Relation, "LIVES_IN") => Some(GraphSymbol::Relation(LIVES_IN)),
        (GraphSymbolKind::Relation, "WORKS_AT") => Some(GraphSymbol::Relation(WORKS_AT)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "City") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Label, "Company") => Some(GraphSymbol::Label(LabelId(3))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "since") => Some(GraphSymbol::Property(SINCE)),
        _ => None,
    }
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 100, 50, 50)
}

fn vertex_patterns(seed: u64) -> String {
    format!(
        "(a:Person {{name:'A{seed}', since:{}}}), \
         (b:Person {{name:'B{seed}', since:{}}}), \
         (c:City {{name:'C{seed}', since:{}}}), \
         (w:Company {{name:'W{seed}', since:{}}})",
        10 + seed,
        20 + seed,
        30 + seed,
        40 + seed,
    )
}

fn multi_statement(seed: u64, verb: &str) -> String {
    format!(
        "{verb} {}, \
         (a)-[:KNOWS {{name:'K{seed}', since:{}}}]->(b), \
         (a)-[:LIVES_IN {{name:'L{seed}', since:{}}}]->(c), \
         (b)-[:WORKS_AT {{name:'W{seed}', since:{}}}]->(w);",
        vertex_patterns(seed),
        2000 + seed,
        2100 + seed,
        2200 + seed,
    )
}

fn relation_statements(seed: u64, verb: &str) -> Vec<(RelationId, String)> {
    [
        (
            KNOWS, "KNOWS", "a", "Person", "A", "b", "Person", "B", "K", 2000,
        ),
        (
            LIVES_IN, "LIVES_IN", "a", "Person", "A", "c", "City", "C", "L", 2100,
        ),
        (
            WORKS_AT, "WORKS_AT", "b", "Person", "B", "w", "Company", "W", "W", 2200,
        ),
    ]
    .into_iter()
    .map(
        |(relation, name, src, src_label, src_key, dst, dst_label, dst_key, edge_key, since)| {
            let pattern = if verb == "MERGE" {
                format!(
                    "MERGE ({src})-[e:{name}]->({dst}) \
                 ON CREATE SET e.name='{edge_key}{seed}', e.since={}",
                    since + seed,
                )
            } else {
                format!(
                    "{verb} ({src})-[:{name} {{name:'{edge_key}{seed}', since:{}}}]->({dst})",
                    since + seed,
                )
            };
            (
                relation,
                format!(
                    "MATCH ({src}:{src_label}), ({dst}:{dst_label}) \
             WHERE {src}.name='{src_key}{seed}' AND {dst}.name='{dst_key}{seed}' {pattern};"
                ),
            )
        },
    )
    .collect()
}

fn identity_counter(
    offset: u128,
) -> impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    let mut vertex = offset;
    let mut edge = offset;
    move |request| {
        Ok(match request.request {
            fgdb_gql::insertion::GraphInsertRequest::Vertex { .. } => {
                vertex += 1;
                ElementId::Vertex(VId(vertex))
            }
            fgdb_gql::insertion::GraphInsertRequest::Edge { .. } => {
                edge += 1;
                ElementId::Edge(EId(edge))
            }
        })
    }
}

fn graph<V: Vfs + Clone>(db: &Database<V>) -> LogicalGraph {
    let rows = db.vertices().unwrap();
    let names: BTreeMap<_, _> = rows
        .iter()
        .map(|row| {
            let name = row
                .props
                .iter()
                .find(|(key, _)| *key == NAME)
                .unwrap()
                .1
                .clone();
            (row.vid, name)
        })
        .collect();
    let mut vertices: Vec<_> = rows
        .into_iter()
        .map(|row| (names[&row.vid].clone(), row.labels, row.props))
        .collect();
    vertices.sort_unstable();
    let mut edges: Vec<_> = db
        .edges()
        .unwrap()
        .into_iter()
        .map(|record| {
            (
                record.entry.relation,
                names[&record.entry.src].clone(),
                names[&record.entry.dst].clone(),
                record.props,
            )
        })
        .collect();
    edges.sort_unstable();
    LogicalGraph { vertices, edges }
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn properties(name: String, since: u64) -> Properties {
    vec![
        (NAME, text(&name)),
        (SINCE, CanonicalScalar::Int(since as i64)),
    ]
}

fn expected_graph(seed: u64) -> LogicalGraph {
    let mut vertices: Vec<_> = [("A", 1, 10), ("B", 1, 20), ("C", 2, 30), ("W", 3, 40)]
        .into_iter()
        .map(|(name, label, since)| {
            (
                text(&format!("{name}{seed}")),
                vec![LabelId(label)],
                properties(format!("{name}{seed}"), since + seed),
            )
        })
        .collect();
    vertices.sort_unstable();
    let edges = [
        (KNOWS, "A", "B", "K", 2000),
        (LIVES_IN, "A", "C", "L", 2100),
        (WORKS_AT, "B", "W", "W", 2200),
    ]
    .into_iter()
    .map(|(relation, src, dst, name, since)| {
        (
            relation,
            text(&format!("{src}{seed}")),
            text(&format!("{dst}{seed}")),
            properties(format!("{name}{seed}"), since + seed),
        )
    })
    .collect();
    LogicalGraph { vertices, edges }
}

fn answers<V: Vfs + Clone>(db: &Database<V>, contexts: &PurposeContexts) -> Vec<QueryResult> {
    [
        "MATCH (n) RETURN n.name AS name, n.since AS since ORDER BY name",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS source, b.name AS target ORDER BY source",
        "MATCH (a:Person)-[:LIVES_IN]->(c:City) RETURN a.name AS source, c.name AS target ORDER BY source",
        "MATCH (b:Person)-[:WORKS_AT]->(w:Company) RETURN b.name AS source, w.name AS target ORDER BY source",
    ]
    .into_iter()
    .map(|query| {
        let answer = db.query(&contexts.query(), query, &GqlParameters::new(), symbols, query_policy()).unwrap();
        assert!(matches!(answer, QueryResult::Rows { .. }), "{query}: {answer:?}");
        answer
    })
    .collect()
}

async fn run_scripts<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    scripts: &[(RelationId, String)],
    allocate: &mut impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, ()>,
) {
    for (relation, script) in scripts {
        let before = db.frontier().unwrap();
        let prepared = PreparedGraphWriteScript::prepare(script, *relation, symbols).unwrap();
        let (_, completion) = db
            .execute_graph_write_script_autocommit_governed(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &prepared,
                &GqlParameters::new(),
                policy(),
                &mut *allocate,
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap(), CommitSeq(before.0 + 1), "{script}");
    }
}

#[test]
fn insert_and_create_multi_relation_statements_match_split_commits_across_seeds() {
    for seed in 0..3_u64 {
        let ((), report) = run_async_under_lab(0x9e57_0001 + seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            for verb in ["INSERT", "CREATE"] {
                let mut multi = Database::open_memory(&contexts.commit(), keys(0x61))
                    .await
                    .unwrap();
                let mut split = Database::open_memory(&contexts.commit(), keys(0x62))
                    .await
                    .unwrap();
                let mut multi_ids = identity_counter(0);
                // Deliberately different IDs prove the comparison uses caller names.
                let mut split_ids = identity_counter(10_000);
                run_scripts(
                    &mut multi,
                    &contexts,
                    &[(KNOWS, multi_statement(seed, verb))],
                    &mut multi_ids,
                )
                .await;
                let mut statements = vec![(KNOWS, format!("{verb} {};", vertex_patterns(seed)))];
                statements.extend(relation_statements(seed, verb));
                run_scripts(&mut split, &contexts, &statements, &mut split_ids).await;
                assert_eq!(graph(&multi), expected_graph(seed), "{verb}, seed {seed}");
                assert_eq!(graph(&multi), graph(&split), "{verb}, seed {seed}");
                assert_eq!(
                    answers(&multi, &contexts),
                    answers(&split, &contexts),
                    "{verb}, seed {seed}"
                );
            }
        });
        assert!(report.lab_test_passed(), "seed {seed}: {report:?}");
    }
}

#[test]
fn three_relation_merge_script_matches_split_commits_across_seeds() {
    for seed in 0..3_u64 {
        let ((), report) = run_async_under_lab(0x9e57_0010 + seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut multi = Database::open_memory(&contexts.commit(), keys(0x61))
                .await
                .unwrap();
            let mut split = Database::open_memory(&contexts.commit(), keys(0x62))
                .await
                .unwrap();
            let mut multi_ids = identity_counter(0);
            let mut split_ids = identity_counter(10_000);
            let vertices = [(KNOWS, format!("INSERT {};", vertex_patterns(seed)))];
            run_scripts(&mut multi, &contexts, &vertices, &mut multi_ids).await;
            run_scripts(&mut split, &contexts, &vertices, &mut split_ids).await;
            let statements = relation_statements(seed, "MERGE");
            let script = statements
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            run_scripts(&mut multi, &contexts, &[(KNOWS, script)], &mut multi_ids).await;
            run_scripts(&mut split, &contexts, &statements, &mut split_ids).await;
            assert_eq!(graph(&multi), expected_graph(seed), "seed {seed}");
            assert_eq!(graph(&multi), graph(&split), "seed {seed}");
            assert_eq!(
                answers(&multi, &contexts),
                answers(&split, &contexts),
                "seed {seed}"
            );
        });
        assert!(report.lab_test_passed(), "seed {seed}: {report:?}");
    }
}

#[test]
fn multi_relation_statement_recovers_complete_graph_or_original_graph_at_marker_boundary() {
    check_multi_relation_recovery(false);
}

#[test]
fn direct_insert_recovers_all_relations_in_one_commit() {
    check_multi_relation_recovery(true);
}

fn check_multi_relation_recovery(direct_insert: bool) {
    let ((), report) = run_async_under_lab(0x9e57_0020, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txcx = contexts.txn();
        for (name, crash, tear, committed) in [
            ("before", Some(CrashPoint::BeforeCapsule), false, false),
            (
                "capsule",
                Some(CrashPoint::AfterCapsuleBeforeD1),
                false,
                false,
            ),
            ("d1", Some(CrashPoint::AfterD1), false, false),
            (
                "marker-survived",
                Some(CrashPoint::AfterMarkerBeforeD2),
                false,
                true,
            ),
            (
                "marker-torn",
                Some(CrashPoint::AfterMarkerBeforeD2),
                true,
                false,
            ),
            (
                "marker-synced",
                Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync),
                false,
                true,
            ),
            ("complete", None, false, true),
        ] {
            // Use UnixVfs because the existing torn-log helper operates on a path.
            let path = std::env::temp_dir().join(format!(
                "fgdb-multi-relation-script-{}-{direct_insert}-{name}",
                std::process::id(),
            ));
            let mut db = Database::create(&commit, &path, keys(0x63)).await.unwrap();
            let mut ids = identity_counter(0);
            let mut prefix = vec![(KNOWS, format!("INSERT {};", vertex_patterns(99)))];
            prefix.extend(relation_statements(99, "INSERT"));
            run_scripts(&mut db, &contexts, &prefix, &mut ids).await;
            let before = graph(&db);
            assert_eq!(before, expected_graph(99));
            let before_answers = answers(&db, &contexts);
            let basis = db.frontier().unwrap();

            let mut reference = Database::open_memory(&commit, keys(0x64)).await.unwrap();
            let mut reference_ids = identity_counter(10_000);
            run_scripts(&mut reference, &contexts, &prefix, &mut reference_ids).await;
            let mut suffix = vec![(KNOWS, format!("INSERT {};", vertex_patterns(7)))];
            suffix.extend(relation_statements(7, "INSERT"));
            run_scripts(&mut reference, &contexts, &suffix, &mut reference_ids).await;
            let after = graph(&reference);
            let after_answers = answers(&reference, &contexts);

            let mut txn = db.begin(&txcx).unwrap();
            if direct_insert {
                let prepared = fgdb_gql::PreparedGraphInsertText::prepare(
                    multi_statement(7, "INSERT").trim_end_matches(';'),
                    KNOWS,
                    symbols,
                )
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
                let mut vertex = 1_000;
                let mut edge = 1_000;
                txn.execute_graph_insert_governed(
                    &mut db,
                    &contexts.query(),
                    &prepared,
                    fgdb_gql::insertion::GraphInsertPolicy::new(query_policy(), 50, 50),
                    |request| -> Result<ElementId, ()> {
                        Ok(match request {
                            fgdb_gql::insertion::GraphInsertRequest::Vertex { .. } => {
                                vertex += 1;
                                ElementId::Vertex(VId(vertex))
                            }
                            fgdb_gql::insertion::GraphInsertRequest::Edge { .. } => {
                                edge += 1;
                                ElementId::Edge(EId(edge))
                            }
                        })
                    },
                )
                .unwrap();
            } else {
                let prepared = PreparedGraphWriteScript::prepare(
                    &multi_statement(7, "INSERT"),
                    KNOWS,
                    symbols,
                )
                .unwrap();
                txn.execute_graph_write_script_governed(
                    &mut db,
                    &contexts.query(),
                    &prepared,
                    &GqlParameters::new(),
                    policy(),
                    &mut ids,
                )
                .unwrap();
            }
            assert_eq!(
                db.frontier().unwrap(),
                basis,
                "{name}: staging is not durable"
            );
            assert_eq!(graph(&db), before, "{name}: no published staged prefix");
            let result = txn.commit_with_crash(&mut db, &commit, crash).await;
            assert_eq!(result.is_ok(), crash.is_none(), "{name}: injection reached");
            if matches!(
                crash,
                Some(
                    CrashPoint::AfterMarkerBeforeD2
                        | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                )
            ) {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(
                        WriteError::CommitOutcomeUnknown { .. }
                    ))
                ));
                assert!(db.frontier().is_err());
            }
            drop(db);
            if tear {
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(&path, 1).unwrap();
            }
            for rebuilding in [false, true] {
                let reopened = if rebuilding {
                    Database::open_rebuilding(&commit, &path, keys(0x63))
                        .await
                        .unwrap()
                } else {
                    Database::open(&commit, &path, keys(0x63)).await.unwrap()
                };
                assert_eq!(
                    reopened.frontier().unwrap(),
                    CommitSeq(basis.0 + u64::from(committed)),
                    "{name}, rebuilding={rebuilding}"
                );
                assert_eq!(
                    reopened.delta_since(basis).unwrap().count(),
                    usize::from(committed),
                    "{name}"
                );
                if direct_insert {
                    for (id, relation, src, dst) in [
                        (1_001, KNOWS, 1_001, 1_002),
                        (1_002, LIVES_IN, 1_001, 1_003),
                        (1_003, WORKS_AT, 1_002, 1_004),
                    ] {
                        let edge = reopened.edge(EId(id)).unwrap();
                        if committed {
                            let edge = edge.unwrap();
                            assert_eq!(
                                (edge.entry.relation, edge.entry.src, edge.entry.dst),
                                (relation, VId(src), VId(dst)),
                                "{name}: caller edge {id}",
                            );
                        } else {
                            assert!(edge.is_none(), "{name}: published caller edge {id}");
                        }
                    }
                    for id in 1_001..=1_004 {
                        assert_eq!(reopened.vertex(VId(id)).unwrap().is_some(), committed);
                    }
                }
                assert_eq!(
                    &graph(&reopened),
                    if committed { &after } else { &before },
                    "{name}, rebuilding={rebuilding}"
                );
                assert_eq!(
                    &answers(&reopened, &contexts),
                    if committed {
                        &after_answers
                    } else {
                        &before_answers
                    },
                    "{name}, rebuilding={rebuilding}"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
