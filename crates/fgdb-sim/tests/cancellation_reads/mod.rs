use super::*;
use fgdb::{GqlError, WriteTxn, WriteTxnError};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GraphAggregateError, GraphAggregateRow, GraphSetExecutionError,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphSet, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use std::sync::Arc;

type QueryError<E> = GqlQueryError<E, Box<asupersync::error::Error>>;

#[derive(Debug, PartialEq, Eq)]
enum Rows {
    Values(Vec<GraphValueRow>),
    Aggregates(Vec<GraphAggregateRow>),
}

#[derive(Debug)]
enum Failure {
    PatternDb(QueryError<GqlError>),
    PatternTxn(QueryError<WriteTxnError>),
    AggregateDb(QueryError<GraphAggregateError<GqlError>>),
    AggregateTxn(QueryError<GraphAggregateError<WriteTxnError>>),
    SetDb(QueryError<GraphSetExecutionError<GqlError>>),
    SetTxn(QueryError<GraphSetExecutionError<WriteTxnError>>),
}

impl Failure {
    fn interrupted(&self) -> bool {
        match self {
            Self::PatternDb(GqlQueryError::Interrupted(_))
            | Self::PatternTxn(GqlQueryError::Interrupted(_))
            | Self::AggregateDb(GqlQueryError::Interrupted(_))
            | Self::AggregateTxn(GqlQueryError::Interrupted(_))
            | Self::SetDb(GqlQueryError::Interrupted(_))
            | Self::SetTxn(GqlQueryError::Interrupted(_))
            | Self::PatternTxn(GqlQueryError::Source(WriteTxnError::Interrupted(_)))
            | Self::AggregateTxn(GqlQueryError::Source(GraphAggregateError::Source(
                WriteTxnError::Interrupted(_),
            )))
            | Self::AggregateTxn(GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::Source(WriteTxnError::Interrupted(_)),
            )))
            | Self::SetTxn(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::Interrupted(_),
            ))) => true,
            _ => false,
        }
    }
}

// One prepared statement per test case, never stored in bulk.
#[allow(clippy::large_enum_variant)]
enum Prepared {
    Pattern(PreparedGraphPattern<GraphValueRow>),
    Aggregate(PreparedGraphAggregate),
    Set(PreparedGraphSet),
}

impl Prepared {
    fn execute(
        &self,
        db: &Database<MemVfs>,
        txn: Option<&WriteTxn>,
        cx: &QueryCx,
    ) -> Result<Rows, Failure> {
        match (self, txn) {
            (Self::Pattern(query), None) => db
                .execute_graph_pattern_governed(cx, query, policy())
                .map(|run| Rows::Values(run.value))
                .map_err(Failure::PatternDb),
            (Self::Pattern(query), Some(txn)) => txn
                .execute_graph_pattern_governed(db, cx, query, policy())
                .map(|run| Rows::Values(run.value))
                .map_err(Failure::PatternTxn),
            (Self::Aggregate(query), None) => db
                .execute_graph_aggregate_governed(cx, query, policy())
                .map(|run| Rows::Aggregates(run.value))
                .map_err(Failure::AggregateDb),
            (Self::Aggregate(query), Some(txn)) => txn
                .execute_graph_aggregate_governed(db, cx, query, policy())
                .map(|run| Rows::Aggregates(run.value))
                .map_err(Failure::AggregateTxn),
            (Self::Set(query), None) => db
                .execute_graph_set_governed(cx, query, policy())
                .map(|run| Rows::Values(run.value))
                .map_err(Failure::SetDb),
            (Self::Set(query), Some(txn)) => txn
                .execute_graph_set_governed(db, cx, query, policy())
                .map(|run| Rows::Values(run.value))
                .map_err(Failure::SetTxn),
        }
    }
}

pub(super) async fn run(seed: u64, contexts: &PurposeContexts) {
    let cx = contexts.query();
    let commit = contexts.commit();
    let txcx = contexts.txn();
    let arguments = GqlParameters::new();
    // Each family compiles native text once. Shortest paths remain native GLA
    // plans; the standalone endpoint kernel would miss transaction admission.
    let families = [
        (
            "pattern",
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:R]->(b) RETURN a.p AS p, a.q AS q, b.p AS peer ORDER BY p, q, peer",
            0,
        ),
        (
            "aggregate",
            "MATCH (a:Person) RETURN COUNT(*) AS count, SUM(a.q) AS total, AVG(a.p) AS mean",
            1,
        ),
        (
            "pipeline",
            "MATCH (a:Person) WITH a.p AS p, a.q AS q WHERE p > 0 RETURN p, q ORDER BY p, q",
            2,
        ),
        (
            "set",
            "MATCH (a:Person) RETURN a.p AS p, a.q AS q UNION DISTINCT MATCH (b:Person) WHERE b.p <= 3 RETURN b.p AS p, b.q AS q ORDER BY p, q",
            2,
        ),
        (
            "shortest",
            "MATCH walk = ALL SHORTEST WALK (a)-[:R*1..4]->(b) WHERE a.p = 1 RETURN path_length(walk) AS hops, a.q AS q, b.p AS peer ORDER BY hops, q, peer",
            0,
        ),
    ];
    for (family, source, kind) in families {
        let prepared = match kind {
            0 => Prepared::Pattern(
                PreparedGraphText::prepare(source, symbols)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    })
                    .bind_parameters(&arguments)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    }),
            ),
            1 => Prepared::Aggregate(
                PreparedGraphAggregateText::prepare(source, symbols)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    })
                    .bind_parameters(&arguments)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    }),
            ),
            _ => Prepared::Set(
                PreparedGraphSetText::prepare(source, symbols)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    })
                    .bind_parameters(&arguments)
                    .unwrap_or_else(|error| {
                        panic!("seed={seed} family={family} source={source}: {error:?}")
                    }),
            ),
        };
        let mut control = seeded(&commit, seed).await;
        let mut control_txn = control.begin(&txcx).expect("begin reference prefix");
        control_txn
            .write(&mut control, prefix())
            .expect("stage reference prefix");
        control_txn
            .commit(&mut control, &commit)
            .await
            .expect("commit reference prefix");
        assert_clean(contexts);
        let expected = prepared
            .execute(&control, None, &cx)
            .unwrap_or_else(|error| {
                panic!("seed={seed} family={family} source={source} control: {error:?}")
            });
        let expected_state = observed(&control);
        let expected_seq = control.frontier().expect("reference frontier");
        assert_clean(contexts);

        for overlay in [false, true] {
            let path = if overlay { "WriteTxn" } else { "Database" };
            // Counting has its own database and aborted transaction. Retained
            // read witnesses from that pass cannot alter any exact-k execution.
            let mut counting = seeded(&commit, seed).await;
            let count_before = observed(&counting);
            let count_seq = counting.frontier().expect("counting frontier");
            let mut count_txn = counting.begin(&txcx).expect("begin counting prefix");
            count_txn
                .write(&mut counting, prefix())
                .expect("stage counting prefix");
            let count_digest = count_txn.staged_effect_digest().expect("counting digest");
            let count_obligations = contexts.outstanding_obligations();
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let probe_cx = cx.with_checkpoint_probe(Arc::clone(&probe));
            let baseline = prepared
                .execute(&counting, overlay.then_some(&count_txn), &probe_cx)
                .unwrap_or_else(|error| panic!("seed={seed} family={family} source={source} path={path} counting: {error:?}"));
            let n = probe.calls();
            assert!(
                n >= 16,
                "seed={seed} family={family} source={source} path={path} N={n}: insufficient checkpoints"
            );
            assert_eq!(
                count_txn
                    .staged_effect_digest()
                    .expect("counting digest after read"),
                count_digest
            );
            assert_eq!(contexts.outstanding_obligations(), count_obligations);
            check_state(&counting, &count_before, count_seq);
            count_txn.abort();
            assert_clean(contexts);
            if overlay {
                assert_eq!(
                    baseline, expected,
                    "seed={seed} family={family} source={source} path={path}: overlay must see preserved prefix"
                );
            } else {
                assert_ne!(
                    baseline, expected,
                    "seed={seed} family={family} source={source} path={path}: committed read must not see staged prefix"
                );
            }
            let selected = stops(n, seed);
            let distinct = selected
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                selected.len(),
                distinct.len(),
                "seed={seed} family={family} source={source} path={path}: duplicate stops"
            );
            assert!(
                distinct.len() >= 16,
                "seed={seed} family={family} source={source} path={path}: insufficient distinct stops"
            );
            assert!(
                distinct.contains(&1)
                    && distinct.contains(&2)
                    && distinct.contains(&(n - 1))
                    && distinct.contains(&n)
            );
            for k in selected {
                let diagnostic =
                    format!("seed={seed} family={family} source={source} path={path} k={k}/{n}");
                let mut db = seeded(&commit, seed).await;
                let before = observed(&db);
                let seq = db.frontier().expect("sweep frontier");
                let mut txn = db.begin(&txcx).expect("begin preserved prefix");
                txn.write(&mut db, prefix())
                    .expect("stage preserved prefix");
                let digest = txn.staged_effect_digest().expect("prefix digest");
                // The open transaction legitimately owns its snapshot pin.
                let obligations = contexts.outstanding_obligations();
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(k)));
                let interrupted_cx = cx.with_checkpoint_probe(Arc::clone(&probe));
                let result = prepared.execute(&db, overlay.then_some(&txn), &interrupted_cx);
                assert!(
                    matches!(&result, Err(error) if error.interrupted()),
                    "{diagnostic}: expected typed Interrupted, got {result:?}"
                );
                assert_eq!(probe.calls(), k, "{diagnostic}: exact checkpoint ordinal");
                assert_eq!(
                    observed(&db),
                    before,
                    "{diagnostic}: published state changed"
                );
                assert_eq!(
                    db.frontier().expect("unchanged frontier"),
                    seq,
                    "{diagnostic}"
                );
                check_state(&db, &before, seq);
                assert_eq!(
                    txn.staged_effect_digest().expect("preserved digest"),
                    digest,
                    "{diagnostic}: prefix changed"
                );
                assert_eq!(
                    contexts.outstanding_obligations(),
                    obligations,
                    "{diagnostic}: leaked query obligation"
                );
                check_identity(&mut db, &cx, &before);
                let retry = prepared
                    .execute(&db, overlay.then_some(&txn), &cx)
                    .unwrap_or_else(|error| panic!("{diagnostic}: uninterrupted retry: {error:?}"));
                assert_eq!(retry, baseline, "{diagnostic}: query remained usable");
                assert_eq!(
                    txn.staged_effect_digest().expect("retry digest"),
                    digest,
                    "{diagnostic}: retry changed prefix"
                );
                txn.commit(&mut db, &commit).await.unwrap_or_else(|error| {
                    panic!("{diagnostic}: prefix commit failed: {error:?}")
                });
                assert_clean(contexts);
                assert_eq!(
                    observed(&db),
                    expected_state,
                    "{diagnostic}: prefix side effects lost"
                );
                assert_eq!(
                    db.frontier().expect("committed frontier"),
                    expected_seq,
                    "{diagnostic}"
                );
                let committed = prepared.execute(&db, None, &cx).unwrap_or_else(|error| {
                    panic!("{diagnostic}: committed retry failed: {error:?}")
                });
                assert_eq!(
                    committed, expected,
                    "{diagnostic}: never-interrupted committed control"
                );
                assert_clean(contexts);
            }
        }
    }
}
