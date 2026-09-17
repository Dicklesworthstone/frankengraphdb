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
    // The script facade overlaps the native ones; its entrypoint itself binds parameters.
    if let Some(script) = typed(PreparedGraphWriteScript::prepare(statement, R, symbols)) {
        let args = arguments(script.parameter_schema(), 0, 0);
        coverage[14] += 1;
        let _ = timed(statement, "script", || typed(txn.execute_graph_write_script_governed(
            db, cx, &script, &args, write_policy(), |request| allocate(next, request))));
    }
}

async fn sweep(corpus: impl IntoIterator<Item = String>, label: &str, variant_seed: impl Fn(usize) -> usize) {
    let mut coverage = [0_usize; 15];
    for (i, statement) in corpus.into_iter().enumerate() {
        let variant = variant_seed(i);
        let ((), report) = run_async_under_lab(0x46_55_5A_31 + i as u64, |root| async move {
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
        });
        assert!(report.lab_test_passed(), "{label} #{i}: {report:?}");
    }
    for (slot, count) in coverage.iter().enumerate() {
        assert!(*count > 0, "facade slot {slot} never executed; corpus needs a sample");
    }
}

#[test]
fn execute_random_statements_under_lab_never_panics() {
    let mut corpus = Vec::new();
    let mut rng = gen::Rng::new(0x46_55_5A_31);
    for _ in 0..ITERATIONS {
        let mut gen_rng = gen::Rng::new(0x46_55_5A_31 ^ rng.next_u64());
        corpus.push(gen::Corpus::new(gen_rng.next_u64()).statement());
    }
    sweep(corpus, "random", |i| i % 4);
}

#[test]
fn mutated_statements_prepared_and_executed_never_panic() {
    let mut corpus = Vec::new();
    let mut rng = gen::Rng::new(0x46_55_5A_32);
    for _ in 0..ITERATIONS {
        let base = gen::Corpus::new(rng.next_u64()).unmutated_statement();
        corpus.push(gen::Corpus::new(rng.next_u64()).mutate(&base));
    }
    sweep(corpus, "mutated", |i| (i + 1) % 4);
}

#[test]
fn deeply_nested_statements_refuse_with_typed_error_not_stack_overflow() {
    let mut corpus = Vec::new();
    for depth in [8_usize, 64, 256] {
        corpus.push(gen::deep_statement(depth));
    }
    let mut build = String::new();
    for i in 0..256 {
        build.push_str("MATCH (v");
        build.push_str(&i.to_string());
        build.push_str(") ");
    }
    build.push_str("RETURN v0");
    corpus.push(build);
    sweep(corpus, "deep", |_| 0);
}

mod gen {
    //! Private in-file generator copied from the shared fuzz corpus contract.
    //! Splitmix64; families cover MATCH chains, OPTIONAL MATCH, WHERE, WITH,
    //! aggregates + HAVING, set operations, SHORTEST WALK, INSERT/CREATE,
    //! SET/REMOVE, DELETE/DETACH DELETE, MERGE, and parameters. Mutation ops
    //! apply to about half of statements; deep nesting is bounded at build time.

    pub struct Rng { state: u64 }

    impl Rng {
        pub fn new(seed: u64) -> Self { Self { state: seed } }

        pub fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        pub fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n.max(1)
        }

        pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    pub struct Corpus { rng: Rng, depth: u32 }

    impl Corpus {
        pub fn new(seed: u64) -> Self { Self { rng: Rng::new(seed), depth: 0 } }

        pub fn unmutated_statement(&mut self) -> String { self.build() }

        pub fn statement(&mut self) -> String {
            let statement = self.build();
            if self.rng.below(2) == 0 { self.mutate(&statement) } else { statement }
        }

        pub fn mutate(&mut self, text: &str) -> String {
            let bytes: Vec<char> = text.chars().collect();
            if bytes.is_empty() { return "MATCH (a) RETURN a".to_string(); }
            match self.rng.below(5) {
                0 => { // token drop
                    let at = self.rng.below(bytes.len() as u64) as usize;
                    let mut out = String::new();
                    for (j, ch) in bytes.iter().enumerate() {
                        if j != at { out.push(*ch); }
                    }
                    out
                }
                1 => { // token duplicate
                    let at = self.rng.below(bytes.len() as u64) as usize;
                    let mut out = String::new();
                    for (j, ch) in bytes.iter().enumerate() {
                        out.push(*ch);
                        if j == at { out.push(*ch); }
                    }
                    out
                }
                2 => { // adjacent swap
                    let at = self.rng.below(bytes.len() as u64) as usize;
                    let mut out: String = bytes[..at].iter().collect();
                    if at + 1 < bytes.len() {
                        out.push(bytes[at + 1]);
                        out.push(bytes[at]);
                        out.extend(bytes[at + 2..].iter());
                    } else { out.push(bytes[at]); }
                    out
                }
                3 => { // byte flip on a printable ASCII char, valid UTF-8 boundary
                    let at = self.rng.below(bytes.len() as u64) as usize;
                    let mut out: String = bytes[..at].iter().collect();
                    let replacement = (33 + self.rng.below(90)) as u8 as char;
                    out.push(replacement);
                    out.extend(bytes[at + 1..].iter());
                    out
                }
                _ => { // truncation at a char boundary
                    let at = self.rng.below(bytes.len() as u64) as usize;
                    bytes[..at].iter().collect()
                }
            }
        }

        pub fn deep_statement(depth: usize) -> String {
            let mut out = String::new();
            for _ in 0..depth {
                out.push_str("MATCH (a)-[:R]->(b) ");
            }
            out.push_str("RETURN b");
            out
        }

        fn build(&mut self) -> String {
            self.depth = 0;
            self.statement(0)
        }

        fn statement(&mut self, level: u32) -> String {
            self.depth = level;
            let family = self.rng.below(100);
            match family {
                0..=19 => self.match_chain(),
                20..=29 => self.optional_match(),
                30..=44 => self.where_expression(),
                45..=54 => self.with_pipeline(),
                55..=64 => self.aggregate(),
                65..=72 => self.set_operation(),
                73..=77 => self.shortest_walk(),
                78..=85 => self.insert_or_create(),
                86..=90 => self.set_or_remove(),
                91..=94 => self.delete_or_detach(),
                _ => self.merge_statement(),
            }
        }

        fn pattern(&mut self, tag: u32) -> String {
            let label = if self.rng.below(3) == 0 { ":L" } else { "" };
            format!("({}{})", self.name(tag), label)
        }

        fn name(&mut self, tag: u32) -> String {
            format!("{}{tag}", ['a', 'b', 'c', 'v', 'n'][self.rng.below(5) as usize])
        }

        fn relation(&mut self) -> &'static str {
            if self.rng.below(2) == 0 { "R" } else { "S" }
        }

        fn match_chain(&mut self) -> String {
            let hops = 1 + self.rng.below(3) as u32;
            let mut out = String::from("MATCH ");
            out.push_str(&self.pattern(0));
            for tag in 1..=hops {
                let direction = self.rng.below(3);
                let rel = self.relation();
                if direction == 0 {
                    out.push_str(&format!("-[:{rel}]->"));
                } else if direction == 1 {
                    out.push_str(&format!("<-[:{rel}]-"));
                } else {
                    out.push_str(&format!("-[:{rel}]-"));
                }
                out.push_str(&self.pattern(tag));
            }
            out.push_str(&self.return_clause());
            out
        }

        fn optional_match(&mut self) -> String {
            format!("MATCH {} OPTIONAL MATCH {}-[:{}]->{} RETURN ALL {}",
                self.pattern(0), self.name(0), self.relation(), self.pattern(1), self.name(1))
        }

        fn predicate(&mut self) -> String {
            let v = self.name(9);
            let prop = if self.rng.below(2) == 0 { "p" } else { "q" };
            match self.rng.below(5) {
                0 => format!("{v}.{prop}={}", self.rng.below(5) as i64 - 2),
                1 => format!("{v}.{prop}<$p"),
                2 => format!("{v}.{prop} IS NOT NULL"),
                3 => format!("{v}.{prop}=$q"),
                _ => format!("{v}.{prop} IS NULL"),
            }
        }

        fn where_expression(&mut self) -> String {
            format!("MATCH {} WHERE {} RETURN {}",
                self.pattern(0), self.predicate(), self.name(0))
        }

        fn with_pipeline(&mut self) -> String {
            format!("MATCH {} WITH {}.p AS value WHERE value IS NOT NULL RETURN value",
                self.pattern(0), self.name(0))
        }

        fn aggregate(&mut self) -> String {
            let op = self.pick(&["COUNT(*)", "SUM(x)", "MIN(x)", "MAX(x)", "AVG(x)"]);
            format!("MATCH {} WITH {}.p AS x {} AS stat RETURN stat",
                self.pattern(0), self.name(0), op)
        }

        fn pick(&mut self, options: &[&str]) -> &str {
            options[self.rng.below(options.len() as u64) as usize]
        }

        fn set_operation(&mut self) -> String {
            let op = self.pick(&["UNION", "EXCEPT", "INTERSECT"]);
            let all = if self.rng.below(2) == 0 { "ALL" } else { "DISTINCT" };
            format!("MATCH ({}) RETURN ALL {}.p {op} {all} MATCH ({}) RETURN ALL {}.p",
                "a", "a", "b", "b")
        }

        fn shortest_walk(&mut self) -> String {
            format!("MATCH {} SHORTEST {} WALK (a)-[:R*1..3]->(b) RETURN b",
                self.pattern(0), if self.rng.below(2) == 0 { "2" } else { "1" })
        }

        fn insert_or_create(&mut self) -> String {
            if self.rng.below(2) == 0 {
                format!("INSERT ({}:Person {{p:1}})", self.name(0))
            } else {
                format!("CREATE ({} {{p:1,q:2}})", self.name(0))
            }
        }

        fn set_or_remove(&mut self) -> String {
            let v = self.name(0);
            if self.rng.below(2) == 0 {
                format!("MATCH {v} SET {v}.p=1,{v}.q=$q")
            } else {
                format!("MATCH {v} REMOVE {v}.p,{v}:L")
            }
        }

        fn delete_or_detach(&mut self) -> String {
            let v = self.name(0);
            if self.rng.below(2) == 0 {
                format!("MATCH {v} DELETE {v}")
            } else {
                format!("MATCH {v} DETACH DELETE {v}")
            }
        }

        fn merge_statement(&mut self) -> String {
            format!("MERGE ({} {{p:$p}})", self.name(0))
        }

        fn return_clause(&mut self) -> String {
            let target = self.name(0);
            if self.rng.below(3) == 0 {
                format!(" RETURN DISTINCT {target}")
            } else {
                format!(" RETURN ALL {target}")
            }
        }
    }
}
