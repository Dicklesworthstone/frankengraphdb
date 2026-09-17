//! Deterministic structure-aware fuzzing over real storage and governed execution.
//! Every successful preparation is bound and executed; engine panics propagate.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch, WriteTxn};
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
    QueryCx, VId,
};
use std::time::{Duration, Instant};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
// knob: corpus inputs per test (plus 6 WITNESS_SEEDS). 512 keeps debug runtime
// well under ~60s on one core while binding every facade slot; lower it for a
// quick local smoke. Each corpus also spans four seeded graph variants.
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
        let mut rng = fuzz_gen::Rng::new(0x67_72_61_70_68 + variant as u64);
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

fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(20_000, 20_000, 2_000_000, 2_000_000),
        128, 128, 128)
}

fn allocate(next: &mut u128, request: fgdb_gql::GraphWriteIdentityRequest)
    -> Result<ElementId, ()>
{
    let id = *next;
    *next = next.checked_add(1).ok_or(())?;
    Ok(match request.request {
        fgdb_gql::insertion::GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
        fgdb_gql::insertion::GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
    })
}

fn execute_bound<T: Into<GraphWriteStatement>>(txn: &mut WriteTxn,
    db: &mut Database<MemVfs>, cx: &QueryCx, bound: T, next: &mut u128, statement: &str)
{
    let program = typed(PreparedGraphWriteProgram::prepare(vec![bound.into()]))
        .expect("one bound statement is a valid program");
    let _ = timed(statement, "write program", || typed(txn.execute_graph_write_program_governed(
        db, cx, &program, write_policy(), |request| allocate(next, request))));
}

fn execute_writes(txn: &mut WriteTxn, db: &mut Database<MemVfs>, cx: &QueryCx,
    statement: &str, next: &mut u128, coverage: &mut [usize; 15]) {
    macro_rules! write_facade {
        ($slot:expr, $facade:ty) => {
            if let Some(prepared) = typed(<$facade>::prepare(statement, R, symbols)) {
                let args = arguments(prepared.parameter_schema(), 0, 0);
                if let Some(bound) = typed(prepared.bind_parameters(&args)) {
                    coverage[$slot] += 1;
                    execute_bound(txn, db, cx, bound, next, statement);
                }
            }
        };
    }
    write_facade!(7, PreparedGraphInsertText);
    write_facade!(8, PreparedGraphMutationText);
    write_facade!(9, PreparedGraphDeleteText);
    write_facade!(10, PreparedGraphEdgeMergeText);
    write_facade!(11, PreparedGraphVertexMergeText);
    write_facade!(12, PreparedGraphVertexUpsertText);
    write_facade!(13, PreparedGraphEdgeUpsertText);
    // The script facade overlaps the native ones; its entrypoint binds parameters.
    if let Some(script) = typed(PreparedGraphWriteScript::prepare(statement, R, symbols)) {
        let args = arguments(script.parameter_schema(), 0, 0);
        coverage[14] += 1;
        let _ = timed(statement, "script", || typed(txn.execute_graph_write_script_governed(
            db, cx, &script, &args, write_policy(), |request| allocate(next, request))));
    }
}

/// Deterministic statements that bind through rarely-drawn facade slots.
const WITNESS_SEEDS: [&str; 8] = [
    "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(n.p) AS total HAVING total > 0",
    "MATCH (a),(b) MERGE (a)-[:R]->(b)",
    "CREATE (n:L {p:1})",
    "MATCH (n) SET n.p = 1",
    "MATCH (n) DELETE n",
    "MERGE (n:L {p:1})",
    "MERGE (n:L {p:1}) ON MATCH SET n.q=1 ON CREATE SET n.q=2",
    "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON MATCH SET e.p=1 ON CREATE SET e.q=2",
];

fn sweep(corpus: impl IntoIterator<Item = String>, label: &str,
    variant_seed: impl Fn(usize) -> usize, require_coverage: bool) {
    let mut totals = [0_usize; 15];
    for (i, statement) in corpus.into_iter().enumerate() {
        let variant = variant_seed(i);
        let (executed, report) = run_async_under_lab(0x46_55_5A_31 + i as u64, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let txn_cx = contexts.txn();
            let mut db = seeded(&commit, variant).await;
            let mut coverage = [0_usize; 15];
            execute_reads(&db, &query, &statement, variant, &mut coverage);
            let mut next = 10_000_u128;
            let mut txn = db.begin(&txn_cx).unwrap();
            execute_writes(&mut txn, &mut db, &query, &statement, &mut next, &mut coverage);
            txn.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            coverage
        });
        assert!(report.lab_test_passed(), "{label} #{i}: {report:?}");
        for slot in 0..15 {
            totals[slot] += executed[slot];
        }
    }
    if require_coverage {
        for (slot, count) in totals.iter().enumerate() {
            assert!(*count > 0, "{label}: facade slot {slot} never executed; corpus needs a sample");
        }
    }
}

#[test]
fn execute_random_statements_under_lab_never_panics() {
    let mut corpus = Vec::new();
    let mut rng = fuzz_gen::Rng::new(0x46_55_5A_31);
    for _ in 0..ITERATIONS {
        let mut rng_gen = fuzz_gen::Rng::new(0x46_55_5A_31 ^ rng.next_u64());
        corpus.push(fuzz_gen::Corpus::new(rng_gen.next_u64()).statement());
    }
    corpus.extend(WITNESS_SEEDS.iter().map(|s| (*s).to_string()));
    sweep(corpus, "random", |i| i % 4, true);
}

#[test]
fn mutated_statements_prepared_and_executed_never_panic() {
    let mut corpus = Vec::new();
    let mut rng = fuzz_gen::Rng::new(0x46_55_5A_32);
    for _ in 0..ITERATIONS {
        let base = fuzz_gen::Corpus::new(rng.next_u64()).unmutated_statement();
        corpus.push(fuzz_gen::Corpus::new(rng.next_u64()).mutate(&base));
    }
    corpus.extend(WITNESS_SEEDS.iter().map(|s| (*s).to_string()));
    sweep(corpus, "mutated", |i| (i + 1) % 4, true);
}

#[test]
fn deeply_nested_statements_refuse_with_typed_error_not_stack_overflow() {
    let mut corpus = Vec::new();
    for depth in [8_usize, 64, 256] {
        corpus.push(fuzz_gen::Corpus::deep_statement(depth));
    }
    let mut build = String::new();
    for i in 0..256 {
        build.push_str("MATCH (v");
        build.push_str(&i.to_string());
        build.push_str(") ");
    }
    build.push_str("RETURN v0");
    corpus.push(build);
    sweep(corpus, "deep", |_| 0, false);
}

mod fuzz_gen {
    pub struct Rng {
        state: u64,
    }
    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self { state: seed }
        }
        pub fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = self.state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^ (value >> 31)
        }
        pub fn below(&mut self, n: usize) -> usize {
            assert!(n > 0);
            (self.next_u64() % n as u64) as usize
        }
        pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    pub struct Corpus {
        rng: Rng,
        depth: u32,
    }
    impl Corpus {
        pub fn new(seed: u64) -> Self {
            Self {
                rng: Rng::new(seed),
                depth: 0,
            }
        }
        pub fn statement(&mut self) -> String {
            let base = self.unmutated_statement();
            if self.rng.below(2) == 0 {
                self.mutate(&base)
            } else {
                base
            }
        }
        pub fn deep_statement(depth: usize) -> String {
            let depth = depth.min(256);
            format!(
                "MATCH (n) WHERE {}n.p = 1{} RETURN n",
                "(".repeat(depth),
                ")".repeat(depth)
            )
        }
        pub fn unmutated_statement(&mut self) -> String {
            if self.rng.below(100) < 2 {
                self.depth = (64 + self.rng.below(193)) as u32;
                return match self.rng.below(3) {
                    0 => Self::deep_statement(self.depth as usize),
                    1 => format!(
                        "MATCH (n){} RETURN n",
                        " OPTIONAL MATCH (n)-[:R]->(m)".repeat(self.depth as usize)
                    ),
                    _ => format!(
                        "MATCH (n){} RETURN n",
                        " WITH n MATCH (n)".repeat(self.depth as usize)
                    ),
                };
            }
            let relation = *self.rng.pick(&["R", "R", "S", "missing"]);
            let property = *self.rng.pick(&["p", "q"]);
            let value = *self.rng.pick(&[
                "0",
                "1",
                "-1",
                "9223372036854775807",
                "$name",
                "$value",
                "NULL",
                "'λ'",
            ]);
            let label = if self.rng.below(1024) == 0 {
                "zzplantedzz"
            } else {
                "L"
            };
            let temporal = if self.rng.below(5) == 0 {
                " FOR SYSTEM_TIME AS OF SEQ 1"
            } else {
                ""
            };
            match self.rng.below(100) {
                0..=19 => {
                    let mut text = format!("MATCH (a:{label})");
                    for index in 0..self.rng.below(4) {
                        text.push_str(&format!("-[:{relation}]->(v{index})"));
                    }
                    text.push_str(&format!(
                        "{temporal} RETURN ALL a LIMIT {}",
                        self.rng.pick(&["3", "$limit"])
                    ));
                    text
                }
                20..=29 => format!("MATCH (a) OPTIONAL MATCH (a)-[:{relation}]->(b) RETURN a,b"),
                30..=44 => {
                    let op = self.rng.pick(&["=", "<>", "<", ">=", "+", "AND", "OR"]);
                    format!(
                        "MATCH (n){temporal} WHERE (n.{property} {op} {value}) AND NOT (n.q IS NULL) RETURN n"
                    )
                }
                45..=54 => {
                    if self.rng.below(2) == 0 {
                        format!(
                            "MATCH (n) WITH n.{property} AS x RETURN SUM(x) AS total HAVING total > 0"
                        )
                    } else {
                        format!("MATCH (n) WITH n AS x WHERE x.{property} = {value} RETURN x")
                    }
                }
                55..=64 => {
                    let aggregate = self.rng.pick(&["COUNT", "SUM", "MIN", "MAX", "AVG"]);
                    format!(
                        "MATCH (n){temporal} RETURN {aggregate}(n.{property}) AS total HAVING total > 0"
                    )
                }
                65..=72 => {
                    let op = self.rng.pick(&["UNION", "EXCEPT", "INTERSECT"]);
                    let quantifier = self.rng.pick(&["ALL", "DISTINCT"]);
                    format!(
                        "MATCH (a){temporal} RETURN a AS x {op} {quantifier} MATCH (b) RETURN b AS x"
                    )
                }
                73..=77 => format!("MATCH SHORTEST WALK (a)-[:{relation}*1..3]->(b) RETURN a,b"),
                78..=85 => {
                    let verb = self.rng.pick(&["INSERT", "CREATE"]);
                    let mut text = format!("{verb} (n:{label} {{p:{value}}})");
                    if self.rng.below(3) == 0 {
                        text.push_str("; MATCH (n) SET n.q = 2");
                    }
                    text
                }
                86..=90 => {
                    if self.rng.below(2) == 0 {
                        format!("MATCH (n) SET n.{property} = {value}")
                    } else {
                        format!("MATCH (n) REMOVE n.{property}")
                    }
                }
                91..=94 => format!("MATCH (n) {}DELETE n", self.rng.pick(&["", "DETACH "])),
                _ => match self.rng.below(4) {
                    0 => format!("MERGE (n:{label} {{p:{value}}})"),
                    1 => format!(
                        "MERGE (n:{label} {{p:{value}}}) ON MATCH SET n.q=1 ON CREATE SET n.q=2"
                    ),
                    2 => "MATCH (a),(b) MERGE (a)-[e:R]->(b)".into(),
                    _ => format!(
                        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON MATCH SET e.p={value} ON CREATE SET e.q=2"
                    ),
                },
            }
        }
        pub fn mutate(&mut self, input: &str) -> String {
            // Split punctuation from identifiers, preserving every UTF-8 boundary.
            let mut ranges = Vec::new();
            let mut start = None;
            for (at, ch) in input.char_indices() {
                if ch.is_alphanumeric() || ch == '_' {
                    start.get_or_insert(at);
                } else {
                    if let Some(begin) = start.take() {
                        ranges.push((begin, at));
                    }
                    if !ch.is_whitespace() {
                        ranges.push((at, at + ch.len_utf8()));
                    }
                }
            }
            if let Some(begin) = start {
                ranges.push((begin, input.len()));
            }
            if ranges.is_empty() {
                return input.to_owned();
            }
            let token = self.rng.below(ranges.len());
            let (start, end) = ranges[token];
            let mut text = input.to_owned();
            match self.rng.below(5) {
                0 => {
                    text.replace_range(start..end, "");
                }
                1 => {
                    text.insert_str(end, &format!(" {}", &input[start..end]));
                }
                2 if token + 1 < ranges.len() => {
                    let (next_start, next_end) = ranges[token + 1];
                    text.replace_range(
                        start..next_end,
                        &format!(
                            "{}{}{}",
                            &input[next_start..next_end],
                            &input[end..next_start],
                            &input[start..end]
                        ),
                    );
                }
                2 => {
                    text.insert_str(start, "(");
                }
                3 => {
                    let boundaries: Vec<_> = input.char_indices().map(|(at, _)| at).collect();
                    let at = boundaries[self.rng.below(boundaries.len())];
                    let end = at + input[at..].chars().next().unwrap().len_utf8();
                    let replacement = char::from(32 + self.rng.below(95) as u8).to_string();
                    text.replace_range(at..end, &replacement);
                }
                _ => {
                    text.truncate(start);
                }
            }
            text
        }
    }
}
