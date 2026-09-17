//! Deterministic, structure-aware text fuzzing. Engine panics are never intercepted.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphDeleteText,
    PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsertText, PreparedGraphInsertText,
    PreparedGraphMutationText, PreparedGraphPipelineAggregateText, PreparedGraphSetText,
    PreparedGraphText, PreparedGraphVertexMergeText, PreparedGraphVertexUpsertText,
    PreparedGraphWriteScript, PreparedTemporalGraphAggregateText, PreparedTemporalGraphSetText,
    PreparedTemporalGraphText,
};
use std::time::{Duration, Instant};

// knob: ITERATIONS is per-campaign statements; each campaign loops
// SEEDS.len() seeds x ITERATIONS statements (currently 5 x 5_000 = 25_000),
// and the generated + mutated campaigns together execute 50_000 statements,
// every one through all 15 facades. Tuned to stay well under ~60s debug on
// one core; lower it for a quick local smoke.
const ITERATIONS: usize = 5_000;
const SEEDS: usize = 5;
// INSERT, literal, index, size, UNWIND, collect, list parameter, edge DELETE,
// ACYCLIC and SIMPLE. EXPLAIN is owned by fgdb and exercised there.
const FAMILY_COUNT: usize = 10;
const FAMILY_FLOOR: usize = 100;

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
        pub fn surface_statement(&mut self, family: usize) -> String {
            let value = self.rng.below(100);
            match family {
                0 => format!("INSERT (n:L {{p:{value}}})"),
                1 => format!("MATCH (n) RETURN [{value},NULL,[n.p]] AS xs"),
                2 => format!("MATCH (n) WITH [{value},n.p] AS xs RETURN xs[0] AS x"),
                3 => format!("MATCH (n) RETURN size([{value},n.p]) AS k"),
                4 => format!("MATCH (n) WITH [{value},n.p] AS xs UNWIND xs AS x RETURN x"),
                5 => "MATCH (n) RETURN collect(n.p) AS xs".into(),
                6 => "MATCH (n) UNWIND $xs AS x RETURN x".into(),
                7 => "MATCH (a)-[e:R]->(b) DELETE e".into(),
                8 => format!(
                    "MATCH ACYCLIC (a)-[:R*0..{}]->(b) RETURN a,b",
                    1 + value % 4
                ),
                9 => format!("MATCH SIMPLE (a)-[:R*0..{}]->(b) RETURN a,b", 1 + value % 4),
                _ => unreachable!("bounded family index"),
            }
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
            match self.rng.below(120) {
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
                73..=77 => {
                    // Restricted-walk surface: WALK/ACYCLIC/SIMPLE + quantified bounds.
                    let mode = self.rng.pick(&[
                        "WALK",
                        "ACYCLIC",
                        "SIMPLE",
                        "ALL SHORTEST",
                        "ANY SHORTEST",
                    ]);
                    let bounds = self
                        .rng
                        .pick(&["*1..3", "*0..4", "*2", "*", "*2..1", "*1025"]);
                    format!("MATCH {mode} (a)-[:{relation}{bounds}]->(b) RETURN a,b")
                }
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
                100..=119 => {
                    let family = self.rng.below(super::FAMILY_COUNT);
                    self.surface_statement(family)
                }
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

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn expect_typed<E: core::fmt::Debug>(err: &E) -> String {
    format!("{err:?}")
}

fn prepare_all(stmt: &str) {
    macro_rules! prepare {
        ($facade:ident $(, $relation:expr)?) => {{
            let started = Instant::now();
            if let Err(err) = $facade::prepare(stmt, $($relation,)? symbols) {
                let _ = expect_typed(&err);
                let _ = format!("{err}");
            }
            let elapsed = started.elapsed();
            assert!(elapsed <= Duration::from_secs(2), "{} exceeded prepare bound: {elapsed:?}; input={stmt:?}", stringify!($facade));
        }};
    }
    prepare!(PreparedGraphText);
    prepare!(PreparedGraphAggregateText);
    prepare!(PreparedGraphSetText);
    prepare!(PreparedGraphPipelineAggregateText);
    prepare!(PreparedTemporalGraphText);
    prepare!(PreparedTemporalGraphSetText);
    prepare!(PreparedTemporalGraphAggregateText);
    prepare!(PreparedGraphInsertText, RelationId(1));
    prepare!(PreparedGraphMutationText, RelationId(1));
    prepare!(PreparedGraphDeleteText, RelationId(1));
    prepare!(PreparedGraphEdgeMergeText, RelationId(1));
    prepare!(PreparedGraphVertexMergeText, RelationId(1));
    prepare!(PreparedGraphVertexUpsertText, RelationId(1));
    prepare!(PreparedGraphEdgeUpsertText, RelationId(1));
    prepare!(PreparedGraphWriteScript, RelationId(1));
}

#[test]
fn generated_statements_prepare_without_panic_across_all_facades() {
    let mut executed = 0usize;
    for seed in 0..SEEDS {
        let mut corpus = fuzz_gen::Corpus::new(0x5EED_0001 + seed as u64);
        for _ in 0..ITERATIONS {
            prepare_all(&corpus.unmutated_statement());
            executed += 1;
        }
    }
    assert!(
        executed >= 20_000,
        "generated campaign executed {executed} statements, below the 20k acceptance floor"
    );
}

#[test]
fn mutated_statements_prepare_without_panic_across_all_facades() {
    let mut executed = 0usize;
    for seed in 0..SEEDS {
        let mut corpus = fuzz_gen::Corpus::new(0x5EED_1001 + seed as u64);
        for _ in 0..ITERATIONS {
            prepare_all(&corpus.statement());
            executed += 1;
        }
    }
    assert!(
        executed >= 20_000,
        "mutated campaign executed {executed} statements, below the 20k acceptance floor"
    );
}
#[test]
fn deeply_nested_statements_refuse_with_typed_error_not_stack_overflow() {
    for stmt in [
        fuzz_gen::Corpus::deep_statement(256),
        format!(
            "MATCH (n){} RETURN n",
            " OPTIONAL MATCH (n)-[:R]->(m)".repeat(256)
        ),
        format!("MATCH (n){} RETURN n", " WITH n MATCH (n)".repeat(256)),
        "MATCH (n) RETURN n ".repeat(256),
    ] {
        // These exceed the grammar's bounded depth/definition contract, not a Rust stack budget.
        let result = PreparedGraphText::prepare(&stmt, symbols);
        assert!(result.is_err(), "deep input unexpectedly accepted: {stmt}");
        if let Err(err) = result {
            let _ = expect_typed(&err);
            let _ = format!("{err}");
        }
        prepare_all(&stmt);
    }
}

#[test]
fn grammatical_seeds_reach_every_preparation_facade() {
    macro_rules! accepts {
        ($facade:ident, $stmt:expr $(, $relation:expr)?) => {{
            let result = $facade::prepare($stmt, $($relation,)? symbols);
            if let Err(err) = result {
                panic!("{} rejected grammatical seed {:?}: {}", stringify!($facade), $stmt, expect_typed(&err));
            }
            prepare_all($stmt);
        }};
    }
    accepts!(PreparedGraphText, "MATCH (n) RETURN n");
    accepts!(
        PreparedGraphAggregateText,
        "MATCH (n) RETURN COUNT(*) AS total"
    );
    accepts!(
        PreparedGraphSetText,
        "MATCH (a) RETURN a AS x UNION ALL MATCH (b) RETURN b AS x"
    );
    accepts!(
        PreparedGraphPipelineAggregateText,
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS total"
    );
    accepts!(
        PreparedTemporalGraphText,
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n"
    );
    accepts!(
        PreparedTemporalGraphSetText,
        "MATCH (a) FOR SYSTEM_TIME AS OF SEQ 1 RETURN a AS x UNION ALL MATCH (b) RETURN b AS x"
    );
    accepts!(
        PreparedTemporalGraphAggregateText,
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total"
    );
    accepts!(PreparedGraphInsertText, "CREATE (n:L {p:1})", RelationId(1));
    accepts!(
        PreparedGraphMutationText,
        "MATCH (n) SET n.p=1",
        RelationId(1)
    );
    accepts!(PreparedGraphDeleteText, "MATCH (n) DELETE n", RelationId(1));
    accepts!(
        PreparedGraphEdgeMergeText,
        "MATCH (a),(b) MERGE (a)-[:R]->(b)",
        RelationId(1)
    );
    accepts!(
        PreparedGraphVertexMergeText,
        "MERGE (n:L {p:1})",
        RelationId(1)
    );
    accepts!(
        PreparedGraphVertexUpsertText,
        "MERGE (n:L {p:1}) ON MATCH SET n.q=1 ON CREATE SET n.q=2",
        RelationId(1)
    );
    accepts!(
        PreparedGraphEdgeUpsertText,
        "MATCH (a),(b) MERGE (a)-[e:R]->(b) ON MATCH SET e.p=1 ON CREATE SET e.q=2",
        RelationId(1)
    );
    accepts!(
        PreparedGraphWriteScript,
        "CREATE (n:L {p:1}); MATCH (n) SET n.q=2",
        RelationId(1)
    );
}

#[test]
fn each_new_surface_family_prepares_and_is_refused_by_the_facades() {
    let mut counts = [(0usize, 0usize); FAMILY_COUNT];
    let mut executed = 0usize;
    for seed in 0..SEEDS {
        let mut corpus = fuzz_gen::Corpus::new(0x5EED_2001 + seed as u64);
        for index in 0..ITERATIONS {
            let family = index % FAMILY_COUNT;
            let base = corpus.surface_statement(family);
            let stmt = match (index / FAMILY_COUNT) % 3 {
                0 => base,
                1 => format!("{base} @"),
                _ => corpus.mutate(&base),
            };
            let started = Instant::now();
            let outcome: Result<(), String> = match family {
                0 => PreparedGraphInsertText::prepare(&stmt, RelationId(1), symbols)
                    .map(|_| ())
                    .map_err(|err| format!("{err:?}")),
                5 => PreparedGraphAggregateText::prepare(&stmt, symbols)
                    .map(|_| ())
                    .map_err(|err| format!("{err:?}")),
                7 => PreparedGraphDeleteText::prepare(&stmt, RelationId(1), symbols)
                    .map(|_| ())
                    .map_err(|err| format!("{err:?}")),
                8 | 9 => PreparedGraphText::prepare(&stmt, symbols)
                    .map(|_| ())
                    .map_err(|err| format!("{err:?}")),
                _ => {
                    let types: &[(&str, fgdb_gql::GqlParameterType)] = if stmt.contains("$xs") {
                        &[("xs", fgdb_gql::GqlParameterType::List)]
                    } else {
                        &[]
                    };
                    PreparedGraphSetText::prepare_with_parameter_types(&stmt, types, symbols)
                        .map(|_| ())
                        .map_err(|err| format!("{err:?}"))
                }
            };
            match &outcome {
                Ok(()) => counts[family].0 += 1,
                Err(detail) => {
                    counts[family].1 += 1;
                    assert!(!detail.is_empty(), "refusal must carry a typed detail");
                    if (index / FAMILY_COUNT) % 3 == 0 {
                        panic!("family {family}: grammatical base refused: {stmt}: {detail}");
                    }
                }
            }
            assert!(
                started.elapsed() <= Duration::from_secs(2),
                "family {family}: {stmt}"
            );
            prepare_all(&stmt);
            executed += 1;
        }
    }
    assert!(executed >= 20_000, "executed {executed} below 20k floor");
    for (family, (prepared, refused)) in counts.iter().enumerate() {
        assert!(
            *prepared >= FAMILY_FLOOR,
            "family {family} prepared {prepared}; counts={counts:?}"
        );
        assert!(
            *refused >= FAMILY_FLOOR,
            "family {family} refused {refused}; counts={counts:?}"
        );
    }
    eprintln!("family counts (prepared, refused) = {counts:?}; executed = {executed}");
}
