//! Real capabilities, native chain collectors, transaction staging and reopen.
//! Private WITH rows cannot bypass scope, quotas or the one commit boundary.

#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQueryText, PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x60; 32], NS, [0x62; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9167), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights, max_rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows,
        },
        expires_at_ms: 10_000,
    }
}

fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000),
        100,
        100,
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

#[test]
fn multipart_read_before_create_masks_hidden_matches_and_freezes_optional_rows() {
    lab(0xc4a1_2031, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, label, value) in [(1, L, 10), (2, L, 20), (3, L, 30), (4, HIDDEN, 40)] {
            seed.create_vertex(VId(id), vec![label], vec![(P, CanonicalScalar::Int(value))]);
        }
        for (id, source, destination) in
            [(11, 1, 2), (12, 1, 2), (13, 1, 4), (14, 4, 2), (15, 2, 3)]
        {
            seed.add_edge(EId(id), VId(source), VId(destination), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let hidden = db.vertex(VId(4)).unwrap().unwrap();
        let original_edges = db.edges().unwrap();
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::ReadWrite, 100), NOW)
            .unwrap();
        let definition = PreparedGraphInsertQueryText::prepare(
            "MATCH (a:Visible) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
             WITH coalesce(b.p,-1) AS value CREATE (copy:Visible {p:value}) \
             RETURN copy.p AS p ORDER BY p",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (stats, rows, completion) = db
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &definition,
                policy(100),
                || NOW,
            )
            .await
            .unwrap();
        // The hidden endpoint and its incoming/outgoing matches contribute
        // no row. The two visible parallel relationships remain two rows,
        // and a=3 contributes exactly one NULL-extended optional row.
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 0));
        assert_eq!(rows.value.len(), 4);
        for (row, value) in rows.value.iter().zip([-1, 20, 20, 30]) {
            assert_eq!(
                row.values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(value))]
            );
        }
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == basis.0 + 1
        ));
        assert_eq!(db.vertex(VId(4)).unwrap().unwrap(), hidden);
        assert_eq!(db.edges().unwrap(), original_edges);
        assert_eq!(db.vertices().unwrap().len(), 8);

        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let write_only = issuer.issue_at(&grant(Rights::Write, 100), NOW).unwrap();
        assert!(
            db.execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &write_only,
                "main",
                &definition,
                policy(100),
                || NOW,
            )
            .await
            .is_err()
        );
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn multipart_reads_between_writes_mask_hidden_witnesses_and_commit_atomically() {
    lab(0xc4a1_2032, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(20))]);
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(40))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let original_vertices = db.vertices().unwrap();
        let original_edges = db.edges().unwrap();
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::ReadWrite, 100), NOW)
            .unwrap();
        let program = chain(
            "CREATE (start:Visible {p:1}) WITH start \
             MATCH (candidate) WHERE candidate.p >= 10 WITH start,candidate \
             OPTIONAL MATCH (candidate)-[:R]->(peer) \
             WITH start,candidate.p + coalesce(peer.p,-1) AS value \
             CREATE (copy:Visible {p:value}),(start)-[:S]->(copy) \
             RETURN copy.p AS p",
        );
        let mut bounded = policy(100);
        bounded.max_vertices = 1;
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            &program,
            bounded,
            || NOW,
        )
        .await
        .expect_err("a later creation quota failure must discard the earlier CREATE");
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), original_vertices);
        assert_eq!(db.edges().unwrap(), original_edges);

        let write_only = issuer.issue_at(&grant(Rights::Write, 100), NOW).unwrap();
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &write_only,
            "main",
            &program,
            policy(100),
            || NOW,
        )
        .await
        .expect_err("the intervening graph read requires read authority");
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), original_vertices);
        assert_eq!(db.edges().unwrap(), original_edges);

        let (stats, rows, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(100),
                || NOW,
            )
            .await
            .unwrap();
        // The hidden vertex is absent both as a MATCH candidate and as an
        // OPTIONAL witness. The new copy would itself pass the MATCH filter,
        // but the complete input bag is frozen before it is allocated.
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 1));
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(19))]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == basis.0 + 1
        ));
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 4);
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap(), original_vertices[0]);
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), original_vertices[1]);
        assert_eq!(db.edge(EId(11)).unwrap().unwrap(), original_edges[0]);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_replacement_metadata_interruptions_keep_their_channel_and_discard_create() {
    use fgdb_gql::{
        GqlQueryError, GraphSymbolResolver, GraphWriteChainError, GraphWriteChainStep,
        ReverseSymbolCatalog,
    };
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    struct Catalog;
    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_label(L, "Visible");
            catalog.insert_property(P, "p");
            catalog.insert_property(SECRET, "secret");
            Some(catalog)
        }
    }

    fn interrupted_source(error: &(dyn core::error::Error + 'static)) -> bool {
        let mut cause = Some(error);
        while let Some(error) = cause {
            if matches!(
                error.downcast_ref::<WriteTxnError>(),
                Some(WriteTxnError::Interrupted(_))
            ) {
                return true;
            }
            cause = error.source();
        }
        false
    }

    lab(0xc4a1_20e1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();

        // A carried nonempty map also updates a just-created owned target,
        // and RETURN observes the replacement in the same native commit.
        let nonempty = PreparedGraphWriteChainText::prepare(
            "CREATE (n:Visible {p:7}) WITH n,{p:n.p+1} AS patch \
             SET n=patch RETURN n.p AS value",
            R,
            Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut success = Database::open_memory(&commit, keys()).await.unwrap();
        let before = success.frontier().unwrap();
        let (stats, rows, completion) = success
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &nonempty,
                policy(10),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(8))]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == before.0 + 1
        ));

        for returning in [false, true] {
            let text = format!(
                "CREATE (n:Visible) WITH n,{{}} AS patch SET n=patch{}",
                if returning { " RETURN 1 AS done" } else { "" },
            );
            let program = PreparedGraphWriteChainText::prepare(&text, R, Catalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let mutation = match program.steps().last().unwrap() {
                GraphWriteChainStep::Mutation(mutation) if !returning => mutation.as_ref(),
                GraphWriteChainStep::MutationQuery(query) if returning => query.mutation(),
                _ => panic!("the carried replacement must use its native mutation collector"),
            };
            assert!(mutation.actions().iter().any(|action| matches!(
                action,
                fgdb_gql::GraphMutationAction::SetProperties {
                    current: Some(_),
                    ..
                }
            )));

            // Measure the preflight prefix without hard-coding a checkpoint
            // ordinal. Zero evaluator work refuses before CREATE can run,
            // so no replacement metadata has been read at this boundary.
            let mut refused = Database::open_memory(&commit, keys()).await.unwrap();
            let prefix = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = query.with_checkpoint_probe(Arc::clone(&prefix));
            let mut no_work = policy(10);
            no_work.query.evaluator.max_work_units = 0;
            let error = refused
                .execute_graph_write_chain_authorized(
                    &txn,
                    &controlled,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    no_work,
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, GqlQueryError::Evaluator(_)), "{error:?}");
            assert!(refused.vertices().unwrap().is_empty());
            let preflight_calls = prefix.calls();

            // This replacement must read the newly created target's frozen
            // properties, but an empty target/map produces no field intents.
            // Thus this law isolates mutation-reader cancellation from native
            // property-staging failures while exercising both chain collectors.
            let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
            let observed = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = query.with_checkpoint_probe(Arc::clone(&observed));
            let (_, rows, _) = baseline
                .execute_graph_write_chain_authorized(
                    &txn,
                    &controlled,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(baseline.vertices().unwrap().len(), 1);
            assert_eq!(rows.value.len(), usize::from(returning));
            let calls = observed.calls();
            assert!(calls > preflight_calls);

            for stop in 1..=calls {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                let error = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &controlled,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(10),
                        || NOW,
                    )
                    .await
                    .expect_err("every observed checkpoint must remain interruptible");
                match &error {
                    GqlQueryError::Interrupted(WriteTxnError::Interrupted(_)) => {}
                    // Existing CREATE/commit wrappers retain their source
                    // channel. The empty replacement has no property staging:
                    // its reader must never return a Mutation source error.
                    GqlQueryError::Source(GraphWriteChainError::Insert(_)) => {
                        assert!(interrupted_source(&error), "{text}, stop={stop}: {error:?}");
                    }
                    // Whole-program static admission also predates this
                    // reader. Its measured prefix is the only allowed
                    // Mutation source wrapper for this zero-intent update.
                    GqlQueryError::Source(GraphWriteChainError::Mutation(_))
                        if stop <= preflight_calls =>
                    {
                        assert!(interrupted_source(&error), "{text}, stop={stop}: {error:?}");
                    }
                    _ => panic!("{text}, stop={stop}: {error:?}"),
                }
                assert_eq!(probe.calls(), stop, "{text}, stop={stop}");
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn multipart_mutations_between_writes_use_staged_values_and_rollback_the_whole_chain() {
    lab(0xc4a1_2033, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(20))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let original_vertices = db.vertices().unwrap();
        let original_edges = db.edges().unwrap();
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::ReadWrite, 100), NOW)
            .unwrap();
        let body = "CREATE (start:Visible {p:1}) WITH start \
            MATCH (a:Visible) WHERE a.p >= 10 WITH start,a \
            OPTIONAL MATCH (a)-[:R]->(b) WITH start,a,b,a.p AS value \
            SET a.p=value+1,b.p=1/0 WITH start,a \
            CALL (a) { RETURN a.p AS updated } SET start.p=updated";
        let program = chain(&format!("{body} RETURN a.p,start.p"));
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            &program,
            policy(100).with_mutation_limit(1),
            || NOW,
        )
        .await
        .expect_err("the second SET's effect limit discards the CREATE and first SET");
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), original_vertices);
        assert_eq!(db.edges().unwrap(), original_edges);
        let late_failure = chain(&format!("{body} RETURN 1/(11-start.p) AS bad LIMIT 0"));
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            &late_failure,
            policy(100),
            || NOW,
        )
        .await
        .expect_err("LIMIT 0 cannot hide a late RETURN failure or publish earlier writes");
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), original_vertices);
        assert_eq!(db.edges().unwrap(), original_edges);
        let (stats, rows, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(100).with_mutation_limit(2),
                || NOW,
            )
            .await
            .unwrap();
        // The hidden endpoint never becomes a SET target. CALL then reads
        // the first SET's staged value and supplies it to the second SET.
        assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(11)),
                GraphValue::Scalar(CanonicalScalar::Int(11)),
            ]
        );
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(11).into())]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), original_vertices[1]);
        assert_eq!(db.edges().unwrap(), original_edges);
        assert!(matches!(completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == basis.0 + 1
        ));
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    let template = PreparedGraphWriteChainText::prepare(text, R, symbols).expect(text);
    let result = template.bind_parameters(&GqlParameters::new()).expect(text);
    assert_eq!(template.returns_rows(), result.returns_rows());
    result
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}

fn csv_chain(text: &str) -> PreparedGraphWriteChain {
    use fgdb_gql::csv_parameters::CsvParameterLimits;
    use fgdb_gql::{GqlParameterSpec, GqlParameterType};
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        R,
        &[("rows", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    let schema: Vec<_> = ["p", "q"]
        .into_iter()
        .map(|name| GqlParameterSpec {
            name: name.into(),
            parameter_type: GqlParameterType::Int64,
            requires_positive: false,
            occurrences: 1,
        })
        .collect();
    let mut input = b"p,q\n2,1\n1,2\n".as_slice();
    let (chain, records) = template
        .bind_csv_rows_stream_controlled(
            "rows",
            &schema,
            CsvParameterLimits::default(),
            |buffer| -> Result<usize, ()> {
                let count = buffer.len().min(3).min(input.len());
                buffer[..count].copy_from_slice(&input[..count]);
                input = &input[count..];
                Ok(count)
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(records, 2);
    chain
}

#[test]
fn csv_initial_relations_match_list_oracle_through_native_and_authorized_hosts() {
    lab(0xc4a1_20c1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let arguments = GqlParameters::new()
            .with_list(
                "rows",
                [(2, 1), (1, 2)]
                    .into_iter()
                    .map(|(p, q)| {
                        GraphValue::map(vec![
                            ("p".into(), GraphValue::Scalar(CanonicalScalar::Int(p))),
                            ("q".into(), GraphValue::Scalar(CanonicalScalar::Int(q))),
                        ])
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        for text in [
            "UNWIND $rows AS row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row WITH row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row UNWIND [2,1] AS repeat MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row MATCH (seed:Visible {p:row.p}) MERGE (a:Visible {p:row.q}) MERGE (seed)-[:R]->(a)",
            "UNWIND $rows AS row CREATE (a:Visible {p:row.p}) WITH row,a CREATE (b:Visible {p:row.q}), (a)-[:R]->(b)",
            "UNWIND $rows AS row MATCH (a:Visible {p:row.p}), (b:Visible {p:row.q}) MERGE (a)-[:R]->(b) MERGE (a)-[:S]->(b)",
            "UNWIND $rows AS row FOREACH (x IN [row.p,row.q] | CREATE (:Visible {p:x}))",
            "UNWIND $rows AS row WITH row FOREACH (x IN [row.p,row.q] | CREATE (:Visible {p:x}))",
            "UNWIND $rows AS row WITH row FOREACH (values IN [[],[row.p,row.q]] | FOREACH (value IN values | CREATE (:Visible {p:value})))",
        ] {
            assert!(
                PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
            let ordinary = PreparedGraphWriteChainText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&arguments)
                .unwrap();
            let streamed = csv_chain(text);
            let mut expected = None;
            for mode in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(10), vec![L], vec![(P, CanonicalScalar::Int(1))]);
                seed.create_vertex(VId(20), vec![L], vec![(P, CanonicalScalar::Int(2))]);
                db.write(&commit, seed).await.unwrap();
                let basis = db.frontier().unwrap();
                if mode == 2 {
                    db.execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &streamed,
                        policy(10),
                        || NOW,
                    )
                    .await
                    .unwrap();
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    transaction
                        .execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            if mode == 0 { &ordinary } else { &streamed },
                            policy(10),
                        )
                        .unwrap();
                    transaction.finish(&mut db, &commit).await.unwrap();
                }
                assert_eq!(db.frontier().unwrap().0, basis.0 + 1, "mode {mode}: {text}");
                let actual = (db.vertices().unwrap(), db.edges().unwrap());
                if let Some(expected) = &expected {
                    assert_eq!(&actual, expected, "mode {mode}: {text}");
                } else {
                    expected = Some(actual);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn csv_input_admission_and_late_clause_failure_publish_no_prefix() {
    lab(0xc4a1_20c2, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let program = csv_chain(
            "UNWIND $rows AS row MERGE (a:Visible {p:row.p}) MERGE (b:Visible {p:row.q+10}) MERGE (a)-[:R]->(b)",
        );
        for authorized in [false, true] {
            for input_limit in [true, false] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                let mut allowance = policy(10);
                if input_limit {
                    allowance.query.evaluator.max_scratch_entries = 0;
                } else {
                    allowance.max_vertices = 3;
                }
                if authorized {
                    assert!(
                        db.execute_graph_write_chain_authorized(
                            &txn,
                            &query,
                            &commit,
                            &issuer,
                            &token,
                            "main",
                            &program,
                            allowance,
                            || NOW
                        )
                        .await
                        .is_err()
                    );
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    assert!(
                        transaction
                            .execute_graph_write_chain_engine_governed(
                                &mut db, &query, &program, allowance
                            )
                            .is_err()
                    );
                    transaction.abort();
                }
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let forbidden =
            csv_chain("UNWIND $rows AS row MERGE (a:Hidden {p:row.p}) MERGE (b:Visible {p:row.q})");
        let mut no_work = policy(10);
        no_work.query.evaluator.max_work_units = 0;
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &forbidden,
                no_work,
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(
            authorization(&error),
            Some(Error::ScopeDenied),
            "authorization must precede input copying: {error:?}"
        );
        assert!(db.vertices().unwrap().is_empty());
    });
}

#[test]
fn write_only_private_rows_create_multiple_relations_in_one_reopenable_commit() {
    lab(0xc4a1_2001, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let program = chain(
            "CREATE (a:Visible {p:3}) WITH a CREATE (b:Visible {p:a.p+4}), \
             (a)-[:R]->(b), (b)-[:S]->(a)",
        );
        let (stats, result, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert_eq!(
            (result.rows.snapshot_records, result.rows.result_rows),
            (0, 0)
        );
        assert!(result.value.is_empty());
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, basis.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        let mut values: Vec<_> = vertices.iter().map(|row| row.props[0].1.clone()).collect();
        values.sort();
        assert_eq!(
            values,
            vec![
                CanonicalScalar::Int(3).into(),
                CanonicalScalar::Int(7).into()
            ]
        );
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].entry.src, edges[1].entry.dst);
        assert_eq!(edges[0].entry.dst, edges[1].entry.src);
        assert_eq!(txn.outstanding_obligations(), 0);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
    });
}

#[test]
fn final_rows_alone_spend_signed_rows_and_paging_keeps_all_creations() {
    lab(0xc4a1_2002, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        for rows in [0, 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let program = chain(&format!(
                "UNWIND [1,2] AS x CREATE (a:Visible {{p:x}}) WITH a \
                 CREATE (b:Visible {{p:a.p+10}}), (a)-[:R]->(b) \
                 RETURN a.p AS before, b.p AS after ORDER BY after DESC LIMIT {rows}"
            ));
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &limited,
                    "main",
                    &program,
                    policy(rows),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
            assert_eq!(result.value.len() as u64, rows);
            assert_eq!(result.rows.result_rows, rows);
            if rows == 1 {
                assert_eq!(
                    result.value[0].values(),
                    &[
                        GraphValue::Scalar(CanonicalScalar::Int(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(12)),
                    ]
                );
            }
            assert_eq!(
                (db.vertices().unwrap().len(), db.edges().unwrap().len()),
                (4, 2)
            );
        }
    });
}

#[test]
fn hidden_carried_fields_refuse_and_selected_inputs_remain_masked() {
    lab(0xc4a1_2003, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(40))]);
        db.write(&commit, seed).await.unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        for text in [
            "CREATE (a:Visible {p:1}) WITH a CREATE (b:Visible {p:a.secret})",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b CREATE (c:Visible {p:a.secret})",
        ] {
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ScopeDenied), "{error:?}");
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        }
        let program = chain(
            "MATCH (a:Visible) CREATE (b:Visible {p:a.p}) WITH a,b \
             CREATE (b)-[:R]->(a) RETURN b.p AS p",
        );
        let (stats, result, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(10),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (1, 1));
        assert_eq!(
            result.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(7))]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), before.1[1]);
    });
}

#[test]
fn a_late_scope_expression_quota_or_output_refusal_discards_every_step() {
    lab(0xc4a1_2004, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let cases = [
            ("CREATE (a:Visible) WITH a CREATE (b:Hidden)", 100),
            ("CREATE (a:Visible) WITH a CREATE (b:Visible {p:1/0})", 100),
            ("CREATE (a:Visible) WITH a CREATE (b:Visible)", 1),
        ];
        for (text, vertices) in cases {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let mut bounded = policy(10);
            bounded.max_vertices = vertices;
            let program = chain(text);
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "{text}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = db.frontier().unwrap();
        let program = chain("CREATE (a:Visible) WITH a CREATE (b:Visible) RETURN b");
        let mut called = 0;
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
                |rows, meter| {
                    called += 1;
                    assert_eq!(rows.len(), 1);
                    meter(1)?;
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(called, 1);
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn source_free_with_computation_spends_the_same_signed_work_permit() {
    lab(0xc4a1_2005, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(Rights::Write, 0), NOW)
            .unwrap()
            .attenuate(Restriction::MaxWork(512))
            .unwrap();
        for (value, succeeds) in [("512", true), ("size(range(1,512))", false)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let program = chain(&format!(
                "CREATE (a:Visible) WITH {value} AS n CREATE (b:Visible {{p:n}})"
            ));
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0),
                    || NOW,
                )
                .await;
            assert_eq!(result.is_ok(), succeeds, "{value}: {result:?}");
            if !succeeds {
                assert!(matches!(
                    authorization(&result.unwrap_err()),
                    Some(Error::LimitExceeded(_))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertices().unwrap().is_empty());
            }
        }
    });
}

#[test]
fn write_only_merges_mutations_and_public_reads_fail_before_publication() {
    lab(0xc4a1_2006, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 10), NOW).unwrap();
        for text in [
            "CREATE (a:Visible) WITH a CREATE (b:Visible) RETURN b",
            "MATCH (a:Visible) CREATE (b:Visible) WITH b CREATE (c:Visible)",
            "CREATE (a:Visible) SET a.p = 3",
            "CREATE (a:Visible) MERGE (b:Visible {p:3})",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.frontier().unwrap();
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::PermissionDenied),
                "{error:?}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn delete_projection_and_incidence_proof_share_each_native_allowance() {
    lab(0xc4a1_2014, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let complete = Grant {
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            ..grant(Rights::ReadWrite, 0)
        };
        let token = issuer.issue_at(&complete, NOW).unwrap();
        let program = chain(
            "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
             DELETE a RETURN size(range(1,32)) AS size LIMIT 0",
        );
        let mut measured = None;
        for attempt in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(7))]);
            seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(8))]);
            seed.create_vertex(VId(3), vec![L], vec![(P, CanonicalScalar::Int(9))]);
            // This unrelated visible edge must be inspected by the isolation
            // proof after the RETURN projection has already consumed its work.
            seed.add_edge(EId(1), VId(2), VId(3), vec![]);
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut bounded = policy(0).with_mutation_limit(1);
            if let Some((records, work, scratch)) = measured {
                bounded.query = GqlQueryPolicy::new(
                    records - u64::from(attempt == 4),
                    0,
                    work - u64::from(attempt == 2),
                    scratch - u64::from(attempt == 3),
                );
            }
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if attempt < 2 {
                let (_, result, _) = result.unwrap();
                let usage = (
                    result.rows.snapshot_records,
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                if let Some(expected) = measured {
                    assert_eq!(
                        usage, expected,
                        "exact cumulative limits must still succeed"
                    );
                } else {
                    assert!(usage.0 > 0 && usage.1 > 1 && usage.2 > 0);
                    measured = Some(usage);
                }
                assert!(result.value.is_empty(), "LIMIT 0 changes no effect");
                assert!(db.vertex(VId(1)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            } else {
                let error = result.unwrap_err();
                if attempt == 4 {
                    assert!(
                        matches!(error, fgdb_gql::GqlQueryError::Rows(_)),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(error, fgdb_gql::GqlQueryError::Evaluator(_)),
                        "{error:?}"
                    );
                }
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before,
                    "neither the creation nor deletion may publish after a combined-limit refusal",
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn created_vertex_and_edge_mutations_return_post_statement_values_and_reopen() {
    lab(0xc4a1_2011, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        for (text, expected, vertices, edges) in [
            (
                "CREATE (a:Visible {p:3}) SET a.p=a.p+4 RETURN a.p AS p",
                CanonicalScalar::Int(7),
                1,
                0,
            ),
            (
                "CREATE (a:Visible {p:5}) REMOVE a.p RETURN a.p AS p",
                CanonicalScalar::Null,
                1,
                0,
            ),
            (
                "CREATE (a:Visible), (b:Visible), (a)-[r:R {p:2}]->(b) \
                 SET r.p=r.p+6 RETURN r.p AS p",
                CanonicalScalar::Int(8),
                2,
                1,
            ),
        ] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let program = chain(text);
            let (stats, result, completion) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(1),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (vertices, edges)
            );
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(expected.clone())]
            );
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            let stored_vertices = db.vertices().unwrap();
            let stored_edges = db.edges().unwrap();
            let fields = if edges == 0 {
                &stored_vertices[0].props
            } else {
                &stored_edges[0].props
            };
            if expected == CanonicalScalar::Null {
                assert!(fields.is_empty(), "REMOVE must unset, not store NULL");
            } else {
                assert_eq!(fields, &vec![(P, expected.into())]);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), stored_vertices);
            assert_eq!(reopened.edges().unwrap(), stored_edges);
        }
    });
}

#[test]
fn selected_mutation_chains_preserve_hidden_fields_and_abort_every_late_refusal() {
    lab(0xc4a1_2012, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(40))]);
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.secret=99",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.p=a.secret",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b SET a.p=8 RETURN a.secret",
            "MATCH (a:Visible) CREATE (b:Visible) WITH a,b DETACH DELETE a",
        ] {
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{text}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let program = chain(
            "MATCH (a:Visible) CREATE (b:Visible {p:a.p}) WITH a,b \
             SET a.p=a.p+1 RETURN a.p AS updated, b.p AS previous",
        );
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(0),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mutation effect limit exceeded"));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before,
        );
        let mut calls = 0;
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1).with_mutation_limit(1),
                || NOW,
                |rows, meter| {
                    calls += 1;
                    assert_eq!(
                        rows[0].values(),
                        &[
                            GraphValue::Scalar(CanonicalScalar::Int(8)),
                            GraphValue::Scalar(CanonicalScalar::Int(7)),
                        ]
                    );
                    meter(1)?;
                    Err(WriteTxnError::Authorization(Error::ScopeDenied))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before,
        );
        db.execute_graph_write_chain_authorized(
            &txn,
            &query,
            &commit,
            &issuer,
            &token,
            "main",
            &program,
            policy(1).with_mutation_limit(1),
            || NOW,
        )
        .await
        .unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(8).into()),
                (SECRET, CanonicalScalar::Int(99).into()),
            ]
        );
        assert_eq!(db.vertex(VId(2)).unwrap().unwrap(), before.1[1]);
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn plain_and_detach_delete_chains_keep_native_incidence_rules_and_reopen() {
    lab(0xc4a1_2013, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let complete = Grant {
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            ..grant(Rights::ReadWrite, 1)
        };
        let token = issuer.issue_at(&complete, NOW).unwrap();
        for detaching in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(7))]);
            seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(8))]);
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let isolated = chain(
                "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
                 DELETE a RETURN audit.p AS p",
            );
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &isolated,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("incident relationship"),
                "{error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            let (text, effects) = if detaching {
                (
                    "MATCH (a:Visible {p:7}) CREATE (audit:Visible {p:99}) WITH a,audit \
                     DETACH DELETE a RETURN audit.p AS p",
                    1,
                )
            } else {
                (
                    "MATCH (a:Visible {p:7})-[r:R]->(b:Visible) \
                     CREATE (audit:Visible {p:99}) WITH a,r,audit \
                     DELETE a,r RETURN audit.p AS p",
                    2,
                )
            };
            let program = chain(text);
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(effects - 1),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("mutation effect limit exceeded"),
                "{error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            let (stats, result, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1).with_mutation_limit(effects),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(99))]
            );
            assert!(db.vertex(VId(1)).unwrap().is_none());
            assert!(db.vertex(VId(2)).unwrap().is_some());
            assert!(db.edges().unwrap().is_empty());
            let survivors = db.vertices().unwrap();
            assert_eq!(survivors.len(), 2);
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0.0 + 1);
            assert_eq!(txn.outstanding_obligations(), 0);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), survivors);
            assert!(reopened.edges().unwrap().is_empty());
        }
    });
}

#[test]
fn computed_create_output_and_private_with_read_the_admitted_staged_bag() {
    lab(0xc4a1_2014, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let definition = PreparedGraphInsertQueryText::prepare(
            "UNWIND [2,3] AS x CREATE (a:Visible {p:x})-[r:R {p:x*10}]->(b:Visible {p:x+1}) \
             WITH collect(a) AS ns, sum([e IN [r] | e.p][0]) AS total \
             RETURN [n IN ns | n.p] AS values, total",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (stats, rows, _) = db
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &definition,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 2));
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::List(
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Int(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(3))
                    ]
                    .into_boxed_slice()
                ),
                GraphValue::Scalar(CanonicalScalar::Int(50)),
            ]
        );
        let program = chain(
            "CREATE (a:Visible {p:7}) WITH a, [v IN [a] | v.p][0] AS old \
             CREATE (b:Visible {p:old+1}) RETURN [v IN [a,b] | v.p] AS values",
        );
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    GraphValue::Scalar(CanonicalScalar::Int(8))
                ]
                .into_boxed_slice()
            ),]
        );
        assert_eq!(db.vertices().unwrap().len(), 6);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_merge_continuations_match_native_bags_and_rollback_late_failures() {
    lab(0xc4a1_2017, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let mut scope = grant(Rights::ReadWrite, 3);
        scope.properties = Scope::only([P, SECRET]);
        let token = issuer.issue_at(&scope, NOW).unwrap();
        for (prefix, returned, edge_count) in [
            (
                "UNWIND [1,2,3] AS i MERGE (a:Visible {secret:1}) \
                 ON CREATE SET a.p=0 ON MATCH SET a.p=a.p+1 \
                 WITH a,[v IN [a] | v.p] AS prior \
                 MERGE (b:Visible {secret:2}) ON CREATE SET b.p=7",
                "b",
                0,
            ),
            (
                "CREATE (a:Visible {p:1}),(b:Visible {p:2}) \
                 WITH a,b UNWIND [1,2,3] AS i \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=0 ON MATCH SET r.p=r.p+1 \
                 WITH a,b,r,[e IN [r] | e.p] AS prior \
                 MERGE (a)-[s:S {p:7}]->(b)",
                "s",
                2,
            ),
            (
                "CREATE (a:Visible {p:1}),(b:Visible {p:2}) \
                 WITH a,b UNWIND [1,2,3] AS i \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=0 ON MATCH SET r.p=r.p+1 \
                 WITH a,b,r,[e IN [r] | e.p] AS prior \
                 MERGE (a)-[s:S]->(b) ON CREATE SET s.p=7",
                "s",
                2,
            ),
        ] {
            for fail_output in [false, true] {
                let expression = if fail_output { "1/(v.p-7)" } else { "v.p" };
                let text = format!(
                    "{prefix} RETURN prior,[v IN [{returned}] | {expression}] AS next {}",
                    if fail_output { "LIMIT 0" } else { "" },
                );
                let program = chain(&text);
                for authorized in [false, true] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let frontier = db.frontier().unwrap();
                    let output = if authorized {
                        let result = db
                            .execute_graph_write_chain_authorized(
                                &txn,
                                &query,
                                &commit,
                                &issuer,
                                &token,
                                "main",
                                &program,
                                policy(if fail_output { 0 } else { 3 }),
                                || NOW,
                            )
                            .await;
                        if fail_output {
                            assert!(result.is_err(), "authorized: {text}");
                            None
                        } else {
                            Some(result.unwrap().1)
                        }
                    } else {
                        let mut transaction = db.begin(&txn).unwrap();
                        let result = transaction.execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            &program,
                            policy(if fail_output { 0 } else { 3 }),
                        );
                        if fail_output {
                            assert!(result.is_err(), "native: {text}");
                            transaction.abort();
                            None
                        } else {
                            let output = result.unwrap().1;
                            transaction.finish(&mut db, &commit).await.unwrap();
                            Some(output)
                        }
                    };
                    if let Some(output) = output {
                        assert_eq!(output.value.len(), 3, "{text}");
                        for row in output.value {
                            assert_eq!(
                                row.values(),
                                &[
                                    GraphValue::List(
                                        vec![GraphValue::Scalar(CanonicalScalar::Int(2))]
                                            .into_boxed_slice(),
                                    ),
                                    GraphValue::List(
                                        vec![GraphValue::Scalar(CanonicalScalar::Int(7))]
                                            .into_boxed_slice(),
                                    ),
                                ],
                                "authorized={authorized}: {text}"
                            );
                        }
                        assert_eq!(db.vertices().unwrap().len(), 2);
                        assert_eq!(db.edges().unwrap().len(), edge_count);
                        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
                    } else {
                        assert!(db.vertices().unwrap().is_empty());
                        assert!(db.edges().unwrap().is_empty());
                        assert_eq!(db.frontier().unwrap(), frontier);
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        }
    });
}

#[test]
fn computed_create_output_masks_untrusted_identities_and_refuses_hidden_fields() {
    lab(0xc4a1_2015, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(11))]);
        seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(22))]);
        seed.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(33))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(44))]);
        seed.add_edge(EId(12), VId(1), VId(3), vec![(P, CanonicalScalar::Int(55))]);
        seed.add_edge(EId(13), VId(3), VId(2), vec![(P, CanonicalScalar::Int(66))]);
        db.write(&commit, seed).await.unwrap();
        let mut outside = WriteBatch::new(RelationId(3));
        outside.add_edge(EId(14), VId(1), VId(2), vec![(P, CanonicalScalar::Int(77))]);
        db.write(&commit, outside).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 10), NOW).unwrap();
        let definition = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            "CREATE (n:Visible {p:7}) RETURN [v IN $refs | v.p] AS values",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(
            &GqlParameters::new()
                .with_list(
                    "refs",
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Edge(EId(11)),
                        GraphValue::Vertex(VId(3)),
                        GraphValue::Vertex(VId(999)),
                        GraphValue::Edge(EId(12)),
                        GraphValue::Edge(EId(13)),
                        GraphValue::Edge(EId(14)),
                        GraphValue::Edge(EId(999)),
                    ],
                )
                .unwrap(),
        )
        .unwrap();
        let (_, rows, _) = db
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &definition,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        let mut expected = vec![
            GraphValue::Scalar(CanonicalScalar::Int(11)),
            GraphValue::Scalar(CanonicalScalar::Int(44)),
        ];
        expected.extend((0..6).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::List(expected.into_boxed_slice())]
        );
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for text in [
            "CREATE (n:Visible {p:7}) RETURN [v IN [n] | v.secret][0] AS hidden",
            "UNWIND [1,0] AS x CREATE (n:Visible {p:x}) RETURN [v IN [n] | 1/v.p][0] AS value LIMIT 0",
        ] {
            let definition = PreparedGraphInsertQueryText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let error = db
                .execute_graph_insert_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &definition,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap_err();
            if text.contains("secret") {
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn blind_create_continuations_cannot_read_parameter_carried_element_ids() {
    lab(0xc4a1_2016, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(11))]);
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(22))]);
        db.write(&commit, seed).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
            "CREATE (a:Visible) WITH [v IN $refs | v.p][0] AS copied \
             CREATE (b:Visible {p:copied})",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        for identity in [
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(2)),
            GraphValue::Vertex(VId(999)),
            GraphValue::Edge(EId(999)),
        ] {
            let program = template
                .bind_parameters(
                    &GqlParameters::new()
                        .with_list("refs", vec![identity])
                        .unwrap(),
                )
                .unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::PermissionDenied),
                "{error:?}"
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        }
        // Returning a known CREATE binding to a private continuation retains
        // the existing blind-write contract; it performs no element lookup.
        let own = chain("CREATE (a:Visible {p:3}) WITH a, a.p AS p CREATE (b:Visible {p:p+1})");
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &own,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert!(rows.value.is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

fn post_write_pattern(key: PropertyKeyId) -> fgdb_gql::PreparedGraphSet {
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison, VertexPredicate};
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("matched").unwrap();
    graph
        .filter(
            "matched",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Equal,
                value: 1,
            },
        )
        .unwrap();
    graph
        .prepare_values(
            &[
                GraphColumn::vertex("matched", "matched"),
                GraphColumn::property("matched_p", "matched", key),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}

fn post_write_read(
    chain: &PreparedGraphWriteChain,
    key: PropertyKeyId,
) -> fgdb_gql::PreparedGraphSet {
    use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
    let left = chain.read_input().unwrap();
    let right = post_write_pattern(key);
    let join = RowJoinSpec::new(left.column_types(), right.column_types(), &[(0, 0)])
        .unwrap()
        .with_kind(RowJoinKind::Left);
    left.join(right, join).unwrap()
}

#[test]
fn authorized_post_write_read_sees_staged_bags_masks_hidden_rows_and_reopens() {
    lab(0xc4a1_20d1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P);
        let mut expected_usage = None;
        for hidden in [0, 32] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            if hidden != 0 {
                let mut seed = WriteBatch::new(R);
                for id in 1..=hidden {
                    seed.create_vertex(VId(id), vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
                }
                db.write(&commit, seed).await.unwrap();
            }
            let before = db.frontier().unwrap();
            let (stats, result, completion) = db
                .execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(3),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (2, 0));
            assert_eq!(stats.selection, result.rows);
            assert_eq!(stats.evaluator, result.evaluator);
            assert_eq!(result.rows.result_rows, 3);
            let mut observed = Vec::new();
            for row in &result.value {
                let [GraphValue::Vertex(id), matched, value] = row.values() else {
                    panic!("unexpected continuation row: {row:?}")
                };
                let vertex = db.vertex(*id).unwrap().unwrap();
                let p = vertex
                    .props
                    .iter()
                    .find(|(key, _)| *key == P)
                    .unwrap()
                    .1
                    .clone();
                if p == CanonicalScalar::Int(1).into() {
                    assert_eq!(matched, &GraphValue::Vertex(*id));
                    assert_eq!(value, &GraphValue::Scalar(CanonicalScalar::Int(1)));
                    observed.push(1);
                } else {
                    assert_eq!(p, CanonicalScalar::Int(2).into());
                    assert_eq!(matched, &GraphValue::Scalar(CanonicalScalar::Null));
                    assert_eq!(value, &GraphValue::Scalar(CanonicalScalar::Null));
                    observed.push(2);
                }
            }
            observed.sort_unstable();
            assert_eq!(observed, [1, 1, 2]);
            if let Some(expected) = expected_usage {
                assert_eq!(stats, expected, "hidden rows cannot affect public usage");
            } else {
                expected_usage = Some(stats);
            }
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            let expected = db.vertices().unwrap();
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), expected);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_charges_only_final_rows_and_admits_them_before_commit() {
    use fgdb_gql::{GraphSetOperand, GraphSetPredicateOp};
    lab(0xc4a1_20d2, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P)
            .filter(&[GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            }])
            .unwrap();
        for refuse_output in [true, false] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut called = 0;
            let result = db
                .execute_graph_write_chain_then_read_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(1),
                    || NOW,
                    |rows, meter| {
                        called += 1;
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].values().len(), 3);
                        meter(1)?;
                        if refuse_output {
                            Err(WriteTxnError::Authorization(Error::ScopeDenied))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            assert_eq!(called, 1);
            if refuse_output {
                assert_eq!(
                    authorization(&result.unwrap_err()),
                    Some(Error::ScopeDenied)
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
            } else {
                let (stats, result, _) = result.unwrap();
                assert_eq!(stats.created_vertices, 2);
                assert_eq!(result.rows.result_rows, 1);
                assert_eq!(db.vertices().unwrap().len(), 2);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_schema_rights_scope_and_row_failures_publish_nothing() {
    lab(0xc4a1_20d3, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        for case in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let rights = if case == 1 {
                Rights::Write
            } else {
                Rights::ReadWrite
            };
            let token = issuer
                .issue_at(&grant(rights, if case == 3 { 2 } else { 3 }), NOW)
                .unwrap();
            let continuation = if case == 0 {
                post_write_pattern(P)
            } else {
                post_write_read(&program, if case == 4 { SECRET } else { P })
            };
            let error = db
                .execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    policy(if case == 2 { 2 } else { 3 }),
                    || NOW,
                )
                .await
                .unwrap_err();
            if case == 1 {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            } else if case == 4 {
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
            assert_eq!(db.frontier().unwrap(), before, "case {case}: {error:?}");
            assert!(db.vertices().unwrap().is_empty(), "case {case}");
            assert!(db.edges().unwrap().is_empty(), "case {case}");
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_post_write_read_shares_each_native_allowance_with_its_write_prefix() {
    lab(0xc4a1_20d4, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain("UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) RETURN n");
        let continuation = post_write_read(&program, P);
        let mut write_db = Database::open_memory(&commit, keys()).await.unwrap();
        let (prefix, _, _) = write_db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(3),
                || NOW,
            )
            .await
            .unwrap();
        let mut complete_db = Database::open_memory(&commit, keys()).await.unwrap();
        let (complete, _, _) = complete_db
            .execute_graph_write_chain_then_read_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                &continuation,
                policy(3),
                || NOW,
            )
            .await
            .unwrap();
        assert!(complete.selection.snapshot_records > prefix.selection.snapshot_records);
        assert!(complete.evaluator.work_units > prefix.evaluator.work_units);
        assert!(complete.evaluator.scratch_entries > prefix.evaluator.scratch_entries);
        for dimension in 0..3 {
            let mut bounded = policy(3);
            match dimension {
                0 => {
                    bounded.query = GqlQueryPolicy::new(
                        complete.selection.snapshot_records - 1,
                        3,
                        bounded.query.evaluator.max_work_units,
                        bounded.query.evaluator.max_scratch_entries,
                    );
                }
                1 => bounded.query.evaluator.max_work_units = complete.evaluator.work_units - 1,
                _ => {
                    bounded.query.evaluator.max_scratch_entries =
                        complete.evaluator.scratch_entries - 1
                }
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_then_read_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    &continuation,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "dimension {dimension}"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn textual_post_write_match_preserves_bags_optional_nulls_and_durable_overlay() {
    lab(0xc4a1_20e1, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        let program = chain(
            "UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) WITH n \
             OPTIONAL MATCH (m:Visible {p:1}) WHERE m=n \
             RETURN n.p AS p,m.p AS found ORDER BY p",
        );
        assert_eq!(program.columns(), &["p", "found"]);
        let mut authorized_usage = None;
        for authorized in [false, true] {
            for hidden in [0_u128, 16] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                if hidden != 0 {
                    let mut seed = WriteBatch::new(R);
                    for id in 1..=hidden {
                        seed.create_vertex(
                            VId(id),
                            vec![HIDDEN],
                            vec![(P, CanonicalScalar::Int(1))],
                        );
                    }
                    db.write(&commit, seed).await.unwrap();
                }
                let before = db.frontier().unwrap();
                let (stats, result, completion) = if authorized {
                    db.execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(3),
                        || NOW,
                    )
                    .await
                    .unwrap()
                } else {
                    let mut transaction = db.begin(&txn).unwrap();
                    let (stats, rows, _) = transaction
                        .execute_graph_write_chain_engine_governed(
                            &mut db,
                            &query,
                            &program,
                            policy(3),
                        )
                        .unwrap();
                    let completion = transaction.finish(&mut db, &commit).await.unwrap();
                    (stats, rows, completion)
                };
                assert_eq!((stats.created_vertices, stats.created_edges), (2, 0));
                assert_eq!(stats.selection, result.rows);
                assert_eq!(stats.evaluator, result.evaluator);
                assert_eq!(
                    result
                        .value
                        .iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    vec![
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Int(1)),
                            GraphValue::Scalar(CanonicalScalar::Int(1))
                        ],
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Int(1)),
                            GraphValue::Scalar(CanonicalScalar::Int(1))
                        ],
                        vec![
                            GraphValue::Scalar(CanonicalScalar::Int(2)),
                            GraphValue::Scalar(CanonicalScalar::Null)
                        ],
                    ]
                );
                if authorized {
                    if let Some(expected) = authorized_usage {
                        assert_eq!(
                            stats, expected,
                            "hidden vertices cannot change public counters"
                        );
                    } else {
                        authorized_usage = Some(stats);
                    }
                }
                let frontier = db.frontier().unwrap();
                assert_eq!(frontier.0, before.0 + 1);
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: frontier
                    }
                );
                let expected = db.vertices().unwrap();
                drop(db);
                let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), frontier);
                assert_eq!(reopened.vertices().unwrap(), expected);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn public_write_text_reads_final_state_and_counts_private_occurrences_once() {
    lab(0xc4a1_20e2, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let budget = fgdb_gql::GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(10_000, 1, 1_000_000, 100_000),
            100,
            100,
            100,
        );
        for (text, expected, vertices, edges) in [
            (
                "CREATE (n:Visible {p:7}) WITH n MATCH (n) RETURN sum(n.p) AS total",
                7,
                1,
                0,
            ),
            (
                "CREATE (n:Visible {p:1}) SET n.p=7 WITH n MATCH (n) RETURN sum(n.p) AS total",
                7,
                1,
                0,
            ),
            (
                "UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) OPTIONAL MATCH (m:Visible) RETURN count(m) AS total",
                6,
                2,
                0,
            ),
            (
                "MERGE (a:Visible {p:1}) MERGE (b:Visible {p:2}) MERGE (a)-[r:R]->(b) WITH a,r,b MATCH (a)-[r:R]->(b) RETURN count(*) AS total",
                1,
                2,
                1,
            ),
            (
                "CREATE (n:Visible {p:1}) WITH n MATCH (m:Visible {p:99}) RETURN count(*) AS total",
                0,
                1,
                0,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let result = db
                .query_write_engine(
                    &txn,
                    &query,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    budget,
                )
                .await
                .expect(text);
            let fgdb::QueryResult::Rows { columns, rows } = result else {
                panic!("a final read must return its public schema and rows: {text}");
            };
            assert_eq!(columns, ["total"], "{text}");
            assert_eq!(
                rows,
                vec![vec![fgdb::QueryValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(expected)
                ),)]],
                "{text}"
            );
            assert_eq!(db.vertices().unwrap().len(), vertices, "{text}");
            assert_eq!(db.edges().unwrap().len(), edges, "{text}");
            assert_eq!(db.frontier().unwrap().0, before.0 + 1, "{text}");
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn textual_read_after_set_keeps_frozen_aliases_and_reads_current_properties() {
    lab(0xc4a1_20e3, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "CREATE (n:Visible {p:1}) WITH n,n.p AS before \
             SET n.p=before+1 WITH n,before MATCH (n) RETURN before,n.p AS after",
        );
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (_, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::Scalar(CanonicalScalar::Int(2)),
            ]
        );
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(2).into())]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn terminal_read_failure_even_at_limit_zero_restores_the_prior_transaction_prefix() {
    lab(0xc4a1_20e4, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txn).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![L], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        for limit in ["", " LIMIT 0"] {
            let program = chain(&format!(
                "UNWIND [1,2] AS k MERGE (n:Visible {{p:k}}) WITH n \
                 MATCH (m:Visible) RETURN 1/(m.p-2) AS value{limit}",
            ));
            transaction
                .execute_graph_write_chain_engine_governed(&mut db, &query, &program, policy(100))
                .expect_err("a late arithmetic error must discard every new effect");
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
            assert_eq!(db.frontier().unwrap(), before);
        }
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn textual_read_authority_and_final_host_admission_precede_publication() {
    lab(0xc4a1_20e5, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for case in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let rights = if case == 0 {
                Rights::Write
            } else {
                Rights::ReadWrite
            };
            let token = issuer.issue_at(&grant(rights, 1), NOW).unwrap();
            let program = chain(if case == 1 {
                "CREATE (n:Visible {p:1}) WITH n MATCH (n) RETURN n.secret"
            } else if case == 2 {
                "UNWIND [1,2] AS k CREATE (n:Visible {p:k}) WITH n MATCH (n) RETURN n.p"
            } else {
                "UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) WITH n \
                 MATCH (m:Visible) RETURN count(*) AS total"
            });
            let mut called = 0;
            let error = db
                .execute_graph_write_chain_authorized_checked(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(100),
                    || NOW,
                    |rows, meter| {
                        called += 1;
                        assert_eq!(
                            case, 3,
                            "earlier refusals cannot reach host output admission"
                        );
                        assert_eq!(
                            rows[0].values(),
                            &[GraphValue::Scalar(CanonicalScalar::Int(6))]
                        );
                        meter(1)?;
                        Err(WriteTxnError::Authorization(Error::ScopeDenied))
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(called, usize::from(case == 3));
            if case == 0 {
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            } else if case == 1 || case == 3 {
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn textual_read_limits_are_cumulative_and_noop_merge_reads_do_not_replay_effects() {
    lab(0xc4a1_20e6, |contexts| async move {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "UNWIND [1,1,2] AS k MERGE (n:Visible {p:k}) WITH n \
             MATCH (m:Visible) RETURN count(*) AS total",
        );
        let mut reference = Database::open_memory(&commit, keys()).await.unwrap();
        let (complete, rows, _) = reference
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(6))]
        );
        for dimension in 0..3 {
            let mut bounded = policy(1);
            match dimension {
                0 => {
                    bounded.query = GqlQueryPolicy::new(
                        complete.selection.snapshot_records - 1,
                        1,
                        bounded.query.evaluator.max_work_units,
                        bounded.query.evaluator.max_scratch_entries,
                    )
                }
                1 => bounded.query.evaluator.max_work_units = complete.evaluator.work_units - 1,
                _ => {
                    bounded.query.evaluator.max_scratch_entries =
                        complete.evaluator.scratch_entries - 1
                }
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "dimension {dimension}"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }
        let before = reference.frontier().unwrap();
        let (stats, rows, _) = reference
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(6))]
        );
        assert_eq!(reference.frontier().unwrap(), before);
        assert_eq!(reference.vertices().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

mod post_write_deletions {
    use super::*;
    use fgdb_gql::algebra::GraphValueRow;
    use fgdb_gql::insertion::GraphInsertStats;
    use fgdb_gql::{GqlQueryError, GqlQueryExecution, GraphWriteChainError};
    use fgdb_warden::LimitDimension;

    type Fault = GqlQueryError<GraphWriteChainError<WriteTxnError, WriteTxnError>, WriteTxnError>;
    type Receipt = (
        GraphInsertStats,
        GqlQueryExecution<GraphValueRow>,
        EmbeddedTxnCompletion,
    );

    const EDGE_DELETE: &str = "CREATE (audit:Visible {p:99}) WITH audit \
        MATCH (a:Visible {p:7})-[r:R]->(b:Visible) \
        UNWIND [0,0] AS occurrence DELETE r \
        RETURN audit.p AS p, occurrence ORDER BY occurrence";

    fn edge_grant(rows: u64) -> Grant {
        // Deleting a complete edge requires authority over all its fields;
        // endpoint-label and relation masking still restricts the graph.
        Grant {
            properties: Scope::All,
            ..grant(Rights::ReadWrite, rows)
        }
    }

    fn unrestricted(rows: u64) -> Grant {
        Grant {
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            ..grant(Rights::ReadWrite, rows)
        }
    }

    async fn delete_edges(
        contexts: &PurposeContexts,
        hidden: bool,
        capability: Grant,
        budget: GraphInsertPolicy,
    ) -> Result<Receipt, Fault> {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&capability, NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(10), vec![L], vec![(P, CanonicalScalar::Int(7))]);
        seed.create_vertex(VId(20), vec![L], vec![(P, CanonicalScalar::Int(8))]);
        seed.add_edge(EId(1), VId(10), VId(20), vec![]);
        if hidden {
            seed.create_vertex(VId(90), vec![HIDDEN], vec![(P, CanonicalScalar::Int(8))]);
            seed.add_edge(EId(90), VId(10), VId(90), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let result = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(EDGE_DELETE),
                budget,
                || NOW,
            )
            .await;
        if let Ok((stats, rows, completion)) = &result {
            assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
            assert_eq!(rows.rows.result_rows, 2);
            for row in &rows.value {
                assert_eq!(
                    row.values(),
                    &[
                        GraphValue::Scalar(CanonicalScalar::Int(99)),
                        GraphValue::Scalar(CanonicalScalar::Int(0)),
                    ]
                );
            }
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            assert_eq!(
                *completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap()
                }
            );
            let vertices = db.vertices().unwrap();
            for original in &before.1 {
                assert_eq!(db.vertex(original.vid).unwrap().as_ref(), Some(original));
            }
            assert_eq!(vertices.len(), before.1.len() + 1);
            let edges = db.edges().unwrap();
            assert_eq!(
                edges,
                before
                    .2
                    .into_iter()
                    .filter(|edge| edge.entry.eid != EId(1))
                    .collect::<Vec<_>>()
            );
            let frontier = db.frontier().unwrap();
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(reopened.vertices().unwrap(), vertices);
            assert_eq!(reopened.edges().unwrap(), edges);
        } else {
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "a refused delete must discard the earlier CREATE"
            );
        }
        assert_eq!(txn.outstanding_obligations(), 0);
        result
    }

    #[test]
    fn matched_edge_deletion_preserves_occurrences_hidden_data_and_recovery() {
        lab(0xc4a1_2101, |contexts| async move {
            let expected = delete_edges(&contexts, false, edge_grant(2), policy(2))
                .await
                .unwrap();
            let hidden = delete_edges(&contexts, true, edge_grant(2), policy(2))
                .await
                .unwrap();
            assert_eq!(
                hidden, expected,
                "hidden incidence changes neither rows nor metering"
            );
            for hidden in [false, true] {
                let error = delete_edges(&contexts, hidden, grant(Rights::ReadWrite, 2), policy(2))
                    .await
                    .expect_err("a property-limited grant cannot delete the whole edge");
                assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            }
        });
    }

    #[test]
    fn matched_edge_deletion_shares_budgets_and_delivers_only_final_occurrences() {
        lab(0xc4a1_2102, |contexts| async move {
            let expected = delete_edges(&contexts, true, edge_grant(2), policy(2))
                .await
                .unwrap();
            let measured = expected.0;
            assert!(measured.selection.snapshot_records > 0);
            assert!(measured.evaluator.work_units > 0);
            assert!(measured.evaluator.scratch_entries > 0);
            let exact = GraphInsertPolicy::new(
                GqlQueryPolicy::new(
                    measured.selection.snapshot_records,
                    2,
                    measured.evaluator.work_units,
                    measured.evaluator.scratch_entries,
                ),
                1,
                0,
            )
            .with_mutation_limit(1);
            for hidden in [false, true] {
                assert_eq!(
                    delete_edges(&contexts, hidden, edge_grant(2), exact)
                        .await
                        .unwrap(),
                    expected
                );
                for dimension in 0..6 {
                    let mut budget = exact;
                    let mut capability = edge_grant(2);
                    match dimension {
                        0 => {
                            budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                                measured.selection.snapshot_records - 1,
                                2,
                            )
                        }
                        1 => budget.query.evaluator.max_work_units -= 1,
                        2 => budget.query.evaluator.max_scratch_entries -= 1,
                        3 => budget = budget.with_mutation_limit(0),
                        4 => {
                            budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                                measured.selection.snapshot_records,
                                1,
                            )
                        }
                        _ => capability.limits.max_rows = 1,
                    }
                    let error = delete_edges(&contexts, hidden, capability, budget)
                        .await
                        .expect_err("every stage must spend the shared statement allowance");
                    if dimension == 5 {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::LimitExceeded(LimitDimension::Rows))
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn matched_vertex_deletion_keeps_incidence_proofs_and_atomic_rollback() {
        lab(0xc4a1_2103, |contexts| async move {
            let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
            let issuer = authority();
            let token = issuer.issue_at(&unrestricted(1), NOW).unwrap();
            for action in ["DELETE a", "DELETE a,r", "DETACH DELETE a"] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(10), vec![L], vec![(P, CanonicalScalar::Int(7))]);
                seed.create_vertex(VId(20), vec![L], vec![(P, CanonicalScalar::Int(8))]);
                seed.add_edge(EId(1), VId(10), VId(20), vec![]);
                db.write(&commit, seed).await.unwrap();
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let program = chain(&format!(
                    "CREATE (audit:Visible {{p:99}}) WITH audit \
                     MATCH (a:Visible {{p:7}})-[r:R]->(b:Visible) \
                     {action} RETURN audit.p AS p"
                ));
                let result = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(1),
                        || NOW,
                    )
                    .await;
                if action == "DELETE a" {
                    let error = result.unwrap_err();
                    assert!(
                        error.to_string().contains("incident relationship"),
                        "{error:?}"
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before,
                    );
                } else {
                    let (_, rows, _) = result.unwrap();
                    assert_eq!(
                        rows.value[0].values(),
                        &[GraphValue::Scalar(CanonicalScalar::Int(99))]
                    );
                    assert!(db.vertex(VId(10)).unwrap().is_none());
                    assert_eq!(db.vertex(VId(20)).unwrap().as_ref(), Some(&before.1[1]));
                    assert!(db.edges().unwrap().is_empty());
                    assert_eq!(db.vertices().unwrap().len(), 2);
                    assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn scoped_matched_vertex_deletion_cannot_probe_hidden_incidence() {
        lab(0xc4a1_2104, |contexts| async move {
            let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
            for hidden in [false, true] {
                for action in ["DELETE a", "DETACH DELETE a"] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let mut seed = WriteBatch::new(R);
                    seed.create_vertex(VId(10), vec![L], vec![(P, CanonicalScalar::Int(7))]);
                    if hidden {
                        seed.create_vertex(VId(90), vec![HIDDEN], vec![]);
                        seed.add_edge(EId(90), VId(10), VId(90), vec![]);
                    }
                    db.write(&commit, seed).await.unwrap();
                    let before = (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap(),
                    );
                    let program = chain(&format!(
                        "CREATE (audit:Visible {{p:99}}) WITH audit \
                         MATCH (a:Visible {{p:7}}) {action} RETURN audit.p LIMIT 0"
                    ));
                    let error = db
                        .execute_graph_write_chain_authorized(
                            &txn,
                            &query,
                            &commit,
                            &issuer,
                            &token,
                            "main",
                            &program,
                            policy(1),
                            || NOW,
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(
                        authorization(&error),
                        Some(Error::ScopeDenied),
                        "{action}: {error:?}"
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before,
                        "scope refusal must discard the complete write prefix even with LIMIT 0",
                    );
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        });
    }
}

mod post_write_mutations {
    use super::*;
    use fgdb_gql::algebra::GraphValueRow;
    use fgdb_gql::insertion::GraphInsertStats;
    use fgdb_gql::{GqlQueryError, GqlQueryExecution, GraphWriteChainError};
    use fgdb_warden::LimitDimension;

    type Fault = GqlQueryError<GraphWriteChainError<WriteTxnError, WriteTxnError>, WriteTxnError>;
    type Receipt = (
        GraphInsertStats,
        GqlQueryExecution<GraphValueRow>,
        EmbeddedTxnCompletion,
    );

    const UPDATE: &str = "CREATE (a:Visible {p:1}) WITH a,a.p AS frozen SET a.p=3 \
         WITH a,frozen MATCH (n:Visible) WHERE n.p>=7 AND n.p<=8 \
         SET n.p=n.p+a.p+frozen RETURN frozen,a.p,n.p ORDER BY n.p";

    fn values(cells: &[i64]) -> Vec<GraphValue> {
        cells
            .iter()
            .map(|&value| GraphValue::Scalar(CanonicalScalar::Int(value)))
            .collect()
    }

    // The hidden fixture differs only by forbidden records/fields. Every
    // execution goes through the public capability-checked autocommit API.
    // The owner view is used solely to assert publication and preservation.
    async fn execute(
        contexts: &PurposeContexts,
        program: &PreparedGraphWriteChain,
        hidden: bool,
        capability: Grant,
        budget: GraphInsertPolicy,
    ) -> Result<Receipt, Fault> {
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&capability, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, value) in [(10, 7), (20, 8)] {
            let mut fields = vec![(P, CanonicalScalar::Int(value))];
            if hidden {
                fields.push((SECRET, CanonicalScalar::Int(99)));
            }
            seed.create_vertex(VId(id), vec![L], fields);
        }
        if hidden {
            for id in 101..109 {
                seed.create_vertex(
                    VId(id),
                    vec![HIDDEN],
                    vec![
                        (P, CanonicalScalar::Int(7 + (id % 2) as i64)),
                        (SECRET, CanonicalScalar::Int(id as i64)),
                    ],
                );
            }
        }
        db.write(&commit, seed).await.unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let result = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                program,
                budget,
                || NOW,
            )
            .await;
        if let Ok((stats, _, completion)) = &result {
            let frontier = db.frontier().unwrap();
            assert_eq!(frontier.0, before.0.0 + 1);
            assert_eq!(
                *completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: frontier
                }
            );
            assert_eq!(
                db.vertices().unwrap().len() as u64,
                before.1.len() as u64 + stats.created_vertices
            );
            for original in &before.1 {
                let current = db.vertex(original.vid).unwrap().unwrap();
                if original.labels.contains(&HIDDEN) {
                    assert_eq!(&current, original);
                } else {
                    assert_eq!(
                        current
                            .props
                            .iter()
                            .filter(|(key, _)| *key != P)
                            .collect::<Vec<_>>(),
                        original
                            .props
                            .iter()
                            .filter(|(key, _)| *key != P)
                            .collect::<Vec<_>>()
                    );
                }
            }
            assert_eq!(db.edges().unwrap(), before.2);
        } else {
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "a failed continuation must discard its CREATE and every prior SET"
            );
        }
        assert_eq!(txn.outstanding_obligations(), 0);
        result
    }

    #[test]
    fn owned_matches_preserve_occurrences_frozen_aliases_and_current_properties() {
        lab(0xc4a1_20f1, |contexts| async move {
            let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite, 4), NOW).unwrap();
            for (text, expected, stored) in [
                (
                    "CREATE (a:Visible {p:1}),(b:Visible {p:7}) \
                     WITH a,b,a.p AS frozen SET a.p=3 WITH a,b,frozen \
                     MATCH (n:Visible {p:7}) SET n.p=a.p+frozen \
                     RETURN frozen,a.p,n.p",
                    vec![values(&[1, 3, 4])],
                    vec![Some(3), Some(4)],
                ),
                (
                    "CREATE (a:Visible {p:1}),(b:Visible {p:9}) \
                     WITH a,a.p AS frozen MATCH (n:Visible {p:9}) \
                     SET a.p=7 RETURN frozen,a.p,n.p",
                    vec![values(&[1, 7, 9])],
                    vec![Some(7), Some(9)],
                ),
                (
                    "UNWIND [1,2] AS k CREATE (a:Visible {p:k}) WITH a,k \
                     MATCH (a) UNWIND [0,0] AS copy SET a.p=a.p+10 \
                     WITH a,k,a.p AS updated MATCH (a) REMOVE a.p \
                     RETURN k,updated,a.p ORDER BY k",
                    [1, 1, 2, 2]
                        .into_iter()
                        .map(|key| {
                            let mut row = values(&[key, key + 10]);
                            row.push(GraphValue::Scalar(CanonicalScalar::Null));
                            row
                        })
                        .collect(),
                    vec![None, None],
                ),
            ] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let program = chain(text);
                let (stats, rows, completion) = db
                    .execute_graph_write_chain_authorized(
                        &txn,
                        &query,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        policy(4),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (2, 0));
                assert_eq!(rows.rows.result_rows, expected.len() as u64);
                assert_eq!(
                    rows.value
                        .iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    expected,
                    "{text}"
                );
                let vertices = db.vertices().unwrap();
                let mut actual: Vec<_> = vertices
                    .iter()
                    .map(|vertex| {
                        vertex
                            .props
                            .iter()
                            .find(|(key, _)| *key == P)
                            .map(|(_, value)| value.clone())
                    })
                    .collect();
                actual.sort();
                let mut expected: Vec<Option<fgdb_types::CanonicalPropertyValue>> = stored
                    .into_iter()
                    .map(|value| value.map(|value| CanonicalScalar::Int(value).into()))
                    .collect();
                expected.sort();
                assert_eq!(actual, expected, "{text}");
                assert!(db.edges().unwrap().is_empty());
                let frontier = db.frontier().unwrap();
                assert_eq!(frontier.0, 1);
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: frontier
                    }
                );
                drop(db);
                let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), frontier);
                assert_eq!(reopened.vertices().unwrap(), vertices);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    fn matched_merge() -> PreparedGraphWriteChain {
        let text = "CREATE (audit:Visible {p:100}) WITH audit \
            MATCH (n:Visible) WHERE n.p<>100 UNWIND [0,0] AS occurrence \
            MERGE (made:Visible {p:n.p+audit.p+$delta}) \
            RETURN audit.p,n.p,made.p ORDER BY n.p";
        assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
        PreparedGraphWriteChainText::prepare(text, R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new().with_int64("delta", 10).unwrap())
            .unwrap()
    }

    #[test]
    fn post_match_vertex_merge_freezes_selection_and_resolves_duplicate_keys() {
        lab(0xc4a1_2111, |contexts| async move {
            let program = matched_merge();
            let expected = execute(
                &contexts,
                &program,
                false,
                grant(Rights::ReadWrite, 4),
                policy(4),
            )
            .await
            .unwrap();
            assert_eq!(
                expected.0.created_vertices, 3,
                "one audit and two distinct MERGE keys"
            );
            assert_eq!(
                expected
                    .1
                    .value
                    .iter()
                    .map(|row| row.values().to_vec())
                    .collect::<Vec<_>>(),
                [
                    values(&[100, 7, 117]),
                    values(&[100, 7, 117]),
                    values(&[100, 8, 118]),
                    values(&[100, 8, 118])
                ],
                "new MERGE vertices satisfy MATCH too, but cannot enter its frozen input",
            );
            assert_eq!(
                execute(
                    &contexts,
                    &program,
                    true,
                    grant(Rights::ReadWrite, 4),
                    policy(4)
                )
                .await
                .unwrap(),
                expected,
                "hidden vertices and fields change neither rows nor metering",
            );
        });
    }

    #[test]
    fn post_match_vertex_merge_refusals_discard_the_complete_write_prefix() {
        lab(0xc4a1_2112, |contexts| async move {
            for (tail, scope_failure) in [
                ("MERGE (made:Visible {secret:n.p}) RETURN made.p", true),
                ("MERGE (made:Visible {p:n.p+a.secret}) RETURN made.p", true),
                (
                    "MERGE (made:Visible {p:n.p+10}) ON CREATE SET made.secret=1 RETURN made.p LIMIT 0",
                    true,
                ),
                (
                    "MERGE (made:Visible {p:CASE WHEN n.p=8 THEN null ELSE n.p+10 END}) RETURN made.p LIMIT 0",
                    false,
                ),
            ] {
                let program = chain(&format!(
                    "CREATE (a:Visible {{p:1}}) WITH a \
                     MATCH (n:Visible) WHERE n.p>=7 AND n.p<=8 {tail}"
                ));
                for hidden in [false, true] {
                    let error = execute(
                        &contexts,
                        &program,
                        hidden,
                        grant(Rights::ReadWrite, 2),
                        policy(2),
                    )
                    .await
                    .unwrap_err();
                    if scope_failure {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::ScopeDenied),
                            "{tail}: {error:?}"
                        );
                    } else {
                        assert!(
                            error.to_string().to_lowercase().contains("null"),
                            "{tail}: {error:?}"
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn post_match_vertex_merge_shares_native_creation_and_delivery_allowances() {
        lab(0xc4a1_2113, |contexts| async move {
            let program = matched_merge();
            let expected = execute(
                &contexts,
                &program,
                true,
                grant(Rights::ReadWrite, 4),
                policy(4),
            )
            .await
            .unwrap();
            let measured = expected.0;
            assert!(measured.selection.snapshot_records > 0);
            assert!(measured.evaluator.work_units > 0);
            assert!(measured.evaluator.scratch_entries > 0);
            let exact = GraphInsertPolicy::new(
                GqlQueryPolicy::new(
                    measured.selection.snapshot_records,
                    4,
                    measured.evaluator.work_units,
                    measured.evaluator.scratch_entries,
                ),
                3,
                0,
            );
            assert_eq!(
                execute(
                    &contexts,
                    &program,
                    true,
                    grant(Rights::ReadWrite, 4),
                    exact
                )
                .await
                .unwrap(),
                expected,
            );
            for dimension in 0..6 {
                let mut budget = exact;
                let mut capability = grant(Rights::ReadWrite, 4);
                match dimension {
                    0 => {
                        budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                            measured.selection.snapshot_records - 1,
                            4,
                        )
                    }
                    1 => budget.query.evaluator.max_work_units -= 1,
                    2 => budget.query.evaluator.max_scratch_entries -= 1,
                    3 => budget.max_vertices -= 1,
                    4 => {
                        budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                            measured.selection.snapshot_records,
                            3,
                        )
                    }
                    _ => capability.limits.max_rows = 3,
                }
                let error = execute(&contexts, &program, true, capability, budget)
                    .await
                    .expect_err("MATCH, MERGE and output share the complete allowance");
                if dimension == 5 {
                    assert_eq!(
                        authorization(&error),
                        Some(Error::LimitExceeded(LimitDimension::Rows))
                    );
                }
            }
        });
    }

    #[test]
    fn hidden_match_rows_and_fields_do_not_change_results_or_signed_work() {
        lab(0xc4a1_20f2, |contexts| async move {
            let program = chain(UPDATE);
            let expected = execute(
                &contexts,
                &program,
                false,
                grant(Rights::ReadWrite, 2),
                policy(2),
            )
            .await
            .unwrap();
            assert_eq!(
                expected
                    .1
                    .value
                    .iter()
                    .map(|row| row.values().to_vec())
                    .collect::<Vec<_>>(),
                [values(&[1, 3, 11]), values(&[1, 3, 12])]
            );
            let hidden = execute(
                &contexts,
                &program,
                true,
                grant(Rights::ReadWrite, 2),
                policy(2),
            )
            .await
            .unwrap();
            assert_eq!(hidden, expected);

            // Find the actual shared signed-work boundary, then prove both
            // physically removed and hidden data have that same boundary.
            let (mut refused, mut admitted) = (0, grant(Rights::ReadWrite, 2).limits.max_work);
            while admitted - refused > 1 {
                let trial = refused + (admitted - refused) / 2;
                let mut capability = grant(Rights::ReadWrite, 2);
                capability.limits.max_work = trial;
                match execute(&contexts, &program, false, capability, policy(2)).await {
                    Ok(actual) => {
                        assert_eq!(actual, expected);
                        admitted = trial;
                    }
                    Err(error) => {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::LimitExceeded(LimitDimension::Work)),
                            "{error:?}"
                        );
                        refused = trial;
                    }
                }
            }
            assert!(admitted > 1);
            for hidden in [false, true] {
                for (allowance, succeeds) in [(admitted - 1, false), (admitted, true)] {
                    let mut capability = grant(Rights::ReadWrite, 2);
                    capability.limits.max_work = allowance;
                    let result = execute(&contexts, &program, hidden, capability, policy(2)).await;
                    if succeeds {
                        assert_eq!(result.unwrap(), expected);
                    } else {
                        assert_eq!(
                            authorization(&result.unwrap_err()),
                            Some(Error::LimitExceeded(LimitDimension::Work))
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn forbidden_mutations_and_unrematched_carried_reads_abort_every_write() {
        lab(0xc4a1_20f3, |contexts| async move {
            for tail in [
                "SET n.secret=99 RETURN n.p",
                "REMOVE n.secret RETURN n.p",
                "SET n.p=a.secret RETURN n.p",
                "SET n.p=a.p RETURN a.secret",
                "SET n.p=a.p RETURN a.secret LIMIT 0",
            ] {
                let program = chain(&format!(
                    "CREATE (a:Visible {{p:1}}) WITH a MATCH (n:Visible {{p:7}}) {tail}"
                ));
                for hidden in [false, true] {
                    let error = execute(
                        &contexts,
                        &program,
                        hidden,
                        grant(Rights::ReadWrite, 1),
                        policy(1),
                    )
                    .await
                    .unwrap_err();
                    assert_eq!(
                        authorization(&error),
                        Some(Error::ScopeDenied),
                        "hidden {hidden}, {tail}: {error:?}"
                    );
                }
            }
        });
    }

    #[test]
    fn late_match_arithmetic_and_inadequate_rights_publish_no_prefix() {
        lab(0xc4a1_20f4, |contexts| async move {
            for page in ["", " LIMIT 0"] {
                let program = chain(&format!(
                    "CREATE (audit:Visible {{p:100}}) WITH audit \
                     MATCH (n:Visible) WHERE n.p>=7 AND n.p<=8 \
                     SET n.p=100/(n.p-8) RETURN n.p{page}"
                ));
                for hidden in [false, true] {
                    let error = execute(
                        &contexts,
                        &program,
                        hidden,
                        grant(Rights::ReadWrite, 2),
                        policy(2),
                    )
                    .await
                    .unwrap_err();
                    assert!(
                        matches!(
                            error,
                            GqlQueryError::Source(GraphWriteChainError::Mutation(
                                fgdb_gql::GraphMutationQueryError::Mutation(
                                    fgdb_gql::GraphMutationError::Arithmetic {
                                        row: 1,
                                        action: 0,
                                        ..
                                    }
                                )
                            ))
                        ),
                        "hidden {hidden}, {page}: {error:?}"
                    );
                }
            }
            let program = chain(UPDATE);
            for rights in [Rights::Read, Rights::Write] {
                let error = execute(&contexts, &program, true, grant(rights, 2), policy(2))
                    .await
                    .unwrap_err();
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            }
        });
    }

    #[test]
    fn post_match_mutations_share_native_limits_and_only_final_rows_spend_delivery() {
        lab(0xc4a1_20f5, |contexts| async move {
            let program = chain(UPDATE);
            let expected = execute(
                &contexts,
                &program,
                true,
                grant(Rights::ReadWrite, 2),
                policy(2),
            )
            .await
            .unwrap();
            let measured = expected.0;
            assert!(measured.selection.snapshot_records > 0);
            assert!(measured.evaluator.work_units > 0);
            assert!(measured.evaluator.scratch_entries > 0);
            let exact = GraphInsertPolicy::new(
                GqlQueryPolicy::new(
                    measured.selection.snapshot_records,
                    2,
                    measured.evaluator.work_units,
                    measured.evaluator.scratch_entries,
                ),
                1,
                0,
            )
            .with_mutation_limit(3);
            assert_eq!(
                execute(
                    &contexts,
                    &program,
                    true,
                    grant(Rights::ReadWrite, 2),
                    exact
                )
                .await
                .unwrap(),
                expected
            );
            for dimension in 0..6 {
                let mut budget = exact;
                let mut capability = grant(Rights::ReadWrite, 2);
                match dimension {
                    0 => {
                        budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                            measured.selection.snapshot_records - 1,
                            2,
                        )
                    }
                    1 => budget.query.evaluator.max_work_units -= 1,
                    2 => budget.query.evaluator.max_scratch_entries -= 1,
                    3 => budget = budget.with_mutation_limit(2),
                    4 => {
                        budget.query.rows = fgdb_gql::GqlExecutionBudget::new(
                            measured.selection.snapshot_records,
                            1,
                        )
                    }
                    _ => capability.limits.max_rows = 1,
                }
                let error = execute(&contexts, &program, true, capability, budget)
                    .await
                    .expect_err("the post-write MATCH and SET share the whole statement allowance");
                if dimension == 5 {
                    assert_eq!(
                        authorization(&error),
                        Some(Error::LimitExceeded(LimitDimension::Rows))
                    );
                }
            }
        });
    }
}
