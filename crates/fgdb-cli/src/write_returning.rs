//! Shared preparation and staged execution for native CREATE, mutation,
//! vertex MERGE and relationship MERGE/ON/SET RETURN writes, used by ordinary
//! writes and ordered transactions.
//! A RETURN-less write, including a bounded native UNWIND, uses the native
//! write program path.

use super::{
    Failure, Options, emit, execution_failure, human_value, policy, render_row_body, value_cell,
};
use asupersync::fs::Vfs;
use fgdb::{Database, WriteTxn};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertPolicy,
    GraphMutationPolicy, GraphVertexMergePolicy, GraphVertexUpsertPolicy,
    PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText, PreparedGraphEdgeUpsertQuery,
    PreparedGraphEdgeUpsertQueryText, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
    PreparedGraphMutationQuery, PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts, QueryCx};
use std::io::{self, Write};

const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
// The sequence and row count are the only values added after completion. Their
// bounded integer encodings plus the fixed terminal frame fit this reservation.
const TERMINAL_RESERVATION: usize = 256;

/// One write statement whose RETURN rows are produced with its effects.
pub(super) enum Returning {
    Insert(Box<PreparedGraphInsertQuery>),
    Mutation(Box<PreparedGraphMutationQuery>),
    Merge(Box<PreparedGraphVertexUpsertQuery>),
    EdgeMerge(Box<PreparedGraphEdgeMergeQuery>),
    EdgeUpsert(Box<PreparedGraphEdgeUpsertQuery>),
}
impl Returning {
    pub(super) fn columns(&self) -> &[String] {
        match self {
            Self::Insert(query) => query.columns(),
            Self::Mutation(query) => query.columns(),
            Self::Merge(query) => query.columns(),
            Self::EdgeMerge(query) => query.columns(),
            Self::EdgeUpsert(query) => query.columns(),
        }
    }

    /// Freeze the native statement's rows while staging its complete effects.
    /// The caller owns output admission and the sole completion boundary.
    pub(super) fn execute_in_transaction<V: Vfs + Clone>(
        &self,
        transaction: &mut WriteTxn,
        database: &mut Database<V>,
        cx: &QueryCx,
        allowance: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, Failure> {
        Ok(match self {
            Self::Insert(query) => {
                transaction
                    .execute_graph_insert_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphInsertPolicy::new(allowance, 100_000, 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Self::Mutation(query) => {
                transaction
                    .execute_graph_mutation_query_governed(
                        database,
                        cx,
                        query,
                        GraphMutationPolicy::new(allowance, 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Self::Merge(query) => {
                transaction
                    .execute_graph_vertex_upsert_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(allowance), 1_000),
                    )
                    .map_err(execution_failure)?
                    .2
            }
            Self::EdgeMerge(query) => {
                transaction
                    .execute_graph_edge_merge_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphEdgeMergePolicy::new(allowance).with_creation_limit(100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Self::EdgeUpsert(query) => {
                transaction
                    .execute_graph_edge_upserts_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphEdgeUpsertPolicy::new(
                            GraphEdgeMergePolicy::new(allowance).with_creation_limit(100_000),
                            1_000,
                        ),
                    )
                    .map_err(execution_failure)?
                    .1
            }
        })
    }
}

pub(super) fn prepare(options: &Options) -> Result<Option<Returning>, Failure> {
    prepare_statement(&options.text, &options.params, options)
}

/// Native token classifiers choose the write family before binding. Ordered
/// transaction steps supply their own text and parameters, with one catalog.
pub(super) fn prepare_statement(
    statement: &str,
    params: &GqlParameters,
    options: &Options,
) -> Result<Option<Returning>, Failure> {
    let declarations: Vec<_> = params.parameter_types().collect();
    if PreparedGraphInsertQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            options,
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Insert(Box::new(query))))
            .map_err(Failure::query);
    }
    if PreparedGraphMutationQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            options,
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Mutation(Box::new(query))))
            .map_err(Failure::query);
    }
    if PreparedGraphVertexUpsertQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            options,
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Merge(Box::new(query))))
            .map_err(Failure::query);
    }
    // ON/SET uses the occurrence-preserving bulk executor. The classifier still
    // excludes plain MERGE: do not retry its failures through a different
    // mutation family, or lose the original statement's diagnostics.
    if PreparedGraphEdgeUpsertQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphEdgeUpsertQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            options,
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::EdgeUpsert(Box::new(query))))
            .map_err(Failure::query);
    }
    if PreparedGraphEdgeMergeQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphEdgeMergeQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            options,
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::EdgeMerge(Box::new(query))))
            .map_err(Failure::query);
    }
    Ok(None)
}

struct BufferedRows {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BufferedRows {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.limit)
            .ok_or_else(|| io::Error::other("write RETURN encoded output exceeds 16 MiB"))?;
        self.bytes.extend_from_slice(bytes);
        debug_assert_eq!(self.bytes.len(), end);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn run<V: Vfs + Clone>(
    database: &mut Database<V>,
    contexts: &PurposeContexts,
    query: Returning,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let cx = contexts.query();
    let mut transaction = database.begin(&contexts.txn()).map_err(execution_failure)?;
    let prepared = (|| {
        let result = query.execute_in_transaction(&mut transaction, database, &cx, policy())?;
        let mut rendered = Vec::with_capacity(result.value.len());
        let mut encoded_cells = 0usize;
        for row in result.value {
            cx.checkpoint().map_err(Failure::query)?;
            let mut cells = Vec::with_capacity(row.values().len());
            for value in row.values() {
                let cell = if robot {
                    value_cell(value)?
                } else {
                    human_value(value)?
                };
                encoded_cells = encoded_cells
                    .checked_add(cell.len())
                    .filter(|bytes| *bytes <= MAX_OUTPUT_BYTES - TERMINAL_RESERVATION)
                    .ok_or_else(|| Failure::query("write RETURN encoded output exceeds 16 MiB"))?;
                cells.push(cell);
            }
            rendered.push(cells);
        }
        let count = rendered.len();
        let mut buffer = BufferedRows {
            bytes: Vec::new(),
            limit: MAX_OUTPUT_BYTES - TERMINAL_RESERVATION,
        };
        render_row_body(query.columns(), &rendered, robot, &mut buffer)
            .map_err(|error| Failure::query(error.message))?;
        cx.checkpoint().map_err(Failure::query)?;
        Ok((buffer.bytes, count))
    })();
    let (bytes, count) = match prepared {
        Ok(output) => output,
        Err(error) => {
            transaction.abort();
            return Err(error);
        }
    };
    let completion = match transaction.finish(database, &contexts.commit()).await {
        Ok(completion) => completion,
        Err(error) => {
            return Err(match transaction.state() {
                EmbeddedTxnState::CommitOutcomeUnknown { .. } => Failure::io(format!(
                    "transaction outcome unknown; reopen and resolve before retrying: {error}"
                )),
                EmbeddedTxnState::CommittedNeedsRecovery { commit_seq } => Failure::io(format!(
                    "transaction committed at seq {}; recovery required: {error}",
                    commit_seq.0
                )),
                _ => execution_failure(error),
            });
        }
    };
    let seq = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0,
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0,
    };
    // Only terminal transport can fail from here. No query, conversion, quota,
    // authorization, or cancellation check can discard a committed result.
    out.write_all(&bytes).map_err(Failure::io)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"count":{count},"statements":1}}"#
            ),
        )
    } else {
        emit(out, &format!("{count} row(s), completed at seq {seq}"))
    }
}

#[cfg(test)]
mod edge_upsert_tests {
    use super::*;

    fn options() -> Options {
        crate::parse(
            &[
                "--db",
                "unused",
                "--key-file",
                "unused",
                "--label",
                "Person=1",
                "--relation",
                "R=1",
                "--property",
                "p=1",
                "--property",
                "q=2",
                "CREATE (n)",
            ]
            .map(str::to_owned),
            "write",
        )
        .unwrap()
    }

    #[test]
    fn relationship_actions_return_use_the_native_upsert_family() {
        let options = options();
        for statement in [
            "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1 RETURN r,r.p AS p",
            "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)-[r:R {p:7}]-(b) \
             SET r.q=8 RETURN r.q AS q",
            "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)<-[r:R]-(b) \
             ON CREATE SET r.p=1 RETURN count(*) AS total",
        ] {
            let prepared = prepare_statement(statement, &GqlParameters::new(), &options).unwrap();
            assert!(
                matches!(prepared, Some(Returning::EdgeUpsert(_))),
                "{statement}",
            );
        }
    }

    #[test]
    fn plain_relationship_merge_keeps_its_multi_row_return_owner() {
        let options = options();
        let prepared = prepare_statement(
            "MATCH (a:Person),(b:Person) MERGE (a)-[r:R]->(b) RETURN r",
            &GqlParameters::new(),
            &options,
        )
        .unwrap();
        assert!(matches!(prepared, Some(Returning::EdgeMerge(_))));
        let prepared = prepare_statement(
            "MERGE (n:Person {p:1}) ON CREATE SET n.q=2 RETURN n.q AS q",
            &GqlParameters::new(),
            &options,
        )
        .unwrap();
        assert!(matches!(prepared, Some(Returning::Merge(_))));
    }

    #[test]
    fn return_words_in_literals_and_comments_do_not_create_a_returning_write() {
        let options = options();
        for statement in [
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) SET r.p='RETURN r'",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p=1 /* RETURN r */",
        ] {
            assert!(
                prepare_statement(statement, &GqlParameters::new(), &options)
                    .unwrap()
                    .is_none(),
                "{statement}",
            );
        }
    }

    #[test]
    fn upsert_return_binding_errors_never_fall_back_to_a_nonreturning_program() {
        let options = options();
        let statement = "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
            MERGE (a)-[r:R]->(b) ON CREATE SET r.q=$value RETURN r.q AS q";
        assert!(prepare_statement(statement, &GqlParameters::new(), &options).is_err());
        let mut parameters = GqlParameters::new();
        parameters
            .insert("value", fgdb_gql::GqlParameterValue::Int64(9))
            .unwrap();
        let prepared = prepare_statement(statement, &parameters, &options)
            .unwrap()
            .unwrap();
        assert!(matches!(&prepared, Returning::EdgeUpsert(_)));
        assert_eq!(prepared.columns(), ["q"]);
        assert!(
            prepare_statement(
                "MATCH (a),(b) MERGE (a)-[r:R]->(b) ON CREATE SET r.q=1 RETURN a",
                &GqlParameters::new(),
                &options,
            )
            .is_err(),
        );
    }

    #[test]
    fn bulk_dispatch_keeps_all_occurrences_and_rolls_back_late_result_failure() {
        use asupersync::lab::run_async_under_lab;
        use fgdb::{DatabaseKeys, WriteBatch};
        use fgdb_delta_types::{PropertyKeyId, RelationId};
        use fgdb_gql::algebra::GraphValue;
        use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, VId};

        let ((), report) = run_async_under_lab(0xe6e1_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let keys = DatabaseKeys::new(
                [0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=3 {
                seed.create_vertex(
                    VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let options = options();
            let query = prepare_statement(
                "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
                 ON CREATE SET r.q=7 RETURN count(r) AS total,sum(r.q) AS sum",
                &GqlParameters::new(), &options,
            ).unwrap().unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let result = query.execute_in_transaction(
                &mut transaction, &mut db, &cx, GqlQueryPolicy::new(1_000, 1, 5_000_000, 2_000_000),
            ).unwrap();
            assert_eq!(query.columns(), ["total", "sum"]);
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.value[0].values(), &[
                GraphValue::Scalar(CanonicalScalar::Int(2)),
                GraphValue::Scalar(CanonicalScalar::Int(14)),
            ]);
            assert_eq!(transaction.edges(&db).unwrap().len(), 2);
            assert!(db.edges().unwrap().is_empty());
            let digest = transaction.staged_effect_digest().unwrap();
            let failing = prepare_statement(
                "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
                 ON MATCH SET r.q=0 RETURN 10/r.q AS bad LIMIT 0",
                &GqlParameters::new(), &options,
            ).unwrap().unwrap();
            assert!(failing.execute_in_transaction(
                &mut transaction, &mut db, &cx, GqlQueryPolicy::new(1_000, 0, 5_000_000, 2_000_000),
            ).is_err());
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
            transaction.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.edges().unwrap().len(), 2);
            assert!(db.edges().unwrap().iter().all(|edge| {
                edge.props == vec![(PropertyKeyId(2), CanonicalScalar::Int(7))]
            }));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
