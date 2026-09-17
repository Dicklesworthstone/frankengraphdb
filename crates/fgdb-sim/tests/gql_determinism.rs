//! Native GQL determinism suite (doctrine #4 / bet B5): the same database
//! state + same query + same policy ⇒ byte-identical results, always,
//! including result order.
//!
//! Relations, each over a canonical byte encoding of the full `QueryResult`
//! (tag-exact enum variants, value-exact cells, order-exact rows):
//! D1 repeated execution — N≥5 runs of every battery query on one snapshot.
//! D2 independent databases — two databases built from the identical write
//!     sequence (separate MemVfs; one reopened fast) agree per battery query
//!     at the frontier and on `FOR SYSTEM_TIME AS OF SEQ s` reads at every
//!     commit seq of their shared history, plus identical plan certificates.
//! D3 alternate batching — one big commit vs twelve small commits vs a
//!     shuffled single commit of the same units produce identical frontier
//!     results for every battery query, including the unordered scan.
//! D4 lab-seed independence — the battery under ≥3 lab runtime seeds.
//!
//! Order honesty: six battery queries carry ORDER BY over distinct keys (the
//! two-hop query exercises ties: two vertices share property values). The
//! seventh (`unordered-scan`) names no ORDER BY; the engine's plan compiler
//! still emits a total order over the scan (explicit keys, then the canonical
//! complete-row LexMin tie-break of plan §8.6), so its output is canonical
//! rather than arbitrary — asserted as equality here, and cited rather than
//! claimed as unspecified-behavior freedom.

use asupersync::fs::Vfs;
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

// Twelve independent unit updates describing one logical final graph. Every
// batching re-partitions these same units; fixed element ids make all
// batchings materialize byte-identical elements.
fn unit_0(b: &mut WriteBatch) {
    b.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(2))]);
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
    b.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
}
fn unit_9(b: &mut WriteBatch) {
    b.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(4)));
}
fn unit_10(b: &mut WriteBatch) {
    b.set_vertex_label(VId(4), PERSON, true);
    b.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(6)));
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
async fn apply(db: &mut Database<MemVfs>, cx: &CommitCx, groups: &[Vec<usize>]) -> Vec<CommitSeq> {
    let mut epochs = Vec::new();
    for group in groups {
        let mut batch = WriteBatch::new(R);
        for unit in group {
            UNIT_FNS[*unit](&mut batch);
        }
        epochs.push(db.write(cx, batch).await.expect("history commits"));
    }
    epochs
}

/// The battery: six ordered queries (one per read family, ordered by DISTINCT
/// keys) plus one unordered scan whose plan canonicalization is documented in
/// the module docs.
fn battery() -> Vec<(&'static str, String, GqlParameters)> {
    let mut params = GqlParameters::new();
    for (name, value) in [("min", 2), ("src", 1)] {
        params = params.with_int64(name, value).expect("scalar parameter");
    }
    vec![
        (
            "two-hop-ties",
            "MATCH (a)-[:R]->(b)-[:R]->(c) WHERE a.p >= $min RETURN ALL a.p AS ap, c.p AS cp ORDER BY ap, cp".to_owned(),
            params.clone(),
        ),
        (
            "aggregate",
            "MATCH (n) WHERE n.p >= $min RETURN COUNT(*) AS c, SUM(n.p) AS s, AVG(n.p) AS mean".to_owned(),
            params.clone(),
        ),
        (
            "grouped",
            "MATCH (n) RETURN ABS(n.p) AS bucket, COUNT(*) AS c GROUP BY ABS(n.p) ORDER BY bucket".to_owned(),
            params.clone(),
        ),
        (
            "pipeline",
            "MATCH (n) WHERE n.p >= $min WITH n.p AS x ORDER BY x DESC LIMIT 4 RETURN COUNT(*) AS c, SUM(x) AS s".to_owned(),
            params.clone(),
        ),
        (
            "union",
            "MATCH (a) WHERE a.p >= $min RETURN a.p AS p UNION DISTINCT MATCH (b) WHERE b.p <= 9 RETURN b.p AS p ORDER BY p".to_owned(),
            params.clone(),
        ),
        (
            "any-shortest",
            "MATCH p = ANY SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p = $src RETURN path_length(p) AS hops, b.p AS bp ORDER BY hops, bp".to_owned(),
            params.clone(),
        ),
        (
            "unordered-scan",
            "MATCH (n) RETURN n.p AS p".to_owned(),
            params.clone(),
        ),
    ]
}

/// Canonical byte encoding of a full `QueryResult`: tag-exact over the enum
/// variants, value-exact over every cell, order-exact over rows.
fn canonical(result: &QueryResult) -> Vec<u8> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("battery queries must return rows");
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

fn query_one<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
) -> Vec<u8> {
    let result = db.query(cx, text, params, symbols, policy());
    assert!(result.is_ok(), "query={text}: {result:?}");
    canonical(&result.expect("query success asserted"))
}

/// Run the whole battery at the frontier; returns canonical bytes per query.
fn run<V: Vfs + Clone>(db: &Database<V>, cx: &QueryCx) -> Vec<Vec<u8>> {
    battery()
        .iter()
        .map(|(name, text, params)| {
            let bytes = query_one(db, cx, text, params);
            assert!(bytes.len() > 40, "{name}: non-empty rows required");
            bytes
        })
        .collect()
}

/// Historical arm: canonical bytes of the ordered temporal scan at every
/// epoch. Shared only by databases with the identical commit numbering.
fn historical<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    epochs: &[CommitSeq],
) -> Vec<Vec<u8>> {
    let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p ORDER BY p";
    epochs
        .iter()
        .map(|epoch| {
            let params = GqlParameters::new()
                .with_uint64("at", epoch.0)
                .expect("sequence parameter");
            query_one(db, cx, text, &params)
        })
        .collect()
}

#[test]
fn seeded_histories_are_byte_identical_across_repeats_databases_batchings_and_seeds() {
    for seed in [0x0D20_u64, 0x0D21, 0x0D22] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();

            // Primary history: twelve small commits (rich epoch numbering).
            let vfs = MemVfs::new().expect("primary filesystem");
            let dir = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), dir.clone(), keys())
                .await
                .expect("create primary database");
            let singles: Vec<Vec<usize>> = (0..UNITS).map(|unit| vec![unit]).collect();
            let epochs = apply(&mut db, &commit, &singles).await;
            assert_eq!(epochs.len(), UNITS);

            // D1: repeated execution on one snapshot.
            let first = run(&db, &cx);
            for repeat in 1..REPEATS {
                assert_eq!(run(&db, &cx), first, "D1 repeat {repeat}");
            }

            // D2a: twin database, identical sequence, separate MemVfs.
            let twin_vfs = MemVfs::new().expect("twin filesystem");
            let twin_dir = twin_vfs.database_dir();
            let mut twin = Database::create_with_vfs(&commit, twin_vfs, twin_dir, keys())
                .await
                .expect("create twin database");
            apply(&mut twin, &commit, &singles).await;
            assert_eq!(run(&twin, &cx), first, "D2 twin frontier drift");

            // D2b: fast reopen of the primary after close.
            drop(db);
            let reopened = Database::<MemVfs>::open_with_vfs(&commit, vfs, dir, keys())
                .await
                .expect("fast reopen of the primary");
            assert_eq!(run(&reopened, &cx), first, "D2 reopen frontier drift");

            // D2 historical: identical commit numbering shares every seq.
            let primary_history = historical(&twin, &cx, &epochs);
            assert_eq!(
                historical(&reopened, &cx, &epochs),
                primary_history,
                "D2 reopen history drift"
            );

            // D3: alternate batchings of the same units.
            let mut big = Database::create_with_vfs(
                &commit,
                MemVfs::new().expect("big filesystem"),
                MemVfs::new().expect("big dir").database_dir(),
                keys(),
            )
            .await
            .expect("create big-batch database");
            let all: Vec<usize> = (0..UNITS).collect();
            let big_epochs = apply(&mut big, &commit, &[all.clone()]).await;
            assert_eq!(big_epochs.len(), 1, "batchings must differ in commit count");

            let mut shuffled = Database::create_with_vfs(
                &commit,
                MemVfs::new().expect("shuffled filesystem"),
                MemVfs::new().expect("shuffled dir").database_dir(),
                keys(),
            )
            .await
            .expect("create shuffled database");
            let mut order = all;
            let mut random = seed ^ 0x9E37_79B9_7F4A_7C15;
            for index in (1..order.len()).rev() {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let pick = ((random >> 33) % (index as u64 + 1)) as usize;
                order.swap(index, pick);
            }
            let shuffled_epochs = apply(&mut shuffled, &commit, &[order]).await;
            assert_eq!(shuffled_epochs.len(), 1);

            // D3 at the frontier: every battery query, including the
            // canonically ordered unordered scan, agrees across batchings.
            assert_eq!(run(&big, &cx), first, "D3 big-batch drift");
            // D4 is the enclosing loop: three lab seeds ran the full battery.
            drop(reopened);
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}

#[test]
fn certificates_bind_plans_across_independent_databases() {
    let ((), report) = run_async_under_lab(0x0D23, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let text = "MATCH (a)-[:R]->(b) WHERE a.p >= 2 RETURN b.p AS bp ORDER BY bp";
        let params = GqlParameters::new();
        let mut make = |label: &str| {
            let vfs = MemVfs::new().unwrap_or_else(|error| panic!("{label}: {error}"));
            let dir = vfs.database_dir();
            (vfs, dir)
        };
        let (vfs_a, dir_a) = make("database a");
        let (vfs_b, dir_b) = make("database b");
        let mut a = Database::create_with_vfs(&commit, vfs_a, dir_a, keys())
            .await
            .expect("database a");
        let mut b = Database::create_with_vfs(&commit, vfs_b, dir_b, keys())
            .await
            .expect("database b");
        let all: Vec<usize> = (0..UNITS).collect();
        apply(&mut a, &commit, &[all.clone()]).await;
        apply(&mut b, &commit, &[all]).await;
        let (_, cert_a) = a.explain(text, &params, symbols, true).expect("explain a");
        let (_, cert_b) = b.explain(text, &params, symbols, true).expect("explain b");
        assert_eq!(
            cert_a.expect("certificate a").digest(),
            cert_b.expect("certificate b").digest(),
            "identical plans must certify identically"
        );
    });
    assert!(report.lab_test_passed(), "report={report:?}");
}
