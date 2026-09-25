//! Native GQL determinism: exact value-tagged result bytes, including row order.
//! Four generated histories each run under three lab seeds; D4 compares their
//! complete transcripts across seeds, rather than merely rerunning assertions.
//! D1 repeats the frontier battery five times. D2 compares every query and its
//! EXPLAIN certificate at every commit, then compares historical reads after
//! fast reopen. D3 compares single-commit and causally shuffled batchings.
//!
//! Unordered projection is canonical here, not unspecified: fgdb-gql's
//! algebra_exec/projection.rs ProjectedRows::into_rows returns one sorted
//! stream; algebra/values.rs GraphValue/GraphValueRow define canonical order.
//! We compare both full bytes and multisets for the unordered scan. Explicit
//! ORDER BY p leaves ties resolved by the engine, not by sorting test output.
//! No claim about parallel query execution, float reductions, or certificate
//! replay. Early epochs may have empty path results; frontier witnesses must
//! be nonempty and include distinct rows tied on p and a NULL ordering key.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const PERSON: LabelId = LabelId(1);
const REPEATS: usize = 5;
const UNITS: usize = 12;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x6d; 32],
        DatabaseSecurityNamespaceId([0x2a; 32]),
        [0x51; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}

// Causally layered units: vertex creates, edge creates, then independent
// updates/deletion. Only units within one layer may be shuffled.
fn unit_0(b: &mut WriteBatch) {
    b.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(1))]);
}
fn unit_1(b: &mut WriteBatch) {
    b.create_vertex(VId(2), vec![PERSON], vec![(P, CanonicalScalar::Int(2))]);
}
fn unit_2(b: &mut WriteBatch) {
    b.create_vertex(
        VId(3),
        vec![],
        vec![(P, CanonicalScalar::Int(3)), (Q, CanonicalScalar::Int(7))],
    );
}
fn unit_3(b: &mut WriteBatch) {
    b.create_vertex(VId(4), vec![], vec![]);
}
fn unit_4(b: &mut WriteBatch) {
    b.add_edge(EId(10), VId(1), VId(2), vec![]);
}
fn unit_5(b: &mut WriteBatch) {
    b.add_edge(EId(11), VId(1), VId(3), vec![(Q, CanonicalScalar::Int(5))]);
}
fn unit_6(b: &mut WriteBatch) {
    b.add_edge(EId(12), VId(2), VId(3), vec![]);
}
fn unit_7(b: &mut WriteBatch) {
    b.add_edge(EId(13), VId(3), VId(4), vec![]);
}
fn unit_8(b: &mut WriteBatch) {
    b.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
}
fn unit_9(b: &mut WriteBatch) {
    b.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(4)));
}
fn unit_10(b: &mut WriteBatch) {
    b.set_vertex_label(VId(4), PERSON, true);
    b.set_vertex_property(VId(4), Q, Some(CanonicalScalar::Int(6)));
}
fn unit_11(b: &mut WriteBatch) {
    b.delete_edge(EId(12));
}

const UNIT_FNS: [fn(&mut WriteBatch); UNITS] = [
    unit_0, unit_1, unit_2, unit_3, unit_4, unit_5, unit_6, unit_7, unit_8, unit_9, unit_10,
    unit_11,
];

/// Apply `groups` of unit indices as one commit per group; returns the
/// per-commit sequence numbers.
async fn apply(
    db: &mut Database<MemVfs>,
    cx: &CommitCx,
    groups: &[Vec<usize>],
    graph_seed: u64,
) -> Vec<CommitSeq> {
    let mut epochs = Vec::new();
    for group in groups {
        let mut batch = WriteBatch::new(R);
        for unit in group {
            UNIT_FNS[*unit](&mut batch);
            if *unit == 9 {
                let value = graph_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                batch.set_vertex_property(
                    VId(2),
                    Q,
                    Some(CanonicalScalar::Int((value >> 33) as i64)),
                );
            }
        }
        epochs.push(db.write(cx, batch).await.expect("history commits"));
    }
    epochs
}

/// Exercise every live read classification and each temporal counterpart.
fn battery(at: Option<CommitSeq>) -> Vec<(&'static str, String, GqlParameters)> {
    let temporal = at.map_or_else(String::new, |_| " FOR SYSTEM_TIME AS OF SEQ $at".into());
    let mut queries = Vec::new();
    let mut push = |name, text: String| {
        let mut params = GqlParameters::new();
        if text.contains("$min") {
            params = params.with_int64("min", 2).expect("minimum parameter");
        }
        if let Some(seq) = at {
            params = params.with_uint64("at", seq.0).expect("sequence parameter");
        }
        queries.push((name, text, params));
    };
    push(
        "ordered-ties",
        format!("MATCH (n){temporal} RETURN n, n.p AS p, n.q AS q ORDER BY p"),
    );
    push(
        "unordered-scan",
        format!("MATCH (n){temporal} RETURN n, n.p AS p, n.q AS q"),
    );
    push(
        "two-hop",
        format!(
            "MATCH (a)-[:R]->(b)-[:R]->(c){temporal} WHERE a.p >= $min RETURN ALL a.p AS ap, c.p AS cp ORDER BY ap, cp"
        ),
    );
    push(
        "optional",
        format!("MATCH (a){temporal} OPTIONAL MATCH (a)-[:R]->(b) RETURN a, b.p AS p ORDER BY p"),
    );
    push(
        "aggregate",
        format!("MATCH (n){temporal} RETURN COUNT(*) AS c, SUM(n.p) AS s, AVG(n.p) AS mean"),
    );
    push(
        "grouped",
        format!("MATCH (n){temporal} RETURN n.p AS p, COUNT(*) AS c GROUP BY n.p ORDER BY p"),
    );
    push(
        "pipeline",
        format!(
            "MATCH (n){temporal} WHERE n.p >= $min WITH n.p AS x ORDER BY x DESC LIMIT 4 RETURN COUNT(*) AS c, SUM(x) AS s"
        ),
    );
    push(
        "union",
        format!(
            "MATCH (a){temporal} RETURN a.p AS p UNION DISTINCT MATCH (b) RETURN b.p AS p ORDER BY p"
        ),
    );
    push(
        "except-all",
        format!(
            "MATCH (a){temporal} RETURN a.p AS p EXCEPT ALL MATCH (b) WHERE b.p > 9 RETURN b.p AS p ORDER BY p"
        ),
    );
    push(
        "any-shortest",
        format!(
            "MATCH p = ANY SHORTEST WALK (a)-[:R*1..3]->(b){temporal} WHERE a.p = 2 RETURN path_length(p) AS hops, b.p AS bp ORDER BY hops, bp"
        ),
    );
    push(
        "all-shortest",
        format!(
            "MATCH p = ALL SHORTEST WALK (a)-[:R*1..3]->(b){temporal} WHERE a.p = 2 RETURN path_length(p) AS hops, b.p AS bp ORDER BY hops, bp"
        ),
    );
    push(
        "acyclic",
        format!("MATCH p = ACYCLIC (a)-[:R*1..3]->(b){temporal} RETURN b.p AS bp ORDER BY bp"),
    );
    push(
        "simple",
        format!("MATCH p = SIMPLE (a)-[:R*1..3]->(b){temporal} RETURN b.p AS bp ORDER BY bp"),
    );
    queries
}

/// Canonical byte encoding of a full `QueryResult`: tag-exact over the enum
/// variants, value-exact over every cell, order-exact over rows.
fn canonical(result: &QueryResult) -> Vec<u8> {
    assert!(
        matches!(result, QueryResult::Rows { .. }),
        "read result required"
    );
    let QueryResult::Rows { columns, rows } = result else {
        unreachable!()
    };
    let mut bytes = b"fgdb:query-result:v1\0".to_vec();
    bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
    for column in columns {
        bytes.extend_from_slice(&(column.len() as u64).to_be_bytes());
        bytes.extend_from_slice(column.as_bytes());
    }
    bytes.extend_from_slice(&(rows.len() as u64).to_be_bytes());
    for row in rows {
        bytes.extend_from_slice(&(row.len() as u64).to_be_bytes());
        for cell in row {
            use fgdb_gql::GraphAggregateValue as Cell;
            let (tag, payload): (u8, Vec<u8>) = match cell {
                Cell::Count(value) => (1, value.to_be_bytes().to_vec()),
                Cell::Integer(value) => (2, value.to_be_bytes().to_vec()),
                Cell::Value(value) => (
                    3,
                    value
                        .canonical_bytes()
                        .expect("battery values encode canonically"),
                ),
                Cell::Average(value) => {
                    let mut exact = Vec::new();
                    exact.extend_from_slice(&value.numerator().to_be_bytes());
                    exact.extend_from_slice(&value.denominator().to_be_bytes());
                    (4, exact)
                }
            };
            bytes.push(tag);
            bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&payload);
        }
    }
    bytes
}

fn run(db: &Database<MemVfs>, cx: &QueryCx, at: Option<CommitSeq>, frontier: bool) -> Vec<Vec<u8>> {
    battery(at).into_iter().map(|(name, text, params)| {
        let result = db.query(cx, &text, &params, symbols, policy());
        assert!(result.is_ok(), "{name} at={at:?} query={text}: {result:?}");
        let result = result.expect("query success asserted");
        let QueryResult::Rows { rows, .. } = &result else { unreachable!("read battery") };
        if frontier {
            assert!(!rows.is_empty(), "{name}: frontier witness required");
            if name == "ordered-ties" {
                assert!(rows.iter().any(|row| matches!(&row[1], fgdb_gql::GraphAggregateValue::Value(value) if value.is_null())), "NULL sort key required");
                assert!(rows.iter().enumerate().any(|(i, row)| rows[i + 1..].iter().any(|other| row[1] == other[1] && row[0] != other[0])), "distinct rows tied on p required");
            }
        }
        canonical(&result)
    }).collect()
}

fn certificates(
    db: &Database<MemVfs>,
    at: Option<CommitSeq>,
) -> Vec<fgdb::NativeExplainCertificate> {
    battery(at)
        .into_iter()
        .map(|(name, text, params)| {
            let (_, certificate) = db.explain(&text, &params, symbols, true).expect(name);
            certificate.expect("requested certificate")
        })
        .collect()
}

fn assert_unordered_bag(db: &Database<MemVfs>, other: &Database<MemVfs>, cx: &QueryCx) {
    let text = "MATCH (n) RETURN n, n.p AS p, n.q AS q";
    let bag = |db: &Database<MemVfs>| {
        let result = db
            .query(cx, text, &GqlParameters::new(), symbols, policy())
            .expect("unordered scan");
        let QueryResult::Rows { columns, rows } = result else {
            unreachable!()
        };
        let mut encoded: Vec<_> = rows
            .into_iter()
            .map(|row| {
                canonical(&QueryResult::Rows {
                    columns: columns.clone(),
                    rows: vec![row],
                })
            })
            .collect();
        encoded.sort();
        encoded
    };
    assert_eq!(bag(db), bag(other), "D3 unordered multiset drift");
}

#[test]
fn seeded_histories_are_byte_identical_across_repeats_databases_batchings_and_seeds() {
    let mut graph_outputs = Vec::new();
    for graph_seed in [0xD20_u64, 0xD21, 0xD22, 0xD23] {
        let mut scheduling_baseline = None;
        for lab_seed in [0xA01, 0xA02, 0xA03] {
            let (transcript, report) = run_async_under_lab(lab_seed, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let vfs = MemVfs::new().expect("primary filesystem");
                let dir = vfs.database_dir();
                let mut db = Database::create_with_vfs(&commit, vfs.clone(), dir.clone(), keys())
                    .await
                    .expect("primary");
                let twin_vfs = MemVfs::new().expect("twin filesystem");
                let twin_dir = twin_vfs.database_dir();
                let mut twin = Database::create_with_vfs(&commit, twin_vfs, twin_dir, keys())
                    .await
                    .expect("twin");
                let mut history = Vec::new();
                let mut epochs = Vec::new();
                let mut plan_history = Vec::new();
                for unit in 0..UNITS {
                    let groups = [vec![unit]];
                    let seq = apply(&mut db, &commit, &groups, graph_seed).await[0];
                    assert_eq!(
                        apply(&mut twin, &commit, &groups, graph_seed).await,
                        vec![seq]
                    );
                    let rows = run(&db, &cx, None, unit + 1 == UNITS);
                    assert_eq!(
                        run(&twin, &cx, None, unit + 1 == UNITS),
                        rows,
                        "D2 graph={graph_seed} seq={seq:?}"
                    );
                    let plans = certificates(&db, None);
                    assert_eq!(
                        certificates(&twin, None),
                        plans,
                        "D2 certificates at {seq:?}"
                    );
                    history.push(rows);
                    plan_history.push(plans);
                    epochs.push(seq);
                }
                let first = history.last().expect("committed history");
                for repeat in 1..REPEATS {
                    assert_eq!(&run(&db, &cx, None, true), first, "D1 repeat {repeat}");
                }
                drop(db);
                let reopened = Database::<MemVfs>::open_with_vfs(&commit, vfs, dir, keys())
                    .await
                    .expect("fast reopen");
                assert_eq!(&run(&reopened, &cx, None, true), first, "D2 reopen");
                for (index, seq) in epochs.iter().enumerate() {
                    assert_eq!(
                        run(&reopened, &cx, Some(*seq), false),
                        history[index],
                        "D2 historical seq={seq:?}"
                    );
                    assert_eq!(
                        run(&twin, &cx, Some(*seq), false),
                        history[index],
                        "D2 twin historical seq={seq:?}"
                    );
                    assert_eq!(
                        certificates(&reopened, Some(*seq)),
                        certificates(&twin, Some(*seq)),
                        "D2 historical certificates {seq:?}"
                    );
                }
                let big_vfs = MemVfs::new().expect("big filesystem");
                let big_dir = big_vfs.database_dir();
                let mut big = Database::create_with_vfs(&commit, big_vfs, big_dir, keys())
                    .await
                    .expect("big");
                let all: Vec<_> = (0..UNITS).collect();
                apply(&mut big, &commit, std::slice::from_ref(&all), graph_seed).await;
                assert!(
                    big.frontier().expect("big frontier")
                        < reopened.frontier().expect("small frontier"),
                    "batchings differ in commit count"
                );
                assert_eq!(&run(&big, &cx, None, true), first, "D3 big-batch drift");
                assert_unordered_bag(&big, &reopened, &cx);
                let shuffled_vfs = MemVfs::new().expect("shuffled filesystem");
                let shuffled_dir = shuffled_vfs.database_dir();
                let mut shuffled =
                    Database::create_with_vfs(&commit, shuffled_vfs, shuffled_dir, keys())
                        .await
                        .expect("shuffled");
                let mut order = all;
                let mut random = graph_seed;
                for range in [0..4, 4..8, 8..UNITS] {
                    for i in (range.start + 1..range.end).rev() {
                        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let j = range.start + ((random >> 33) as usize % (i - range.start + 1));
                        order.swap(i, j);
                    }
                }
                assert_ne!(
                    order,
                    (0..UNITS).collect::<Vec<_>>(),
                    "shuffle must change insertion order"
                );
                apply(&mut shuffled, &commit, &[order], graph_seed).await;
                assert_eq!(&run(&shuffled, &cx, None, true), first, "D3 shuffled drift");
                assert_unordered_bag(&shuffled, &reopened, &cx);
                (history, plan_history)
            });
            assert!(
                report.lab_test_passed(),
                "graph={graph_seed} lab={lab_seed} report={report:?}"
            );
            if let Some(baseline) = &scheduling_baseline {
                assert_eq!(
                    &transcript, baseline,
                    "D4 graph={graph_seed} lab={lab_seed}"
                );
            } else {
                scheduling_baseline = Some(transcript);
            }
        }
        graph_outputs.push(scheduling_baseline.expect("three scheduling runs").0);
    }
    for i in 0..graph_outputs.len() {
        for j in i + 1..graph_outputs.len() {
            assert_ne!(
                graph_outputs[i], graph_outputs[j],
                "graph seeds must vary observable history"
            );
        }
    }
}
