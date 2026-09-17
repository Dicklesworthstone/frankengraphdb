//! Deterministic structure-aware fuzzing over real storage and governed execution.
//! Every successful preparation is bound and executed; engine panics propagate.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterSpec, GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramPolicy, GraphWriteStatement,
    PreparedGqlQuery, PreparedGraphAggregateText, PreparedGraphDeleteText,
    PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsertText, PreparedGraphInsertText,
    PreparedGraphMutationText, PreparedGraphPipelineAggregateText, PreparedGraphSetText,
    PreparedGraphText, PreparedGraphVertexMergeText, PreparedGraphVertexUpsertText,
    PreparedGraphWriteProgram, PreparedGraphWriteScript, PreparedTemporalGraphText,
    PreparedTemporalGraphSetText, PreparedTemporalGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, TxnCx, VId, WriteTxn,
};
use std::time::{Duration, Instant};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
// knob: edit ITERATIONS to shrink debug runtime; each corpus also spans four graphs.
const ITERATIONS: usize = 512;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        _ => None,
    }
}

fn names() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_relation("S", S)
        .with_property("p", P).with_property("q", Q).with_label("L", L)
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}

fn expect_typed<E: core::fmt::Debug>(err: &E) -> String { format!("{err:?}") }

fn typed<T, E: core::fmt::Debug>(result: Result<T, E>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => { let _ = expect_typed(&err); None }
    }
}

fn arguments(schema: &[GqlParameterSpec], variant: usize, frontier: u64) -> GqlParameters {
    let mut args = GqlParameters::new();
    for spec in schema {
        args = match spec.parameter_type {
            GqlParameterType::Int64 => args.with_int64(&spec.name, [1, 0, -1, 5][variant % 4]),
            GqlParameterType::UInt64 => args.with_uint64(&spec.name,
                if spec.name == "at" { frontier } else { [1, 2, 5, 1][variant % 4] }),
            GqlParameterType::Scalar(_) => args.with_null(&spec.name),
        }.expect("generated parameter names originate in the admitted schema");
    }
    args
}

fn timed<T>(statement: &str, facade: &str, run: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = run();
    let elapsed = start.elapsed();
    assert!(elapsed <= Duration::from_secs(5), "{facade} took {elapsed:?}: {statement}");
    value
}

async fn seeded(commit: &CommitCx, variant: usize) -> Database<MemVfs> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut left = WriteBatch::new(R);
    for vertex in 1..=5_u128 {
        left.create_vertex(VId(vertex), vec![], vec![(P, CanonicalScalar::Int(vertex as i64))]);
    }
    left.add_edge(EId(11), VId(1), VId(2), vec![]);
    left.add_edge(EId(12), VId(1), VId(2), vec![]);
    left.add_edge(EId(13), VId(4), VId(5), vec![]);
    db.write(commit, left).await.unwrap();
    let mut right = WriteBatch::new(S);
    right.add_edge(EId(21), VId(2), VId(3), vec![]);
    right.add_edge(EId(22), VId(2), VId(3), vec![]);
    db.write(commit, right).await.unwrap();
    if variant != 0 {
        let mut rng = gen::Rng::new(0x67_72_61_70_68 + variant as u64);
        for relation in [R, S] {
            let mut batch = WriteBatch::new(relation);
            if relation == R {
                for id in 6..=10_u128 {
                    batch.create_vertex(VId(id), if rng.below(2) == 0 { vec![L] } else { vec![] },
                        vec![(P, CanonicalScalar::Int(rng.below(11) as i64 - 5)),
                             (Q, CanonicalScalar::Int(rng.below(5) as i64))]);
                }
            }
            for edge in 0..8_u128 {
                batch.add_edge(EId(100 + u128::from(relation.0) * 10 + edge),
                    VId(1 + rng.below(10) as u128), VId(1 + rng.below(10) as u128),
                    vec![(P, CanonicalScalar::Int(rng.below(5) as i64))]);
            }
            db.write(commit, batch).await.unwrap();
        }
    }
    db
}

fn execute_reads(db: &Database<MemVfs>, cx: &QueryCx, statement: &str, variant: usize,
    coverage: &mut [usize; 15]) {
    let frontier = db.frontier().unwrap().0;
    macro_rules! read_facade {
        ($slot:expr, $facade:ty, $execute:ident) => {
            if let Some(template) = typed(<$facade>::prepare(statement, symbols)) {
                let args = arguments(template.parameter_schema(), variant, frontier);
                if let Some(query) = typed(template.bind_parameters(&args)) {
                    coverage[$slot] += 1;
                    let _ = timed(statement, stringify!($facade), ||
                        typed(db.$execute(cx, &query, policy())));
                }
            }
        };
    }
    read_facade!(0, PreparedGraphText, execute_graph_pattern_governed);
    read_facade!(1, PreparedGraphAggregateText, execute_graph_aggregate_governed);
    read_facade!(2, PreparedGraphSetText, execute_graph_set_governed);
    read_facade!(3, PreparedGraphPipelineAggregateText, execute_graph_aggregate_governed);
    read_facade!(4, PreparedTemporalGraphText, execute_temporal_graph_text_governed);
    read_facade!(5, PreparedTemporalGraphSetText, execute_temporal_graph_set_text_governed);
    read_facade!(6, PreparedTemporalGraphAggregateText, execute_temporal_graph_aggregate_text_governed);
    // Keep the original numeric facade in the sweep, but never substitute it for text facades.
    if let Some(query) = typed(PreparedGqlQuery::prepare(statement, &names())) {
        let _ = timed(statement, "PreparedGqlQuery", ||
            typed(db.execute_prepared_query_governed(cx, &query, policy())));
    }
}
