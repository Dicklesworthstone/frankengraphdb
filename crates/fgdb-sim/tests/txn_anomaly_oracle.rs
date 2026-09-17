//! Generated overlapping WriteTxn histories, checked by independent serial replay.
//! fgdb-txn-anomaly-oracle-xye6; plan §7.1, §7.3 and FG-INV-05/06.
//!
//! Rules: fgdb-reference/src/txn.rs:10-34 models SI (which admits write skew),
//! not this product's stronger admission rule. Reference ssi.rs:20-33 explains
//! reader-to-writer dependencies and why a dangerous structure alone is NOT a
//! proof of nonserializability. We instead enumerate every committed serial
//! order, checking each observation in program order and the complete final
//! logical vertex state. No product evaluator or validator computes expectations.
//!
//! Product fcw.rs:12-41,72-138 specifies complete-suffix first-committer-wins.
//! write_txn_parts/finish.rs:82-90,330-385 specifies conservative read/scan and
//! mutation validation, including matching inserts/label gains. Read-only finish
//! is sequence-free; it can refuse stale reads. Native scans conservatively read
//! existing candidates (gql_overlay_graph.rs), so serializable admission is NOT
//! required for every semantically harmless overlap. Same-basis staging precedes
//! publication, as lifecycle.rs:49-65 requires. This is single-process explicit
//! interleaving, not parallel publication or a claim of full SSI machinery.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxn, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, QueryCx,
    VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const L: LabelId = LabelId(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    labels: Vec<LabelId>,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
}
type State = BTreeMap<VId, Node>;

fn node(value: i64, selected: bool) -> Node {
    Node {
        labels: if selected { vec![L] } else { vec![] },
        props: vec![(P, CanonicalScalar::Int(value))],
    }
}

fn normalize(row: VertexRow) -> (VId, Node) {
    let mut labels = row.labels;
    let mut props = row.props;
    labels.sort_unstable();
    props.sort_by_key(|(key, _)| *key);
    (row.vid, Node { labels, props })
}

fn state(db: &Database<MemVfs>) -> State {
    db.vertices().unwrap().into_iter().map(normalize).collect()
}

#[derive(Clone, Debug)]
enum Op {
    Point(VId),
    LabelScan,
    NativeMatch,
    Set(VId, i64),
    Create(VId, i64, bool),
    Delete(VId),
    Label(VId, bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Observation {
    Point(Option<Node>),
    Ids(Vec<VId>),
}

#[derive(Clone, Debug)]
struct Event {
    op: Op,
    observed: Option<Observation>,
}

// Pure logical interpreter. No fgdb or fgdb-gql calls in either this function
// or the serial-order search. Failed preconditions invalidate that serial order.
fn interpret(state: &mut State, op: &Op) -> Option<Option<Observation>> {
    match *op {
        Op::Point(id) => Some(Some(Observation::Point(state.get(&id).cloned()))),
        Op::LabelScan | Op::NativeMatch => Some(Some(Observation::Ids(
            state
                .iter()
                .filter(|(_, n)| n.labels.contains(&L))
                .map(|(id, _)| *id)
                .collect(),
        ))),
        Op::Set(id, value) => {
            state.get_mut(&id)?.props = vec![(P, CanonicalScalar::Int(value))];
            Some(None)
        }
        Op::Create(id, value, selected) => {
            if state.contains_key(&id) {
                return None;
            }
            state.insert(id, node(value, selected));
            Some(None)
        }
        Op::Delete(id) => {
            state.remove(&id)?;
            Some(None)
        }
        Op::Label(id, selected) => {
            state.get_mut(&id)?.labels = if selected { vec![L] } else { vec![] };
            Some(None)
        }
    }
}

fn serializable(
    initial: &State,
    histories: &[Vec<Event>],
    committed: &[usize],
    final_state: &State,
) -> bool {
    fn search(
        current: &State,
        histories: &[Vec<Event>],
        remaining: &[usize],
        final_state: &State,
    ) -> bool {
        if remaining.is_empty() {
            return current == final_state;
        }
        for (slot, index) in remaining.iter().enumerate() {
            let mut candidate = current.clone();
            if !histories[*index]
                .iter()
                .all(|event| interpret(&mut candidate, &event.op) == Some(event.observed.clone()))
            {
                continue;
            }
            let mut rest = remaining.to_vec();
            rest.remove(slot);
            if search(&candidate, histories, &rest, final_state) {
                return true;
            }
        }
        false
    }
    search(initial, histories, committed, final_state)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Selected") => Some(GraphSymbol::Label(L)),
        _ => None,
    }
}

fn execute(
    txn: &mut WriteTxn,
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    op: &Op,
) -> Option<Observation> {
    match *op {
        Op::Point(id) => Some(Observation::Point(
            txn.vertex(db, id).unwrap().map(|r| normalize(r).1),
        )),
        Op::LabelScan => {
            let mut ids: Vec<_> = txn
                .vertices(db)
                .unwrap()
                .into_iter()
                .filter(|r| r.labels.contains(&L))
                .map(|r| r.vid)
                .collect();
            ids.sort_unstable();
            Some(Observation::Ids(ids))
        }
        Op::NativeMatch => {
            let template =
                PreparedGraphText::prepare("MATCH (n:Selected) RETURN n", symbols).unwrap();
            let pattern = template.bind_parameters(&GqlParameters::new()).unwrap();
            let result = txn
                .execute_graph_pattern_governed(
                    db,
                    cx,
                    &pattern,
                    GqlQueryPolicy::new(1000, 1000, 2_000_000, 2_000_000),
                )
                .unwrap();
            let mut ids: Vec<_> = result
                .value
                .iter()
                .map(|r| r.values()[0].as_vertex().unwrap())
                .collect();
            ids.sort_unstable();
            Some(Observation::Ids(ids))
        }
        _ => {
            let mut batch = WriteBatch::new(R);
            match *op {
                Op::Set(id, value) => {
                    batch.set_vertex_property(id, P, Some(CanonicalScalar::Int(value)));
                }
                Op::Create(id, value, selected) => {
                    let n = node(value, selected);
                    batch.create_vertex(id, n.labels, n.props);
                }
                Op::Delete(id) => {
                    batch.delete_vertex(id);
                }
                Op::Label(id, selected) => {
                    batch.set_vertex_label(id, L, selected);
                }
                _ => unreachable!("read handled above"),
            }
            txn.write(db, batch).unwrap();
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Family {
    LostUpdate,
    WriteSkew,
    ReadOnly,
    Phantom,
    DeleteUpdate,
}
const FAMILIES: [Family; 5] = [
    Family::LostUpdate,
    Family::WriteSkew,
    Family::ReadOnly,
    Family::Phantom,
    Family::DeleteUpdate,
];

fn programs(family: Family, seed: u64, value: i64) -> Vec<Vec<Op>> {
    let a = VId(1);
    let b = VId(2);
    let mut programs = match family {
        Family::LostUpdate => vec![
            vec![Op::Point(a), Op::Set(a, value + 1)],
            vec![Op::Point(a), Op::Set(a, value + 2)],
        ],
        Family::WriteSkew => vec![
            vec![Op::Point(b), Op::Set(a, 0)],
            vec![Op::Point(a), Op::Set(b, 0)],
        ],
        // A reader sees both old values while two disjoint writers overlap it.
        // A serial order with the reader first is valid; do not equate commit
        // order with serialization order or demand that the reader abort.
        Family::ReadOnly => vec![
            vec![Op::Point(a), Op::Point(b)],
            vec![Op::Point(a), Op::Set(a, value + 1)],
            vec![Op::Point(b), Op::Set(b, value + 2)],
        ],
        Family::Phantom => vec![
            vec![Op::LabelScan, Op::NativeMatch, Op::Set(a, value + 1)],
            vec![
                Op::Point(a),
                Op::Create(VId(10), value + 3, true),
                Op::NativeMatch,
            ],
        ],
        Family::DeleteUpdate => vec![
            vec![Op::Point(a), Op::Delete(a), Op::Point(a)],
            vec![Op::Point(a), Op::Set(a, value + 1), Op::Point(a)],
        ],
    };
    let count = (2 + seed % 3) as usize;
    while programs.len() < count {
        let id = VId(20 + programs.len() as u128);
        programs.push(vec![
            Op::Point(id),
            Op::Create(id, value + 5, false),
            Op::Label(id, true),
            Op::NativeMatch,
        ]);
    }
    programs
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[derive(Default, Debug)]
struct Counts {
    admitted: usize,
    refused: usize,
    readers: usize,
}

fn run_seed(seed: u64) {
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let query = contexts.query();
        for family in FAMILIES {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let value = 1 + i64::try_from(seed % 31).unwrap();
            let initial: State = [
                (VId(1), node(value, false)),
                (VId(2), node(value, false)),
                (VId(3), node(value + 1, true)),
            ]
            .into_iter()
            .collect();
            let mut batch = WriteBatch::new(R);
            for (id, n) in &initial {
                batch.create_vertex(*id, n.labels.clone(), n.props.clone());
            }
            db.write(&commit, batch).await.unwrap();
            let scripts = programs(family, seed, value);
            let mut txns: Vec<_> = (0..scripts.len())
                .map(|_| db.begin(&txn_cx).unwrap())
                .collect();
            let basis = db.frontier().unwrap();
            assert!(txns.iter().all(|t| t.basis() == basis));
            let mut histories = vec![Vec::new(); scripts.len()];
            let mut private = vec![initial.clone(); scripts.len()];
            // Explicit generated round-robin interleaving: all handles overlap.
            let offset = seed as usize % scripts.len();
            for step in 0..scripts.iter().map(Vec::len).max().unwrap() {
                for turn in 0..scripts.len() {
                    let index = (turn + offset) % scripts.len();
                    if let Some(op) = scripts[index].get(step) {
                        let observed = execute(&mut txns[index], &mut db, &query, op);
                        assert_eq!(
                            interpret(&mut private[index], op),
                            Some(observed.clone()),
                            "overlay seed={seed:#x} family={family:?} txn={index} op={op:?}"
                        );
                        histories[index].push(Event {
                            op: op.clone(),
                            observed,
                        });
                    }
                }
            }
            assert_eq!(state(&db), initial, "staging must remain private");
            let mut order: Vec<_> = (0..scripts.len()).collect();
            order.rotate_left(offset);
            if seed & 1 != 0 {
                order.reverse();
            }
            let mut committed = Vec::new();
            let mut counts = Counts::default();
            for index in &order {
                let before = state(&db);
                let frontier = db.frontier().unwrap();
                let outcome = txns[*index].finish(&mut db, &commit).await;
                match outcome {
                    Ok(EmbeddedTxnCompletion::WriteCommitted { .. }) => {
                        committed.push(*index);
                        counts.admitted += 1;
                    }
                    Ok(EmbeddedTxnCompletion::ReadClosed { .. }) => {
                        committed.push(*index);
                        counts.admitted += 1;
                        counts.readers += 1;
                        assert_eq!(state(&db), before);
                        assert_eq!(db.frontier().unwrap(), frontier);
                    }
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law, .. })) => {
                        assert!(matches!(law, "FG-LAW-FCW-01" | "FG-LAW-FCW-READ-01"));
                        counts.refused += 1;
                        assert_eq!(state(&db), before, "refused effects leaked");
                        assert_eq!(
                            db.frontier().unwrap(),
                            frontier,
                            "refusal consumed sequence"
                        );
                    }
                    Err(error) => panic!(
                        "unexpected refusal seed={seed:#x} family={family:?} txn={index}: {error:?}"
                    ),
                }
                let actual = state(&db);
                assert!(
                    serializable(&initial, &histories, &committed, &actual),
                    "NON_SERIALIZABLE seed={seed:#x} family={family:?} order={order:?} committed={committed:?} histories={histories:?} final={actual:?}"
                );
            }
            assert_eq!(counts.admitted + counts.refused, scripts.len());
            assert!(counts.admitted > 0, "all-refused vacuity: {family:?}");
            if matches!(
                family,
                Family::LostUpdate | Family::WriteSkew | Family::DeleteUpdate
            ) {
                // Both primary txns read/write conflicting elements. The exact
                // suffix-validation rule above requires at least one refusal.
                assert!(counts.refused > 0, "missing conflict family={family:?}");
            }
            if matches!(family, Family::Phantom) {
                assert!(private[1].contains_key(&VId(10)));
                assert!(!initial.contains_key(&VId(10)));
            }
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            let before = state(&db);
            let frontier = db.frontier().unwrap();
            drop(txns);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                state(&reopened),
                before,
                "reopen seed={seed:#x} family={family:?}"
            );
            assert_eq!(reopened.frontier().unwrap(), frontier);
            println!("seed={seed:#x} family={family:?} order={order:?} {counts:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn checker_rejects_nonserializable_reads_and_wrong_final_state() {
    let initial: State = [(VId(1), node(1, false)), (VId(2), node(1, false))]
        .into_iter()
        .collect();
    let histories = vec![
        vec![
            Event {
                op: Op::Point(VId(2)),
                observed: Some(Observation::Point(Some(node(1, false)))),
            },
            Event {
                op: Op::Set(VId(1), 0),
                observed: None,
            },
        ],
        vec![
            Event {
                op: Op::Point(VId(1)),
                observed: Some(Observation::Point(Some(node(1, false)))),
            },
            Event {
                op: Op::Set(VId(2), 0),
                observed: None,
            },
        ],
    ];
    let impossible: State = [(VId(1), node(0, false)), (VId(2), node(0, false))]
        .into_iter()
        .collect();
    assert!(
        !serializable(&initial, &histories, &[0, 1], &impossible),
        "checker accepted write skew"
    );
    let mut valid = initial.clone();
    valid.insert(VId(1), node(0, false));
    assert!(serializable(&initial, &histories, &[0], &valid));
    assert!(
        !serializable(&initial, &histories, &[0], &impossible),
        "checker ignored final state"
    );
    // Legal serial order need not be the supplied completion order.
    let sequential = vec![
        histories[0].clone(),
        vec![
            Event {
                op: Op::Point(VId(1)),
                observed: Some(Observation::Point(Some(node(0, false)))),
            },
            Event {
                op: Op::Set(VId(2), 0),
                observed: None,
            },
        ],
    ];
    assert!(serializable(&initial, &sequential, &[1, 0], &impossible));
}

#[test]
fn anomaly_seed_7710() {
    run_seed(0x7710);
}
#[test]
fn anomaly_seed_7711() {
    run_seed(0x7711);
}
#[test]
fn anomaly_seed_7712() {
    run_seed(0x7712);
}
#[test]
fn anomaly_seed_7713() {
    run_seed(0x7713);
}
#[test]
fn anomaly_seed_7714() {
    run_seed(0x7714);
}
#[test]
fn anomaly_seed_7715() {
    run_seed(0x7715);
}
