//! Metamorphic layout invariance over the native GQL read surface (plan §15
//! "Storage oracles"). The same seeded multi-commit history must answer every
//! native query family with byte-identical rows and order regardless of the
//! physical layout or open path:
//! R1 live vs `Database::compact` (and the compacted generation is what a
//!     fast reopen lands on: retained root + manifest),
//! R2 fast open vs the forced stream fold `open_rebuilding` (UnixVfs face),
//! R3 `FOR SYSTEM_TIME AS OF SEQ s` on the final history vs live queries on a
//!     history truncated at s, for several earlier s,
//! R4 close + reopen at every relation above.
//! Anti-vacuity: compaction must change the partition root identity, every
//! battery query must be non-empty at the baseline, and at least one query
//! must differ between two AS OF sequences.

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
const PERSON: LabelId = LabelId(1);
const TAG: LabelId = LabelId(2);
const ARMS: usize = 9;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}

/// Nine deterministic commit arms: creates, parallel edges, property updates,
/// label flips, edge and vertex deletes, and a bulk edge commit sized to span
/// several tier-D adjacency blocks. `omit` drops exactly one arm (relation
/// negative); `stop_after` truncates the history (R3).
fn arms(seed: u64, stop_after: usize, omit: Option<usize>) -> Vec<WriteBatch> {
    let mut random = seed;
    let mut pick = |modulo: u128| {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((random >> 33) % modulo as u64) as u128
    };
    let mut batches = Vec::new();
    let mut b = WriteBatch::new(R);
    for id in 1..=6 {
        b.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(id as i64))],
        );
    }
    for (eid, src, dst) in [
        (10, 1, 2),
        (11, 1, 2),
        (12, 2, 3),
        (13, 3, 4),
        (14, 4, 1),
        (15, 5, 6),
    ] {
        b.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    batches.push(b);

    let mut b = WriteBatch::new(R);
    for (eid, src, dst) in [(16, 2, 5), (17, 5, 2), (18, 6, 3)] {
        b.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    let src = 1 + pick(4);
    let mut dst = 1 + pick(4);
    if src == dst {
        dst = dst % 4 + 1;
    }
    b.add_edge(EId(40), VId(src), VId(dst), vec![]);
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
    b.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(20)));
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.set_vertex_label(VId(3), TAG, true);
    b.set_vertex_label(VId(1), PERSON, false);
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.delete_edge(EId(11));
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.delete_edge(EId(15));
    b.delete_edge(EId(18));
    b.delete_vertex(VId(6));
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.create_vertex(VId(7), vec![PERSON], vec![(P, CanonicalScalar::Int(7))]);
    b.create_vertex(VId(8), vec![], vec![(P, CanonicalScalar::Int(8))]);
    b.add_edge(EId(20), VId(7), VId(8), vec![]);
    b.add_edge(EId(21), VId(8), VId(2), vec![]);
    let alive = [1_u128, 2, 3, 4, 5, 7, 8];
    for i in 0..30_u128 {
        let src = alive[pick(7) as usize];
        let mut dst = alive[pick(7) as usize];
        if src == dst {
            dst = alive[(dst as usize + 1) % alive.len()];
        }
        b.add_edge(EId(100 + i), VId(src), VId(dst), vec![]);
    }
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(30)));
    b.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(40)));
    b.set_vertex_label(VId(4), TAG, true);
    batches.push(b);

    let mut b = WriteBatch::new(R);
    b.delete_edge(EId(13));
    b.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
    b.set_vertex_label(VId(2), TAG, true);
    batches.push(b);

    batches
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index < stop_after && Some(*index) != omit)
        .map(|(_, batch)| batch)
        .collect()
}

async fn build<V: Vfs + Clone>(
    db: &mut Database<V>,
    cx: &CommitCx,
    seed: u64,
    stop_after: usize,
    omit: Option<usize>,
) -> Vec<CommitSeq> {
    let mut epochs = Vec::new();
    for batch in arms(seed, stop_after, omit) {
        epochs.push(db.write(cx, batch).await.expect("history commits"));
    }
    assert_eq!(
        epochs.len(),
        ARMS.min(stop_after) - usize::from(omit.is_some_and(|index| index < stop_after))
    );
    epochs
}

/// Fixed battery across every native read family `Database::query` dispatches:
/// pattern (multi-hop and OPTIONAL scopes), plain and grouped aggregates, a
/// WITH pipeline, set operations, ANY/ALL SHORTEST WALK with path functions,
/// and restricted ACYCLIC/SIMPLE paths — each with parameters and ORDER BY.
fn battery(at: Option<CommitSeq>) -> Vec<(&'static str, String, GqlParameters)> {
    let temporal = at.map_or_else(String::new, |_| " FOR SYSTEM_TIME AS OF SEQ $at".into());
    let mut out = Vec::new();
    let mut push = |name: &'static str, text: String| {
        let mut params = GqlParameters::new();
        for (name, value) in [("min", 5), ("max", 6), ("src", 5)] {
            if text.contains(&format!("${name}")) {
                params = params.with_int64(name, value).expect("scalar parameter");
            }
        }
        if let Some(seq) = at {
            params = params.with_uint64("at", seq.0).expect("sequence parameter");
        }
        out.push((name, text, params));
    };
    push(
        "two-hop",
        format!(
            "MATCH (a)-[:R]->(b)-[:R]->(c){temporal} WHERE a.p >= $min RETURN ALL a.p AS ap, c.p AS cp ORDER BY ap, cp"
        ),
    );
    push(
        "optional",
        format!(
            "MATCH (a){temporal} WHERE a.p >= $min OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN a.p AS ap, b.p AS bp, c.p AS cp ORDER BY ap, bp, cp"
        ),
    );
    push(
        "aggregate",
        format!(
            "MATCH (n){temporal} WHERE n.p >= $min RETURN COUNT(*) AS c, SUM(n.p) AS s, AVG(n.p) AS mean"
        ),
    );
    push(
        "grouped",
        format!(
            "MATCH (n){temporal} WHERE n.p >= $min RETURN ABS(n.p) AS bucket, COUNT(*) AS c GROUP BY ABS(n.p) ORDER BY bucket"
        ),
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
            "MATCH (a){temporal} WHERE a.p >= $min RETURN a.p AS p UNION DISTINCT MATCH (b) WHERE b.p <= $max RETURN b.p AS p ORDER BY p"
        ),
    );
    push(
        "except-all",
        format!(
            "MATCH (a){temporal} WHERE a.p >= $min RETURN a.p AS p EXCEPT ALL MATCH (b) WHERE b.p <= $max RETURN b.p AS p ORDER BY p"
        ),
    );
    push(
        "any-shortest",
        format!(
            "MATCH p = ANY SHORTEST WALK (a)-[:R*1..3]->(b){temporal} WHERE a.p = $src RETURN path_length(p) AS hops, b.p AS bp ORDER BY hops, bp"
        ),
    );
    push(
        "all-shortest",
        format!(
            "MATCH p = ALL SHORTEST WALK (a)-[:R*1..3]->(b){temporal} WHERE a.p = $src RETURN path_length(p) AS hops, b.p AS bp ORDER BY hops, bp"
        ),
    );
    push(
        "acyclic",
        format!(
            "MATCH p = ACYCLIC (a)-[:R*1..3]->(b){temporal} WHERE a.p >= $min RETURN b.p AS bp ORDER BY bp"
        ),
    );
    push(
        "simple",
        format!(
            "MATCH p = SIMPLE (a)-[:R*1..3]->(b){temporal} WHERE a.p >= $min RETURN b.p AS bp ORDER BY bp"
        ),
    );
    out
}

fn run<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    at: Option<CommitSeq>,
) -> Vec<(&'static str, QueryResult)> {
    battery(at)
        .into_iter()
        .map(|(name, text, params)| {
            let result = db
                .query(cx, &text, &params, symbols, policy())
                .unwrap_or_else(|error| panic!("{name} at={at:?} query={text}: {error:?}"));
            let QueryResult::Rows { rows, .. } = &result else {
                panic!("{name} must return rows");
            };
            let _ = rows;
            (name, result)
        })
        .collect()
}

fn assert_same_rows(
    stage: &str,
    baseline: &[(&'static str, QueryResult)],
    actual: &[(&'static str, QueryResult)],
) {
    assert_eq!(baseline.len(), actual.len(), "{stage}: battery cardinality");
    for ((want_name, want), (got_name, got)) in baseline.iter().zip(actual.iter()) {
        assert_eq!(want_name, got_name, "{stage}: battery drifted");
        assert_eq!(want, got, "{stage}: {got_name}");
    }
}
#[test]
fn seeded_history_answers_identically_across_layouts_and_open_paths() {
    for seed in [0x0A50_u64, 0x0A51, 0x0A52, 0x0A53] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();

            // MemVfs world: baseline, R1 compaction, R3 truncation, R4 reopen.
            let vfs = MemVfs::new().expect("memory filesystem");
            let dir = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), dir.clone(), keys())
                .await
                .expect("create memory database");
            let epochs = build(&mut db, &commit, seed, ARMS, None).await;
            assert_eq!(epochs.len(), ARMS);

            let baseline = run(&db, &cx, None);
            let historical = run(&db, &cx, Some(epochs[2]));
            assert_ne!(historical, baseline, "AS OF must separate snapshots");
            assert_ne!(
                historical,
                run(&db, &cx, Some(epochs[7])),
                "two AS OF sequences differ"
            );
            for (name, result) in &baseline {
                let QueryResult::Rows { rows, .. } = result else {
                    unreachable!()
                };
                assert!(!rows.is_empty(), "{name}: baseline witnesses required");
            }
            let store = fgdb_strata::store::BlockStore::open_with_vfs(
                &commit,
                vfs.clone(),
                &dir,
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
            )
            .await
            .expect("block store");
            let layout = store
                .get_root(&commit, db.partition_root().expect("root"))
                .await
                .expect("resolve physical layout");
            assert!(
                layout.blocks.len() > 1,
                "history must span multiple tier-D blocks"
            );
            drop(store);
            drop(db);
            let mut db = Database::open_with_vfs(&commit, vfs.clone(), &dir, keys())
                .await
                .expect("pre-compaction reopen");
            assert_same_rows("pre-compaction reopen", &baseline, &run(&db, &cx, None));

            // R3: AS OF s vs a history truncated at s, for several earlier s.
            for stop in [3_usize, 5, 8] {
                let truncated_dir = MemVfs::new().expect("truncated filesystem");
                let truncated_path = truncated_dir.database_dir();
                let mut truncated = Database::create_with_vfs(
                    &commit,
                    truncated_dir.clone(),
                    &truncated_path,
                    keys(),
                )
                .await
                .expect("create truncated database");
                build(&mut truncated, &commit, seed, stop, None).await;
                let truncated_rows = run(&truncated, &cx, None);
                let as_of_rows = run(&db, &cx, Some(epochs[stop - 1]));
                assert_same_rows("truncation", &as_of_rows, &truncated_rows);
                drop(truncated);
                let truncated =
                    Database::open_with_vfs(&commit, truncated_dir, truncated_path, keys())
                        .await
                        .expect("prefix reopen");
                assert_same_rows("prefix reopened", &as_of_rows, &run(&truncated, &cx, None));
            }

            // R1 + R4: compaction changes layout, never an answer, and the
            // compacted generation is what a fast reopen lands on.
            let root_before = db.partition_root();
            db.compact(&commit).await.expect("compacts");
            assert_ne!(
                db.partition_root().expect("healthy root"),
                root_before.expect("healthy pre-compact root"),
                "compaction republishes the root"
            );
            assert_same_rows("compacted", &baseline, &run(&db, &cx, None));
            assert_same_rows(
                "compacted history",
                &historical,
                &run(&db, &cx, Some(epochs[2])),
            );
            let compacted_root = db.partition_root().expect("compacted root");
            let compacted_manifest = db.manifest().expect("compacted manifest");
            drop(db);

            let reopened = Database::<MemVfs>::open_with_vfs(&commit, vfs, dir, keys())
                .await
                .expect("fast reopen over retained MemVfs");
            assert_eq!(
                reopened.partition_root().expect("reopened root"),
                compacted_root,
                "reopen keeps the compacted root"
            );
            assert_eq!(
                reopened.manifest().expect("reopened manifest"),
                compacted_manifest,
                "reopen keeps the compacted manifest"
            );
            assert_same_rows("reopened", &baseline, &run(&reopened, &cx, None));
            for stop in [3_usize, 5, 8] {
                let prefix_vfs = MemVfs::new().expect("prefix filesystem");
                let prefix_path = prefix_vfs.database_dir();
                let mut prefix =
                    Database::create_with_vfs(&commit, prefix_vfs, prefix_path, keys())
                        .await
                        .expect("prefix create");
                build(&mut prefix, &commit, seed, stop, None).await;
                assert_same_rows(
                    "compacted reopened AS OF vs prefix",
                    &run(&prefix, &cx, None),
                    &run(&reopened, &cx, Some(epochs[stop - 1])),
                );
            }
            drop(reopened);

            // UnixVfs world for R2: fast open vs the forced stream fold.
            let dir = std::env::temp_dir().join(format!(
                "fgdb-metamorphic-layout-{}-{seed}",
                std::process::id()
            ));
            let mut disk = Database::create(&commit, &dir, keys())
                .await
                .expect("create disk database");
            build(&mut disk, &commit, seed, ARMS, None).await;
            disk.compact(&commit).await.expect("disk compaction");
            let disk_baseline = run(&disk, &cx, None);
            assert_same_rows("cross-layout", &baseline, &disk_baseline);
            let disk_root = disk.partition_root().expect("disk root");
            drop(disk);

            let fast = Database::open(&commit, &dir, keys())
                .await
                .expect("fast open");
            assert_eq!(
                fast.partition_root().expect("fast root"),
                disk_root,
                "fast open retains the published root"
            );
            assert_same_rows("fast open", &baseline, &run(&fast, &cx, None));
            drop(fast);

            let rebuilt = Database::open_rebuilding(&commit, &dir, keys())
                .await
                .expect("forced stream fold");
            assert_same_rows("rebuild", &baseline, &run(&rebuilt, &cx, None));
            drop(rebuilt);
            let rebuilt_reopened = Database::open(&commit, &dir, keys())
                .await
                .expect("rebuild reopen");
            assert_same_rows(
                "rebuilt reopened",
                &baseline,
                &run(&rebuilt_reopened, &cx, None),
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
