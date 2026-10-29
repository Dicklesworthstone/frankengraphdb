//! Native MERGE expressions, atomic UNWIND and capability-scoped scalar inputs.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindWriteExecutionError, GraphUnwindWriteText};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertError, GraphVertexUpsertPolicy,
    GraphWriteProgramPolicy, PreparedGraphVertexUpsertText, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar as Scalar, CanonicalScalarKind, DatabaseSecurityNamespaceId,
    EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const COUNT: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const READY: PropertyKeyId = PropertyKeyId(4);
const COPY: PropertyKeyId = PropertyKeyId(5);
const SECRET: PropertyKeyId = PropertyKeyId(6);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc2; 32]);
const BATCH: &str = "UNWIND $rows AS row MERGE (n:Person {id:row.id}) \
    ON CREATE SET n.count=0 SET n.count=n.count+row.delta";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], NS, [0xc3; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, name) => match name {
            "id" => Some(ID),
            "count" => Some(COUNT),
            "name" => Some(NAME),
            "ready" => Some(READY),
            "copy" => Some(COPY),
            "secret" => Some(SECRET),
            _ => None,
        }
        .map(GraphSymbol::Property),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000)
}
fn program_policy(effects: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), effects, 10, 0)
}
fn scalar(db: &Database<MemVfs>, vertex: VId, key: PropertyKeyId) -> Scalar {
    db.vertex(vertex)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(actual, _)| *actual == key)
        .unwrap()
        .1
        .as_scalar()
        .expect("scalar property")
        .clone()
}
fn rows(values: &[(i64, i64)]) -> GqlParameters {
    let values = values
        .iter()
        .map(|&(id, delta)| {
            GraphValue::map(vec![
                ("id".into(), GraphValue::Scalar(Scalar::Int(id))),
                ("delta".into(), GraphValue::Scalar(Scalar::Int(delta))),
            ])
            .unwrap()
        })
        .collect();
    GqlParameters::new().with_list("rows", values).unwrap()
}

#[test]
fn prepared_text_reuses_parameters_and_compiles_scalar_case_and_property_reads() {
    let ((), report) = run_async_under_lab(0x5c73_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = "MERGE (n:Person {id:$id}) ON CREATE SET n.count=0 \
            SET n.count=coalesce(n.count,0)+$delta, n.name=UPPER($name), \
            n.ready=CASE WHEN $delta>0 THEN TRUE ELSE FALSE END, n.copy=n.count";
        let template = PreparedGraphVertexUpsertText::prepare_with_parameter_types(
            text,
            R,
            &[
                ("id", GqlParameterType::Int64),
                ("delta", GqlParameterType::Int64),
                ("name", GqlParameterType::Scalar(CanonicalScalarKind::Text)),
            ],
            symbols,
        )
        .unwrap();
        assert_eq!(template.parameter_schema().len(), 3);
        for (delta, name, expected, copied) in [(3, "Ada", 3, 0), (4, "Eve", 7, 3)] {
            let arguments = GqlParameters::new()
                .with_int64("id", 1)
                .unwrap()
                .with_int64("delta", delta)
                .unwrap()
                .with_text("name", name)
                .unwrap();
            let frozen = arguments.canonical_bytes();
            let upsert = template.bind_parameters(&arguments).unwrap();
            let (_, outcome, _) = db
                .execute_graph_vertex_upsert_autocommit_governed(
                    &txn,
                    &query,
                    &commit,
                    &upsert,
                    GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
                    |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
                )
                .await
                .unwrap();
            assert_eq!(outcome.created(), copied == 0);
            assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(expected));
            assert_eq!(scalar(&db, VId(1), COPY), Scalar::Int(copied));
            assert_eq!(
                scalar(&db, VId(1), NAME),
                Scalar::ucs_basic_text(&name.to_uppercase()).unwrap()
            );
            assert_eq!(scalar(&db, VId(1), READY), Scalar::Bool(true));
            assert_eq!(arguments.canonical_bytes(), frozen);
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_expression_scopes_and_declared_types_refuse_before_catalog_callbacks() {
    for text in [
        "MERGE (n:Person {id:1}) SET n.count=other.count+1",
        "MERGE (n:Person {id:1}) SET n.count=n",
        "MERGE (n:Person {id:1}) SET n.count=n.count.part",
        "MERGE (n:Person {id:1}) SET n.count=coalesce(n.count,)",
        // A constant list writes a stored List (fgdb-k3nub); one built from
        // the MERGE vertex's own fields does not yet.
        "MERGE (n:Person {id:1}) SET n.count=[n.count]",
        "MERGE (n:Person {id:1}) SET n.count=$s*2",
    ] {
        let mut calls = 0;
        let result = PreparedGraphVertexUpsertText::prepare_with_parameter_types(
            text,
            R,
            &[("s", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
            |kind, name: &str| {
                calls += 1;
                symbols(kind, name)
            },
        );
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    let text = "MERGE (n:Person {id:$id}) SET n.count=n.count+$delta";
    let template = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap();
    assert!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("id", 1).unwrap())
            .is_err()
    );
    assert!(
        template
            .bind_parameters(
                &GqlParameters::new()
                    .with_int64("id", 1)
                    .unwrap()
                    .with_text("delta", "not an integer")
                    .unwrap()
            )
            .is_err()
    );
}

#[test]
fn unwind_repeated_keys_accumulate_in_one_commit_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0x5c73_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let definition = GraphUnwindWriteText::parse(BATCH).unwrap();
        let arguments = rows(&[(1, 3), (2, 5), (1, 7)]);
        let frozen = arguments.canonical_bytes();
        let mut allocations = 0;
        let (receipt, completion) = db
            .execute_graph_unwind_write_autocommit_governed(
                &txn,
                &query,
                &commit,
                &definition,
                &arguments,
                R,
                3,
                symbols,
                program_policy(5),
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(allocations)))
                },
            )
            .await
            .unwrap();
        assert_eq!(allocations, 2);
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().mutation_effects, 5);
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(10));
        assert_eq!(scalar(&db, VId(2), COUNT), Scalar::Int(5));
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_unwind_overflow_or_shared_action_limit_never_publishes_a_prefix() {
    let ((), report) = run_async_under_lab(0x5c73_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        for (input, effects) in [
            (rows(&[(1, i64::MAX), (1, 1)]), 3),
            (rows(&[(1, 1), (1, 1)]), 2),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let definition = GraphUnwindWriteText::parse(BATCH).unwrap();
            let mut allocations = 0;
            let result = db
                .execute_graph_unwind_write_autocommit_governed(
                    &txn,
                    &query,
                    &commit,
                    &definition,
                    &input,
                    R,
                    2,
                    symbols,
                    program_policy(effects),
                    |_| {
                        allocations += 1;
                        Ok::<_, ()>(ElementId::Vertex(VId(1)))
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(GraphUnwindWriteExecutionError::Execution(_))
            ));
            assert_eq!(allocations, 1, "first row ran, second did not allocate");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn overwritten_failing_on_assignment_is_not_erased_but_dead_case_arm_is_lazy() {
    let ((), report) = run_async_under_lab(0x5c73_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let bad = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) ON CREATE SET n.count=1/0 SET n.count=7",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let result = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txn,
                &query,
                &commit,
                &bad,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(1))),
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexUpsertError::Expression {
                clause: 0,
                action: 0,
                ..
            }))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        let good = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {id:1}) SET n.count=CASE WHEN TRUE THEN 7 ELSE 1/0 END",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        db.execute_graph_vertex_upsert_autocommit_governed(
            &txn,
            &query,
            &commit,
            &good,
            GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(query_policy()), 8),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(2))),
        )
        .await
        .unwrap();
        assert_eq!(scalar(&db, VId(2), COUNT), Scalar::Int(7));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9982), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([PERSON]),
        relations: Scope::only([R]),
        properties: Scope::only([ID, COUNT]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 2_000_000,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}

#[test]
fn authorized_computed_reads_mask_hidden_payloads_before_value_or_size_admission() {
    let ((), report) = run_async_under_lab(0x5c73_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let program = PreparedGraphWriteScript::prepare(
            "MERGE (n:Person {id:1}) SET n.count=coalesce(n.secret,0)+1",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut first_stats = None;
        for secret in [
            Scalar::Int(99),
            Scalar::ucs_basic_text(&"x".repeat(8192)).unwrap(),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![PERSON],
                vec![(ID, Scalar::Int(1)), (SECRET, secret.clone())],
            );
            db.write(&commit, seed).await.unwrap();
            let (stats, _) = db
                .execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    program_policy(1),
                    || 100,
                )
                .await
                .unwrap();
            assert_eq!(scalar(&db, VId(1), COUNT), Scalar::Int(1));
            assert_eq!(scalar(&db, VId(1), SECRET), secret);
            if let Some(expected) = &first_stats {
                assert_eq!(&stats, expected);
            } else {
                first_stats = Some(stats);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_on_clause_write_cannot_be_hidden_by_a_trailing_overwrite() {
    let ((), report) = run_async_under_lab(0x5c73_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![PERSON],
            vec![(ID, Scalar::Int(1)), (SECRET, Scalar::Int(7))],
        );
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let original = db.vertex(VId(1)).unwrap();
        let program = PreparedGraphWriteScript::prepare(
            "MERGE (n:Person {id:1}) ON MATCH SET n.secret=1 SET n.secret=7",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(
            db.execute_graph_write_program_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &program,
                program_policy(2),
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertex(VId(1)).unwrap(), original);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-2npty: a MATCH or MERGE key computed from row text, as ingestion
/// writes it (`{id: toInteger(row.s)}`, `{id: row.a + 1}`), is a bind-time
/// constant probed like a precomputed field, as is a flat WHERE equality
/// on one. 400 vertices sit behind a 50-record allowance, which one scan
/// exceeds; a column-fed key and an equality under OR are the controls
/// that refuse there.
#[test]
fn computed_keys_probe_the_property_index_below_one_scan() {
    let ((), report) = run_async_under_lab(0x5c73_2a7f, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=400_i64 {
            batch.create_vertex(
                VId(id as u128),
                vec![PERSON],
                vec![(ID, Scalar::Int(id)), (COUNT, Scalar::Int(0))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let field = |name: &str, value: Scalar| {
            GraphValue::map(vec![(name.into(), GraphValue::Scalar(value))]).unwrap()
        };
        let text = |value: &str| Scalar::ucs_basic_text(value).unwrap();
        let batch =
            |values: Vec<GraphValue>| GqlParameters::new().with_list("rows", values).unwrap();
        let tight = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(50, 20_000, 2_000_000, 2_000_000),
            100,
            10,
            0,
        );
        let mut allocations = 0_u128;
        let total = |db: &Database<MemVfs>| -> i64 {
            (1..=400)
                .map(|id| match scalar(db, VId(id), COUNT) {
                    Scalar::Int(count) => count,
                    other => panic!("integer count, got {other:?}"),
                })
                .sum()
        };
        for (statement, rows, policy) in [
            (
                "UNWIND $rows AS row MATCH (n:Person {id: toInteger(row.s)}) SET n.count = 1",
                vec![field("s", text("5")), field("s", text("17"))],
                tight,
            ),
            (
                "UNWIND $rows AS row MATCH (n:Person {id: row.a + 1}) SET n.count = 2",
                vec![field("a", Scalar::Int(29))],
                tight,
            ),
            (
                "UNWIND $rows AS row MERGE (n:Person {id: toInteger(row.s)}) SET n.count = 3",
                vec![field("s", text("7")), field("s", text("900"))],
                tight,
            ),
        ] {
            db.query_write(
                &txn,
                &query,
                &commit,
                statement,
                &batch(rows),
                symbols,
                R,
                policy,
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(1_000 + allocations)))
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        }
        assert_eq!(scalar(&db, VId(5), COUNT), Scalar::Int(1));
        assert_eq!(scalar(&db, VId(17), COUNT), Scalar::Int(1));
        assert_eq!(scalar(&db, VId(30), COUNT), Scalar::Int(2));
        assert_eq!(scalar(&db, VId(7), COUNT), Scalar::Int(3));
        // MERGE created exactly the one missing key, with its computed id.
        assert_eq!(allocations, 1);
        assert_eq!(scalar(&db, VId(1_001), ID), Scalar::Int(900));
        assert_eq!(scalar(&db, VId(1_001), COUNT), Scalar::Int(3));
        assert_eq!(total(&db), 1 + 1 + 2 + 3);

        // toInteger('x') is NULL: MATCH binds nothing. (The write overlay
        // still scans for a NULL key, as for a NULL parameter, so this runs
        // under the ordinary allowance; the read below needs no scan.)
        let unparsable = batch(vec![field("s", text("x"))]);
        db.query_write(
            &txn,
            &query,
            &commit,
            "UNWIND $rows AS row MATCH (n:Person {id: toInteger(row.s)}) SET n.count = 4",
            &unparsable,
            symbols,
            R,
            program_policy(100),
            |_| Err::<ElementId, _>(()),
        )
        .await
        .unwrap();
        assert_eq!(total(&db), 7);
        // MERGE refuses a NULL key and creates nothing.
        assert!(
            db.query_write(
                &txn,
                &query,
                &commit,
                "UNWIND $rows AS row MERGE (n:Person {id: toInteger(row.s)}) SET n.count = 4",
                &unparsable,
                symbols,
                R,
                program_policy(100),
                |_| Err::<ElementId, _>(()),
            )
            .await
            .is_err()
        );
        assert_eq!(db.vertices().unwrap().len(), 401);

        // A flat WHERE equality probes like the map, mirrored or beside
        // another conjunct.
        for (statement, rows) in [
            (
                "UNWIND $rows AS row MATCH (n:Person) WHERE n.id = toInteger(row.s) SET n.count = 9",
                vec![field("s", text("5"))],
            ),
            (
                "UNWIND $rows AS row MATCH (n:Person) WHERE row.a + 1 = n.id AND n.count = 0 \
                 SET n.count = 9",
                vec![field("a", Scalar::Int(40))],
            ),
        ] {
            db.query_write(
                &txn,
                &query,
                &commit,
                statement,
                &batch(rows),
                symbols,
                R,
                tight,
                |_| Err::<ElementId, _>(()),
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        }
        assert_eq!(scalar(&db, VId(5), COUNT), Scalar::Int(9));
        assert_eq!(scalar(&db, VId(41), COUNT), Scalar::Int(9));

        // A WITH stage computing the key from the row joins on it: the
        // batch writer leaves the statement to the multipart compiler, which
        // used to be unreachable here ("MERGE or MATCH after the UNWIND
        // sources"). Its write-transaction source is not seeded yet, so this
        // runs under the ordinary allowance (fgdb-2npty).
        for (statement, s, vertex, count) in [
            (
                "UNWIND $rows AS row WITH row, toInteger(row.s) AS k \
                 MATCH (n:Person {id: k}) SET n.count = 11",
                "12",
                12,
                11,
            ),
            (
                "UNWIND $rows AS row WITH toInteger(row.s) + 1 AS k \
                 MATCH (n:Person) WHERE n.id = k SET n.count = 12",
                "12",
                13,
                12,
            ),
        ] {
            db.query_write(
                &txn,
                &query,
                &commit,
                statement,
                &batch(vec![field("s", text(s))]),
                symbols,
                R,
                program_policy(100),
                |_| Err::<ElementId, _>(()),
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
            assert_eq!(
                scalar(&db, VId(vertex), COUNT),
                Scalar::Int(count),
                "{statement}"
            );
        }

        // The allowance is below one scan: a key the column itself feeds, or
        // an equality under OR, stays a residual filter and refuses there,
        // and answers the same under the ordinary allowance.
        let six = batch(vec![field("s", text("6"))]);
        for control in [
            "UNWIND $rows AS row MATCH (n:Person) WHERE n.id + 0 = toInteger(row.s) SET n.count = 8",
            "UNWIND $rows AS row MATCH (n:Person) WHERE n.id = toInteger(row.s) OR n.id = 0 \
             SET n.count = 8",
        ] {
            let before = scalar(&db, VId(6), COUNT);
            assert!(
                db.query_write(
                    &txn,
                    &query,
                    &commit,
                    control,
                    &six,
                    symbols,
                    R,
                    tight,
                    |_| Err::<ElementId, _>(()),
                )
                .await
                .is_err(),
                "{control}"
            );
            assert_eq!(scalar(&db, VId(6), COUNT), before, "{control}");
            db.query_write(
                &txn,
                &query,
                &commit,
                control,
                &six,
                symbols,
                R,
                program_policy(100),
                |_| Err::<ElementId, _>(()),
            )
            .await
            .unwrap_or_else(|error| panic!("{control}: {error:?}"));
            assert_eq!(scalar(&db, VId(6), COUNT), Scalar::Int(8), "{control}");
        }

        // A read keyed by a computed parameter expression probes too, in
        // the map or in WHERE.
        for statement in [
            "MATCH (n:Person {id: toInteger($s) + 13}) RETURN n.count AS c",
            "MATCH (n:Person) WHERE n.id = toInteger($s) + 13 RETURN n.count AS c",
        ] {
            let fgdb::QueryResult::Rows { rows, .. } = db
                .query(
                    &query,
                    statement,
                    &GqlParameters::new().with_text("s", "4").unwrap(),
                    symbols,
                    GqlQueryPolicy::new(5, 100, 100_000, 100_000),
                )
                .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
            else {
                panic!("a read returns rows")
            };
            assert_eq!(
                rows,
                [vec![fgdb::QueryValue::Value(GraphValue::Scalar(
                    Scalar::Int(1)
                ))]],
                "{statement}"
            );
        }
        // `= NULL` is never TRUE: the read answers nothing without a scan,
        // within a one-record allowance.
        let fgdb::QueryResult::Rows { rows, .. } = db
            .query(
                &query,
                "MATCH (n:Person {id: toInteger($s)}) RETURN n.count AS c",
                &GqlParameters::new().with_text("s", "x").unwrap(),
                symbols,
                GqlQueryPolicy::new(1, 100, 100_000, 100_000),
            )
            .unwrap()
        else {
            panic!("a read returns rows")
        };
        assert!(rows.is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-at06q: `ON CREATE SET n = {map}` replaces the created vertex's map.
/// The vertex holds exactly the MERGE pattern's keys, so the replacement
/// assigns each entry and unsets each pattern key the map omits. ON MATCH
/// replacement, which needs a run-time read of the matched map, still
/// refuses, as does a replacement after another assignment in its clause.
#[test]
fn on_create_map_replacement_sets_the_entries_and_unsets_omitted_pattern_keys() {
    let ((), report) = run_async_under_lab(0x5c73_a706, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut allocations = 0_u128;
        let props = |db: &Database<MemVfs>, vertex: u128| {
            let mut props: Vec<_> = db
                .vertex(VId(vertex))
                .unwrap()
                .unwrap()
                .props
                .into_iter()
                .map(|(key, value)| (key, value.as_scalar().expect("scalar property").clone()))
                .collect();
            props.sort_by_key(|(key, _)| *key);
            props
        };
        let name = |value: &str| Scalar::ucs_basic_text(value).unwrap();
        let rows = GqlParameters::new()
            .with_list(
                "rows",
                vec![
                    GraphValue::map(vec![
                        ("id".into(), GraphValue::Scalar(Scalar::Int(12))),
                        ("name".into(), GraphValue::Scalar(name("Dee"))),
                    ])
                    .unwrap(),
                ],
            )
            .unwrap();
        let props_param = GqlParameters::new()
            .with_map(
                "props",
                vec![
                    ("id".into(), GraphValue::Scalar(Scalar::Int(11))),
                    ("name".into(), GraphValue::Scalar(name("Cy"))),
                ],
            )
            .unwrap();
        for (statement, params) in [
            (
                "MERGE (n:Person {id: 9}) ON CREATE SET n = {id: 9, name: 'Ada'}",
                GqlParameters::new(),
            ),
            (
                "MERGE (n:Person {id: 10}) ON CREATE SET n = {name: 'Bob'}",
                GqlParameters::new(),
            ),
            (
                "MERGE (n:Person {id: 11}) ON CREATE SET n = $props",
                props_param,
            ),
            (
                "UNWIND $rows AS row MERGE (n:Person {id: row.id}) \
                 ON CREATE SET n = {id: row.id, name: row.name}",
                rows,
            ),
            // The match branch: vertex 9 exists, so ON CREATE does not run.
            (
                "MERGE (n:Person {id: 9}) ON CREATE SET n = {name: 'Eve'}",
                GqlParameters::new(),
            ),
        ] {
            db.query_write(
                &txn,
                &query,
                &commit,
                statement,
                &params,
                symbols,
                R,
                program_policy(10),
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(allocations)))
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        }
        assert_eq!(allocations, 4);
        assert_eq!(props(&db, 1), [(ID, Scalar::Int(9)), (NAME, name("Ada"))]);
        // The omitted pattern key is unset, as `SET n = {...}` unsets it.
        assert_eq!(props(&db, 2), [(NAME, name("Bob"))]);
        assert_eq!(props(&db, 3), [(ID, Scalar::Int(11)), (NAME, name("Cy"))]);
        assert_eq!(props(&db, 4), [(ID, Scalar::Int(12)), (NAME, name("Dee"))]);
        for refused in [
            "MERGE (n:Person {id: 9}) ON MATCH SET n = {name: 'Eve'}",
            "MERGE (n:Person {id: 20}) ON CREATE SET n.name = 'Fay', n = {id: 20}",
            "MERGE (n:Person {id: 20}) SET n = {id: 20}",
        ] {
            assert!(
                db.query_write(
                    &txn,
                    &query,
                    &commit,
                    refused,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    program_policy(10),
                    |_| Err::<ElementId, _>(()),
                )
                .await
                .is_err(),
                "{refused}"
            );
        }
        assert_eq!(db.vertices().unwrap().len(), 4);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
