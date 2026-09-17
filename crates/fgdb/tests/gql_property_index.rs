//! Differential + complexity gates for equality-bound vertex admission
//! (fgdb-bupm).
//!
//! The independent oracle is the *storage* vertex merge over the admitted
//! snapshot (`Database::vertices_at`), folded in the test into the exact
//! answer a whole-graph scan would produce for the same predicate. The
//! equality-served query must return byte-identical rows in the same order at
//! every admitted sequence, including vertices whose value was later changed
//! and vertices deleted after creation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy, GqlScalarParameter,
    GraphSymbol, GraphSymbolKind, PreparedGqlTemplate, PreparedGraphText,
};
use fgdb_types::CanonicalScalarKind;
use fgdb_types::CanonicalText;
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(5);
const BOUND: i64 = 42;
const ID_TEXT: &str = "MATCH (n) WHERE n.p = $v RETURN n";
const LABELED_TEXT: &str = "MATCH (n:Tag) WHERE n.p = $v RETURN n";
const TEXT_TEXT: &str = "MATCH (n) WHERE n.q = $w RETURN n";
const RANGE_TEXT: &str = "MATCH (n) WHERE n.p > $v RETURN n";
const UNBOUND_TEXT: &str = "MATCH (n) RETURN n";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb2; 32],
        DatabaseSecurityNamespaceId([0xb3; 32]),
        [0xb4; 32],
    )
}

/// Deterministic LCG; the closed universe has no rand crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}

/// >=6 commits mixing property updates, label flips and vertex deletes on a
/// shared vertex domain, so every commit changes what equality can observe.
async fn generated(
    seed: u64,
    db: &mut Database<MemVfs>,
    cx: &fgdb_types::CommitCx,
) -> Vec<CommitSeq> {
    let mut rng = Rng(seed | 1);
    let mut seqs = Vec::new();
    for commit in 0..6_u64 {
        let mut batch = WriteBatch::new(R);
        if commit == 0 {
            for id in 0..24_u128 {
                batch.create_vertex(
                    VId(id),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(if id % 2 == 0 { BOUND } else { 1 }))],
                );
            }
            batch.create_vertex(
                VId(90),
                vec![],
                vec![(
                    Q,
                    CanonicalScalar::Text(CanonicalText::new_ucs_basic("hit").unwrap()),
                )],
            );
        } else {
            // Every seed exercises all transitions, including a newly matching
            // value that a first-insert-only index cannot discover.
            batch.set_vertex_property(
                VId(1),
                P,
                Some(CanonicalScalar::Int(if commit % 2 == 1 {
                    BOUND
                } else {
                    1
                })),
            );
            batch.set_vertex_property(
                VId(0),
                P,
                if commit == 5 {
                    None
                } else {
                    Some(CanonicalScalar::Int(if rng.next() % 2 == 0 {
                        BOUND
                    } else {
                        2
                    }))
                },
            );
            batch.set_vertex_label(VId(2), L, commit % 2 == 0);
            batch.delete_vertex(VId(u128::from(10 + commit * 2)));
            batch.set_vertex_property(
                VId(90),
                Q,
                Some(CanonicalScalar::Text(
                    CanonicalText::new_ucs_basic(if commit % 2 == 0 { "hit" } else { "miss" })
                        .unwrap(),
                )),
            );
        }
        seqs.push(db.write(cx, batch).await.unwrap());
    }
    seqs
}

/// The independent oracle: visible rows from the storage merge, filtered by
/// the same predicate semantics the engine compiles. Ordering follows the
/// merge (ascending VId).
fn oracle(db: &Database<MemVfs>, at: CommitSeq, kind: Kind) -> Vec<VId> {
    db.vertices_at(at)
        .unwrap()
        .into_iter()
        .filter(|row| match kind {
            Kind::Int => row.props.iter().any(|(key, value)| {
                *key == P && matches!(value, CanonicalScalar::Int(actual) if *actual == BOUND)
            }),
            Kind::Labeled => {
                row.labels.contains(&L)
                    && row.props.iter().any(|(key, value)| {
                        *key == P
                            && matches!(value, CanonicalScalar::Int(actual) if *actual == BOUND)
                    })
            }
            Kind::Text => row.props.iter().any(|(key, value)| {
                *key == Q
                    && matches!(value, CanonicalScalar::Text(actual) if actual.as_str() == "hit")
            }),
            Kind::Range => row.props.iter().any(|(key, value)| {
                *key == P && matches!(value, CanonicalScalar::Int(actual) if *actual > BOUND)
            }),
            Kind::Unbound => true,
        })
        .map(|row| row.vid)
        .collect()
}

#[derive(Clone, Copy)]
enum Kind {
    Int,
    Labeled,
    Text,
    Unbound,
    Range,
}

/// Indexed answers == scan answers at every cut, including values that were
/// changed after creation and vertices deleted after creation. Anti-vacuity:
/// assert non-empty results at at least one cut per direction.
#[test]
fn equality_bound_answers_equal_scan_answers_across_history() {
    for (lab_seed, graph_seed) in [
        (0xb01_0001_u64, 11_u64),
        (0xb01_0002, 23),
        (0xb01_0003, 0xdead_f00d),
    ] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let seqs = generated(graph_seed, &mut db, &commit).await;
            let symbols = |kind: GraphSymbolKind, name: &str| match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
                (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
                (GraphSymbolKind::Label, "Tag") => Some(GraphSymbol::Label(L)),
                _ => None,
            };
            let int_template = PreparedGraphText::prepare(ID_TEXT, symbols).unwrap();
            let labeled_template = PreparedGraphText::prepare(LABELED_TEXT, symbols).unwrap();
            let text_template = PreparedGraphText::prepare_with_parameter_types(
                TEXT_TEXT,
                &[("w", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
                symbols,
            )
            .unwrap();
            let range_template = PreparedGraphText::prepare(RANGE_TEXT, symbols).unwrap();
            let unbound_template = PreparedGraphText::prepare(UNBOUND_TEXT, symbols).unwrap();
            let int_args = GqlParameters::new().with_int64("v", BOUND).unwrap();
            let text_args = GqlParameters::new().with_text("w", "hit").unwrap();
            let mut saw_nonempty = [false; 3];
            for at in &seqs {
                for (kind, slot, template, args) in [
                    (Kind::Int, 0usize, &int_template, &int_args),
                    (Kind::Labeled, 1, &labeled_template, &int_args),
                    (Kind::Text, 2, &text_template, &text_args),
                ] {
                    let query = template.bind_parameters(args).unwrap();
                    let expected = oracle(&db, *at, kind);
                    let rows = db
                        .execute_graph_pattern_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    let got: Vec<VId> = rows
                        .value
                        .iter()
                        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                        .collect();
                    assert_eq!(got, expected, "at={at:?} direction={}", slot);
                    saw_nonempty[slot] |= !got.is_empty();
                }
                // Scan-path shapes: a range predicate and an unbound vertex
                // scan must bypass the index yet answer identically to the
                // storage oracle.
                for (kind, template, args) in [
                    (Kind::Range, &range_template, &int_args),
                    (Kind::Unbound, &unbound_template, &GqlParameters::new()),
                ] {
                    let query = template.bind_parameters(args).unwrap();
                    let rows = db
                        .execute_graph_pattern_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    let got: Vec<VId> = rows
                        .value
                        .iter()
                        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                        .collect();
                    assert_eq!(got, oracle(&db, *at, kind), "scan at={at:?}");
                }
            }
            assert!(
                saw_nonempty.iter().all(|seen| *seen),
                "seed {graph_seed}: differential produced no rows for some direction"
            );
        });
        assert!(report.lab_test_passed(), "seed {graph_seed}: {report:?}");
    }
}

/// A root equality may prune its own scan, never the shared domain of a later
/// join. Range conjunctions express the same integer predicate without using
/// the equality index; explicit expected identities prevent common-mode loss.
#[test]
fn equality_and_scan_agree_for_staged_and_retained_join_domains() {
    let ((), report) = run_async_under_lab(0xb01_0050, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, p) in [(1, BOUND), (2, BOUND), (3, 9)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![
                    (P, CanonicalScalar::Int(p)),
                    (Q, CanonicalScalar::Int(id as i64)),
                ],
            );
        }
        let old = db.write(&commit, batch).await.unwrap();
        let pinned = db.read_session().unwrap();
        let symbols = |kind: GraphSymbolKind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        };
        let pairs = [
            (
                "MATCH (n) WHERE n.p=42 RETURN n",
                "MATCH (n) WHERE n.p>=42 AND n.p<=42 RETURN n",
            ),
            (
                "MATCH (a) WHERE a.q=1 OPTIONAL MATCH (b) WHERE b.p=a.p RETURN b",
                "MATCH (a) WHERE a.q>=1 AND a.q<=1 OPTIONAL MATCH (b) WHERE b.p=a.p RETURN b",
            ),
        ]
        .map(|(indexed, scan)| {
            [indexed, scan].map(|text| {
                PreparedGraphText::prepare(text, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
            })
        });
        let wide = GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000);
        let before = [vec![VId(1), VId(2)], vec![VId(1), VId(2)]];
        let after = [vec![VId(2)], vec![VId(1), VId(3), VId(4)]];
        macro_rules! check {
            ($results:expr, $expected:expr) => {{
                let [indexed, scan] = $results;
                let indexed = indexed.unwrap().value;
                let scan = scan.unwrap().value;
                assert_eq!(indexed, scan);
                assert_eq!(
                    indexed
                        .iter()
                        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                        .collect::<Vec<_>>(),
                    $expected
                );
            }};
        }
        for (pair, expected) in pairs.iter().zip(&before) {
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed(&cx, query, wide)),
                *expected
            );
        }
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
        staged.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(9))]);
        txn.write(&mut db, staged).unwrap();
        for ((pair, expected), retained) in pairs.iter().zip(&after).zip(&before) {
            check!(
                pair.each_ref()
                    .map(|query| txn.execute_graph_pattern_governed(&db, &cx, query, wide)),
                *expected
            );
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed(&cx, query, wide)),
                *retained
            );
        }
        txn.commit(&mut db, &commit).await.unwrap();
        for ((pair, expected), retained) in pairs.iter().zip(&after).zip(&before) {
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed(&cx, query, wide)),
                *expected
            );
            check!(
                pair.each_ref()
                    .map(|query| pinned.execute_graph_pattern_governed(&cx, query, wide)),
                *retained
            );
        }
        db.compact(&commit).await.unwrap();
        for ((pair, expected), retained) in pairs.iter().zip(&after).zip(&before) {
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed(&cx, query, wide)),
                *expected
            );
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed_at(&cx, query, old, wide)),
                *retained
            );
        }
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for ((pair, expected), retained) in pairs.iter().zip(&after).zip(&before) {
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed(&cx, query, wide)),
                *expected
            );
            check!(
                pair.each_ref()
                    .map(|query| db.execute_graph_pattern_governed_at(&cx, query, old, wide)),
                *retained
            );
            check!(
                pair.each_ref()
                    .map(|query| pinned.execute_graph_pattern_governed(&cx, query, wide)),
                *retained
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// An equality lookup charges identical Work/SnapshotRecord counts on a
/// 1k-vertex and a 50k-vertex graph with the same matching set.
#[test]
fn equality_lookup_charges_stay_constant_as_vertex_count_grows() {
    let mut charged = Vec::new();
    for (lab_seed, total) in [(0xb01_0042_u64, 1_000_usize), (0xb01_0043, 50_000)] {
        let (result, report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let symbols = |kind: GraphSymbolKind, name: &str| match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
                _ => None,
            };
            let template = PreparedGraphText::prepare(ID_TEXT, symbols).unwrap();
            let args = GqlParameters::new().with_int64("v", BOUND).unwrap();
            let query = template.bind_parameters(&args).unwrap();
            let wide = GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000);
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = graph_of(&mut db, &commit, total).await;
            let run = db
                .execute_graph_pattern_governed_at(&query_cx, &query, at, wide)
                .unwrap();
            (
                run.rows.snapshot_records,
                run.evaluator.work_units,
                run.value.len(),
            )
        });
        assert!(report.lab_test_passed(), "total {total}: {report:?}");
        charged.push((total, result));
    }
    // Same matching set: only candidate lookups differ, charges must not.
    assert_eq!(charged[0].1.0, charged[1].1.0, "snapshot records");
    assert_eq!(charged[0].1.1, charged[1].1.1, "work units");
    assert_eq!(charged[0].1.2, charged[1].1.2, "result rows");
    assert!(charged[0].1.0 > 0, "equality lookup must admit matches");
}

/// Chunked commits: exactly the first ten vertices carry BOUND under P;
/// everything else carries a different value. Returns the final commit.
async fn graph_of(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx, total: usize) -> CommitSeq {
    let mut seq = CommitSeq(0);
    let mut at = 0_usize;
    while at < total {
        let mut batch = WriteBatch::new(R);
        let end = (at + 1_000).min(total);
        for index in at..end {
            let value = if index < 10 {
                CanonicalScalar::Int(BOUND)
            } else {
                CanonicalScalar::Int(-1 - (index % 97) as i64)
            };
            batch.create_vertex(VId(index as u128), vec![], vec![(P, value)]);
        }
        at = end;
        seq = db.write(cx, batch).await.unwrap();
    }
    seq
}
