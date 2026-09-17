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
use fgdb_gql::{GqlParameters, GqlQueryPolicy, PreparedGqlTemplate};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(5);
const BOUND: i64 = 42;
const ID_TEXT: &str = "MATCH (n) WHERE n.p = $v RETURN n";
const LABELED_TEXT: &str = "MATCH (n:Tag) WHERE n.p = $v RETURN n";
const TEXT_TEXT: &str = "MATCH (n) WHERE n.q = $w RETURN n";

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
                    vec![(P, CanonicalScalar::Int((id % 3) as i64))],
                );
            }
            batch.create_vertex(
                VId(90),
                vec![],
                vec![(Q, CanonicalScalar::Text("hit".into()))],
            );
        } else {
            match rng.next() % 4 {
                // property update: value moves, making history candidates stale
                0 => {
                    let id = rng.next() % 24;
                    batch.set_vertex_property(
                        VId(id),
                        P,
                        Some(CanonicalScalar::Int((rng.next() % 3) as i64)),
                    );
                }
                // label flip
                1 => {
                    let id = rng.next() % 24;
                    batch.set_vertex_label(VId(id), L, rng.next() % 2 == 0);
                }
                // vertex delete
                2 if commit >= 2 => {
                    let id = rng.next() % 24;
                    batch.delete_vertex(VId(id));
                }
                // text property flip between the two canonical values
                _ => {
                    batch.set_vertex_property(
                        VId(90),
                        Q,
                        Some(CanonicalScalar::Text(if rng.next() % 2 == 0 {
                            "hit".into()
                        } else {
                            "miss".into()
                        })),
                    );
                }
            }
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
        })
        .map(|row| row.vid)
        .collect()
}

#[derive(Clone, Copy)]
enum Kind {
    Int,
    Labeled,
    Text,
}

/// Indexed answers == scan answers at every cut, including values that were
/// changed after creation and vertices deleted after creation. Anti-vacuity:
/// assert non-empty results at at least one cut per direction.
#[test]
fn equality_bound_answers_equal_scan_answers_across_history() {
    for (lab_seed, graph_seed) in [
        (0xbpm_0001_u64, 11_u64),
        (0xbpm_0002, 23),
        (0xbpm_0003, 0xdead_f00d),
    ] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let seqs = generated(graph_seed, &mut db, &commit).await;
            let names = RelationBind::new()
                .with_relation("R", R)
                .with_label("Tag", L)
                .with_property("p", P)
                .with_property("q", Q);
            let int_template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
            let labeled_template = PreparedGqlTemplate::prepare(LABELED_TEXT, &names).unwrap();
            let text_template = PreparedGqlTemplate::prepare(TEXT_TEXT, &names).unwrap();
            let int_args = GqlParameters::new().with_int64("v", BOUND).unwrap();
            let text_args = GqlParameters::new().with_string("w", "hit").unwrap();
            let mut saw_nonempty = [false; 3];
            for at in &seqs {
                for (kind, template, args) in [
                    (Kind::Int, &int_template, &int_args),
                    (Kind::Labeled, &labeled_template, &int_args),
                    (Kind::Text, &text_template, &text_args),
                ] {
                    let query = template.bind_parameters(args).unwrap();
                    let expected = oracle(&db, *at, kind);
                    let rows = db
                        .execute_prepared_query_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    let got: Vec<VId> = rows
                        .value
                        .iter()
                        .map(|row| row.as_vertex().unwrap())
                        .collect();
                    assert_eq!(
                        got,
                        expected,
                        "at={at:?} kind={kind:?}",
                        kind = match kind {
                            Kind::Int => "int",
                            Kind::Labeled => "labeled",
                            Kind::Text => "text",
                        }
                    );
                    saw_nonempty[kind as usize] |= !rows.value.is_empty();
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

/// An equality lookup charges identical Work/SnapshotRecord counts on a
/// 1k-vertex and a 50k-vertex graph with the same matching set.
#[test]
fn equality_lookup_charges_stay_constant_as_vertex_count_grows() {
    let mut charged = Vec::new();
    for (lab_seed, total) in [(0xbpm_0042_u64, 1_000_usize), (0xbpm_0043, 50_000)] {
        let (result, report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let names = RelationBind::new().with_property("p", P);
            let template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
            let args = GqlParameters::new().with_int64("v", BOUND).unwrap();
            let query = template.bind_parameters(&args).unwrap();
            let wide = GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000);
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = graph_of(&mut db, &commit, total).await;
            let run = db
                .execute_prepared_query_governed_at(&query_cx, &query, at, wide)
                .unwrap();
            (
                run.rows.snapshot_records,
                run.evaluator.work_units,
                run.value,
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

/// Chunked commits: `total` vertices, every 100th carries BOUND under P,
/// everything else carries a different value. Returns the final commit.
async fn graph_of(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx, total: usize) -> CommitSeq {
    let mut seq = CommitSeq(0);
    let mut at = 0_usize;
    while at < total {
        let mut batch = WriteBatch::new(R);
        let end = (at + 1_000).min(total);
        for index in at..end {
            let value = if index % 100 == 0 {
                CanonicalScalar::Int(BOUND)
            } else {
                CanonicalScalar::Int(1 + (index % 97) as i64)
            };
            batch.create_vertex(VId(index as u128), vec![], vec![(P, value)]);
        }
        at = end;
        seq = db.write(cx, batch).await.unwrap();
    }
    seq
}
