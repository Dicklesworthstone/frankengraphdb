//! Native scripts against an independent map model (fgdb-gql-write-crash-oracle-sxte).
//! Reuses WriteTxn::commit_with_crash from tests/writetxn_crash.rs and the
//! UnixVfs survived-marker/torn-tail + fast-open/full-fold harness from
//! fgdb/tests/atomic_recovery.rs. All seven Chronicle CrashPoints are covered;
//! the parent-directory point fires only on a FIRST commit (fgdb/tests/spine.rs).
//! This is process-stop recovery, not a power-loss simulator: complete unsynced
//! marker bytes survive, whereas truncating their trailer discards that marker.

use asupersync::{fs::UnixVfs, lab::run_async_under_lab};
use fgdb::{CrashPoint, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Props = BTreeMap<PropertyKeyId, CanonicalScalar>;
type Vertex = (Vec<LabelId>, Props);
type Edge = (VId, RelationId, VId, Props);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    vertices: BTreeMap<VId, Vertex>,
    edges: BTreeMap<EId, Edge>,
}

fn props(p: i64, q: Option<i64>) -> Props {
    let mut values = BTreeMap::from([(P, CanonicalScalar::Int(p))]);
    if let Some(q) = q {
        values.insert(Q, CanonicalScalar::Int(q));
    }
    values
}

// This model never consumes engine receipts/deltas or calls GQL evaluation.
// The same generated intent supplies native source and simple map semantics.
#[derive(Clone, Debug)]
enum Op {
    Create { id: VId, p: i64, insert: bool },
    Merge { id: EId, a: i64, b: i64 },
    Set { p: i64 },
    Remove { p: i64 },
    Delete { p: i64, detach: bool },
}
impl Op {
    fn source(&self) -> String {
        match *self {
            Self::Create { p, insert, .. } => format!(
                "{} (n:Person {{p:{p},q:$value}})",
                if insert { "INSERT" } else { "CREATE" }
            ),
            Self::Merge { a, b, .. } => format!(
                "MATCH (a:Person),(b:Person) WHERE a.p={a} AND b.p={b} MERGE (a)-[e:R]->(b) ON CREATE SET e.q=$fresh ON MATCH SET e.q=$seen"
            ),
            Self::Set { p } => format!("MATCH (n:Person) WHERE n.p={p} SET n.q=$changed"),
            Self::Remove { p } => format!("MATCH (n:Person) WHERE n.p={p} REMOVE n.q"),
            Self::Delete { p, detach } => format!(
                "MATCH (n:Person) WHERE n.p={p} {}DELETE n",
                if detach { "DETACH " } else { "" }
            ),
        }
    }
    fn apply(&self, state: &mut State, value: i64) {
        let find = |s: &State, p| {
            *s.vertices
                .iter()
                .find(|(_, (_, values))| values.get(&P) == Some(&CanonicalScalar::Int(p)))
                .unwrap()
                .0
        };
        match *self {
            Self::Create { id, p, .. } => {
                assert!(
                    state
                        .vertices
                        .insert(id, (vec![PERSON], props(p, Some(value))))
                        .is_none()
                );
            }
            Self::Merge { id, a, b } => {
                let a = find(state, a);
                let b = find(state, b);
                if let Some((_, _, _, values)) = state
                    .edges
                    .values_mut()
                    .find(|(src, r, dst, _)| *src == a && *r == R && *dst == b)
                {
                    values.insert(Q, CanonicalScalar::Int(value + 2));
                } else {
                    assert!(
                        state
                            .edges
                            .insert(
                                id,
                                (
                                    a,
                                    R,
                                    b,
                                    BTreeMap::from([(Q, CanonicalScalar::Int(value + 1))])
                                )
                            )
                            .is_none()
                    );
                }
            }
            Self::Set { p } => {
                let id = find(state, p);
                state
                    .vertices
                    .get_mut(&id)
                    .unwrap()
                    .1
                    .insert(Q, CanonicalScalar::Int(value + 3));
            }
            Self::Remove { p } => {
                let id = find(state, p);
                state.vertices.get_mut(&id).unwrap().1.remove(&Q);
            }
            Self::Delete { p, detach } => {
                let id = find(state, p);
                if !detach {
                    assert!(
                        !state
                            .edges
                            .values()
                            .any(|(a, _, b, _)| *a == id || *b == id)
                    );
                }
                state.edges.retain(|_, (a, _, b, _)| *a != id && *b != id);
                state.vertices.remove(&id);
            }
        }
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        10_000,
        100,
        100,
    )
}
fn observed(db: &Database) -> State {
    State {
        vertices: db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|v| (v.vid, (v.labels, v.props.into_iter().collect())))
            .collect(),
        edges: db
            .edges()
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.entry.eid,
                    (
                        e.entry.src,
                        e.entry.relation,
                        e.entry.dst,
                        e.props.into_iter().collect(),
                    ),
                )
            })
            .collect(),
    }
}

fn generated(seed: u64) -> (State, Vec<Op>, Vec<Op>, i64) {
    // LCG choices change graph size, endpoints, property values and statement order.
    let mut random = seed;
    let mut next = || {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        random
    };
    let count = 4 + next() % 4;
    let value = (next() % 10_000) as i64;
    let creates: Vec<_> = (1..=count)
        .map(|id| Op::Create {
            id: VId(id.into()),
            p: id as i64,
            insert: id % 2 == 0,
        })
        .collect();
    let mut initial = State::default();
    for op in &creates {
        op.apply(&mut initial, value);
    }
    // A retained edge plus generated incident edges make the cascade observable.
    initial
        .edges
        .insert(EId(1), (VId(1), R, VId(2), props(value, None)));
    for id in 2..count {
        initial.edges.insert(
            EId(id.into()),
            (
                VId((id + 1).into()),
                R,
                VId(1),
                props(value + id as i64, None),
            ),
        );
    }
    let fresh = VId(u128::from(count + 1));
    let mut mixed = vec![
        Op::Create {
            id: fresh,
            p: 100,
            insert: next() & 1 == 0,
        },
        Op::Merge {
            id: EId(100),
            a: 2,
            b: 100,
        },
        Op::Merge {
            id: EId(101),
            a: 2,
            b: 100,
        },
        Op::Merge {
            id: EId(102),
            a: 100,
            b: 1,
        },
        Op::Set { p: 2 },
        Op::Remove { p: 3 },
        Op::Create {
            id: VId(u128::from(count + 2)),
            p: 200,
            insert: true,
        },
        Op::Delete {
            p: 200,
            detach: false,
        },
        Op::Delete { p: 1, detach: true },
    ];
    if next() & 1 != 0 {
        mixed.swap(4, 5);
    }
    // FIRST-commit script also creates then mutates/deletes its own overlay.
    let mut first = creates;
    first.extend(mixed.clone());
    (initial, mixed, first, value)
}

#[derive(Clone, Copy, Debug)]
struct Case {
    point: Option<CrashPoint>,
    tear: u64,
}
fn matrix() -> Vec<Case> {
    let mut cases: Vec<_> = [
        CrashPoint::BeforeCapsule,
        CrashPoint::AfterCapsuleBeforeD1,
        CrashPoint::AfterCapsuleFileSyncBeforeDirectorySync,
        CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync,
        CrashPoint::AfterD1,
        CrashPoint::AfterMarkerBeforeD2,
        CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
    ]
    .into_iter()
    .map(|point| Case {
        point: Some(point),
        tear: 0,
    })
    .collect();
    cases.push(Case {
        point: None,
        tear: 0,
    });
    // The helper removes only bytes from the current marker's trailer, never
    // acknowledged history. Two partial-tail lengths exercise decoder boundaries.
    for tear in [1, 8] {
        cases.push(Case {
            point: Some(CrashPoint::AfterMarkerBeforeD2),
            tear,
        });
    }
    cases
}
fn committed(case: Case, first: bool) -> bool {
    case.tear == 0
        && match case.point {
            None
            | Some(
                CrashPoint::AfterMarkerBeforeD2
                | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
            ) => true,
            Some(CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync) => !first,
            _ => false,
        }
}
fn check(actual: &State, pre: &State, post: &State, durable: bool, context: &str) {
    assert!(
        actual == pre || actual == post,
        "PARTIAL_STATE {context}\nactual={actual:?}\npre={pre:?}\npost={post:?}"
    );
    assert_eq!(
        actual,
        if durable { post } else { pre },
        "durability boundary {context}"
    );
}

fn run_seed(seed: u64) {
    static RUN: AtomicU64 = AtomicU64::new(0);
    let run = RUN.fetch_add(1, Ordering::Relaxed);
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let (base, mixed, first_ops, value) = generated(seed);
        let arguments = GqlParameters::new()
            .with_int64("value", value)
            .unwrap()
            .with_int64("fresh", value + 1)
            .unwrap()
            .with_int64("seen", value + 2)
            .unwrap()
            .with_int64("changed", value + 3)
            .unwrap();
        let mut pre_count = 0;
        let mut post_count = 0;
        let mut fired = [0; 7];
        for first in [false, true] {
            let ops = if first { &first_ops } else { &mixed };
            let pre = if first {
                State::default()
            } else {
                base.clone()
            };
            let mut post = pre.clone();
            for op in ops {
                op.apply(&mut post, value);
            }
            assert!(ops.len() >= 3);
            assert_ne!(pre, post);
            assert!(!post.vertices.contains_key(&VId(1)));
            assert!(
                !post
                    .edges
                    .values()
                    .any(|(a, _, b, _)| *a == VId(1) || *b == VId(1))
            );
            assert!(!post.edges.is_empty());
            let source = ops.iter().map(Op::source).collect::<Vec<_>>().join("; ");
            let script = PreparedGraphWriteScript::prepare(&source, R, symbols).unwrap();
            for (index, case) in matrix().into_iter().enumerate() {
                let context = format!("seed={seed:#x} first={first} case={case:?} script={source}");
                // Unique directories, retained for diagnosis. No deletion helpers.
                let path = std::env::temp_dir().join(format!(
                    "fgdb-script-crash-{}-{run}-{seed}-{first}-{index}",
                    std::process::id()
                ));
                let mut db = Database::create(&commit, &path, keys()).await.unwrap();
                if !first {
                    let mut batch = WriteBatch::new(R);
                    for (id, (labels, values)) in &pre.vertices {
                        batch.create_vertex(
                            *id,
                            labels.clone(),
                            values.iter().map(|(k, v)| (*k, v.clone())).collect(),
                        );
                    }
                    for (id, (a, _, b, values)) in &pre.edges {
                        batch.add_edge(
                            *id,
                            *a,
                            *b,
                            values.iter().map(|(k, v)| (*k, v.clone())).collect(),
                        );
                    }
                    db.write(&commit, batch).await.unwrap();
                }
                assert_eq!(observed(&db), pre);
                let basis = db.frontier().unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                let receipt = txn
                    .execute_graph_write_script_governed(
                        &mut db,
                        &query,
                        &script,
                        &arguments,
                        policy(),
                        |request| {
                            Ok::<_, ()>(match (&ops[request.statement], request.request) {
                                (Op::Create { id, .. }, GraphInsertRequest::Vertex { .. }) => {
                                    ElementId::Vertex(*id)
                                }
                                (Op::Merge { id, .. }, GraphInsertRequest::Edge { .. }) => {
                                    ElementId::Edge(*id)
                                }
                                other => panic!("unexpected identity request {other:?}"),
                            })
                        },
                    )
                    .unwrap();
                assert_eq!(receipt.stats().completed_statements as usize, ops.len());
                assert_eq!(observed(&db), pre, "staging leaked: {context}");
                let outcome = txn.commit_with_crash(&mut db, &commit, case.point).await;
                let should_fire = case.point.is_some()
                    && (first
                        || case.point
                            != Some(
                                CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync,
                            ));
                assert_eq!(
                    outcome.is_err(),
                    should_fire,
                    "unreached point: {context}: {outcome:?}"
                );
                if index < 7 && should_fire {
                    fired[index] += 1;
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
                drop(txn);
                drop(db);
                if case.tear != 0 {
                    CommitCoordinator::<UnixVfs>::tear_log_tail_for_test(&path, case.tear).unwrap();
                }
                let durable = committed(case, first);
                let expected_seq = CommitSeq(basis.0 + u64::from(durable));
                let reopened = Database::open(&commit, &path, keys()).await.unwrap();
                check(
                    &observed(&reopened),
                    &pre,
                    &post,
                    durable,
                    &format!("fast {context}"),
                );
                assert_eq!(reopened.frontier().unwrap(), expected_seq, "{context}");
                drop(reopened);
                let mut rebuilt = Database::open_rebuilding(&commit, &path, keys())
                    .await
                    .unwrap();
                check(
                    &observed(&rebuilt),
                    &pre,
                    &post,
                    durable,
                    &format!("rebuild {context}"),
                );
                assert_eq!(rebuilt.frontier().unwrap(), expected_seq, "{context}");
                if durable {
                    post_count += 1;
                } else {
                    pre_count += 1;
                }
                // Allocator floors cover committed creations, including base
                // elements deleted by this script. Same-script create/delete
                // folds away and does not spend an ID (spine.rs:553-565).
                let id = rebuilt
                    .allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap();
                let ElementId::Vertex(id) = id else {
                    panic!("vertex allocator returned edge")
                };
                for old in pre
                    .vertices
                    .keys()
                    .chain(post.vertices.keys().filter(|_| durable))
                {
                    assert!(id.0 > old.0, "reused committed vertex identity: {context}");
                }
                let edge = rebuilt
                    .allocate_identity(&query, GraphInsertRequest::Edge { row: 0, edge: 0 })
                    .unwrap();
                let ElementId::Edge(edge) = edge else {
                    panic!("edge allocator returned vertex")
                };
                for old in pre
                    .edges
                    .keys()
                    .chain(post.edges.keys().filter(|_| durable))
                {
                    assert!(edge.0 > old.0, "reused committed edge identity: {context}");
                }
                let mut next = if durable { post.clone() } else { pre.clone() };
                let target = next.vertices.keys().next().copied().unwrap_or(id);
                let follow_value = value + index as i64 + 50;
                let mut follow = WriteBatch::new(R);
                follow.create_vertex(
                    id,
                    vec![PERSON],
                    vec![(P, CanonicalScalar::Int(follow_value))],
                );
                follow.add_edge(edge, id, target, vec![]);
                next.vertices
                    .insert(id, (vec![PERSON], props(follow_value, None)));
                next.edges.insert(edge, (id, R, target, Props::new()));
                assert_eq!(
                    rebuilt.write(&commit, follow).await.unwrap(),
                    CommitSeq(expected_seq.0 + 1)
                );
                assert_eq!(rebuilt.frontier().unwrap(), CommitSeq(expected_seq.0 + 1));
                assert_eq!(observed(&rebuilt), next, "followup {context}");
                drop(rebuilt);
                let final_db = Database::open(&commit, &path, keys()).await.unwrap();
                assert_eq!(observed(&final_db), next, "followup reopen {context}");
            }
        }
        assert_eq!(fired, [2, 2, 2, 1, 2, 2, 2]);
        assert!(pre_count > 0 && post_count > 0);
        println!("seed={seed:#x} fired={fired:?} pre={pre_count} post={post_count}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn script_crash_seed_901() {
    run_seed(0x901);
}
#[test]
fn script_crash_seed_902() {
    run_seed(0x902);
}
#[test]
fn script_crash_seed_903() {
    run_seed(0x903);
}
#[test]
fn script_crash_seed_904() {
    run_seed(0x904);
}
#[test]
fn script_crash_seed_905() {
    run_seed(0x905);
}
