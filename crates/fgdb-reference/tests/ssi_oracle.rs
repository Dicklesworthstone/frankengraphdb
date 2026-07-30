//! Laws of the SSI oracle — and the delivery of a promise made in
//! `transaction_anomalies.rs`.
//!
//! That file asserts write skew is ADMITTED under snapshot isolation, and says:
//! "when SSI lands, that same history must be REFUSED". This is where that
//! happens. The first law below builds the identical history through the same
//! transaction API and shows the SSI oracle flags it — so the two files together
//! are an executable statement of exactly what SSI buys over SI, rather than a
//! claim about it.
//!
//! **THE HARD DIRECTION IS NOT "IT CATCHES WRITE SKEW".** A rule that flagged
//! every concurrent read-write pair would catch write skew and be useless: it
//! would refuse serializable histories wholesale. The discriminating law is
//! `a_single_rw_edge_is_not_dangerous` — one antidependency, no cycle, must NOT
//! be flagged. Two consecutive rw edges is the whole content of the Fekete
//! theorem, and a checker that ignores the "consecutive" part still passes every
//! anomaly law here.
//!
//! Every history is built by running real transactions against a real
//! `ReferenceDatabase` and taking their traces, not by hand-writing read and
//! write sets. Hand-written sets would test the graph algorithm against my
//! understanding of what a transaction reads, which is the thing most likely to
//! be wrong.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::intents::{Intent, Statement};
use fgdb_reference::ssi::{DangerousStructure, TxnTrace, dangerous_structures};
use fgdb_reference::txn::{Transaction, TxnOutcome};
use fgdb_reference::{ConflictKey, ReferenceDatabase, ssi};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};
use std::collections::BTreeSet;

const GRAPH: GraphId = GraphId(1);
const MAIN: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const OTHER_REL: RelationId = RelationId(2);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);
const SEMANTICS: ObjectId = ObjectId([0x11; 32]);

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

fn create(vid: u128, value: i64) -> Statement {
    Statement::new(vec![Intent::CreateVertex {
        vid: VId(vid),
        labels: vec![LABEL],
        props: vec![(PROP, int(value))],
    }])
}

fn set(vid: u128, value: i64) -> Statement {
    Statement::new(vec![Intent::SetProp {
        elem: ElementId::Vertex(VId(vid)),
        name: PROP,
        value: int(value),
    }])
}

fn add_edge(eid: u128, src: u128, relation: RelationId, dst: u128) -> Statement {
    Statement::new(vec![Intent::AddEdge {
        eid: EId(eid),
        src: VId(src),
        etype: relation,
        dst: VId(dst),
        props: vec![],
    }])
}

fn delete_edge(eid: u128) -> Statement {
    Statement::new(vec![Intent::DeleteEdge { eid: EId(eid) }])
}

fn delete_vertex(vid: u128) -> Statement {
    Statement::new(vec![Intent::DeleteVertex { vid: VId(vid) }])
}

/// Two vertices at 1, committed at sequence 1.
fn seeded() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    let mut txn = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    txn.execute(&[create(1, 1), create(2, 1)])
        .expect("executes");
    commit(&mut db, txn, 1, 0);
    db
}

/// Commit `txn` at `seq` and return its trace under `id`.
///
/// The trace is taken before the commit, since committing consumes the
/// transaction, and marked committed from the OUTCOME rather than from the
/// requested sequence — a transaction that was refused must not produce a trace
/// claiming it landed.
fn commit(
    db: &mut ReferenceDatabase,
    txn: Transaction,
    seq: u64,
    id: usize,
) -> (TxnOutcome, TxnTrace) {
    let trace = txn.trace(id);
    let outcome = txn
        .commit(db, REL, SEMANTICS, CommitSeq(seq))
        .expect("commit should not error");
    let trace = match outcome.committed_parts() {
        Some((commit_seq, _, _)) => trace.committed_at(commit_seq),
        None => trace,
    };
    (outcome, trace)
}

/// THE PROMISE FROM `transaction_anomalies.rs`, DELIVERED.
///
/// The same history — two transactions read the pair, each writes the other's
/// element, both commit under SI — has a dangerous structure, so SSI refuses what
/// SI admitted. The pivot is the second committer, with its incoming and outgoing
/// edges both from the first: the two-transaction write-skew shape, which is why
/// the checker must allow `incoming_from == outgoing_to`.
#[test]
fn write_skew_has_a_dangerous_structure() {
    let mut db = seeded();
    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    // Each reads the whole invariant through TRACKED reads.
    for vid in [1, 2] {
        assert_eq!(
            t1.read_property(ElementId::Vertex(VId(vid)), PROP),
            Some(int(1))
        );
        assert_eq!(
            t2.read_property(ElementId::Vertex(VId(vid)), PROP),
            Some(int(1))
        );
    }
    t1.execute(&[set(1, 0)]).expect("executes");
    t2.execute(&[set(2, 0)]).expect("executes");

    let (first, trace1) = commit(&mut db, t1, 2, 1);
    let (second, trace2) = commit(&mut db, t2, 3, 2);
    assert!(
        first.is_committed() && second.is_committed(),
        "SI must admit this history — otherwise SSI has nothing to add"
    );

    let history = vec![trace1, trace2];
    assert_eq!(
        dangerous_structures(&history),
        vec![DangerousStructure {
            pivot: 2,
            incoming_from: 1,
            outgoing_to: 1,
        }],
        "SSI must refuse the history SI admitted"
    );
    assert!(!ssi::is_provably_serializable(&history));
}

/// THE DISCRIMINATING LAW: a single rw edge is not dangerous.
///
/// One transaction reads v1, another writes v1 concurrently and commits first.
/// There is an antidependency, there is no cycle, and the history is
/// serializable — the reader simply orders before the writer. A checker that
/// flagged this would refuse serializable work wholesale while still passing
/// every anomaly law in this file.
#[test]
fn a_single_rw_edge_is_not_dangerous() {
    let mut db = seeded();
    let mut reader = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut writer = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    reader.read_property(ElementId::Vertex(VId(1)), PROP);
    // The reader also writes, so it is not trivially edgeless — but it writes
    // something nobody reads.
    reader.execute(&[set(2, 5)]).expect("executes");
    writer.execute(&[set(1, 9)]).expect("executes");

    let (w, trace_w) = commit(&mut db, writer, 2, 1);
    let (r, trace_r) = commit(&mut db, reader, 3, 2);
    assert!(w.is_committed() && r.is_committed());

    let history = vec![trace_w, trace_r];
    assert_eq!(
        dangerous_structures(&history),
        vec![],
        "one antidependency is not a cycle"
    );
    assert!(ssi::is_provably_serializable(&history));
}

/// A serial history — each transaction begins after the previous committed — has
/// no antidependencies at all, so nothing to flag. Without this law a checker
/// that ignored the concurrency test would pass everything else.
#[test]
fn a_serial_history_has_no_dangerous_structure() {
    let mut db = seeded();
    let mut traces = Vec::new();
    for (id, seq) in [(1usize, 2u64), (2, 3), (3, 4)] {
        let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
        txn.read_property(ElementId::Vertex(VId(1)), PROP);
        txn.read_property(ElementId::Vertex(VId(2)), PROP);
        txn.execute(&[set(1, seq as i64), set(2, seq as i64)])
            .expect("executes");
        let (outcome, trace) = commit(&mut db, txn, seq, id);
        assert!(outcome.is_committed(), "serial commits never conflict");
        traces.push(trace);
    }
    assert_eq!(dangerous_structures(&traces), vec![]);
}

/// Concurrent transactions that neither read nor write anything in common have
/// no edges.
#[test]
fn disjoint_concurrent_transactions_have_no_dangerous_structure() {
    let mut db = seeded();
    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    t1.read_property(ElementId::Vertex(VId(1)), PROP);
    t2.read_property(ElementId::Vertex(VId(2)), PROP);
    t1.execute(&[set(1, 7)]).expect("executes");
    t2.execute(&[set(2, 8)]).expect("executes");

    let (_, trace1) = commit(&mut db, t1, 2, 1);
    let (_, trace2) = commit(&mut db, t2, 3, 2);
    assert_eq!(dangerous_structures(&[trace1, trace2]), vec![]);
}

/// A transaction that did not commit forms no edges — asserted where it is the
/// ONLY incoming endpoint, so nothing else can account for the all-clear.
///
/// THIS LAW WAS WEAKER THAN IT READ. The first version put the refused
/// transaction at both endpoints of the structure, where the pivot loop's own
/// committed() check already excluded it — so the edge rule was never exercised
/// and letting uncommitted transactions form edges passed the test. Here the
/// refused transaction can only be the incoming endpoint, and if its edge counted,
/// P would be a pivot.
///
/// A history is a record of what HAPPENED. Counting a refused transaction's
/// footprint would report anomalies in histories that are serializable precisely
/// because the transaction was refused — the very outcome first-committer-wins
/// exists to produce.
#[test]
fn an_uncommitted_transaction_forms_no_edges() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    // Three transactions, all reading at sequence 1.
    let mut out = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut pivot = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut refused = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    out.execute(&[set(2, 10)]).expect("executes");
    pivot.read_property(ElementId::Vertex(VId(2)), PROP);
    pivot.execute(&[set(1, 20)]).expect("executes");
    refused.read_property(ElementId::Vertex(VId(1)), PROP);
    // Writing v2 puts it in conflict with `out`, so SI refuses it.
    refused.execute(&[set(2, 30)]).expect("executes");

    let (o, trace_out) = commit(&mut db, out, 2, 3);
    let (p, trace_pivot) = commit(&mut db, pivot, 3, 2);
    let (r, trace_refused) = commit(&mut db, refused, 4, 1);
    assert!(o.is_committed() && p.is_committed());
    assert!(r.conflicts().is_some(), "SI must refuse the third");
    assert_eq!(trace_refused.commit_seq, None);

    // The pivot's OUT edge is real: it read v2, which `out` wrote concurrently
    // and committed before it. Only the IN edge is missing, and only because the
    // transaction that would supply it never committed.
    assert_eq!(
        dangerous_structures(&[
            trace_out.clone(),
            trace_pivot.clone(),
            trace_refused.clone()
        ]),
        vec![],
        "a refused transaction is not part of the history"
    );
    // Control: the SAME three traces with the refused one marked committed DO
    // form the structure, so the all-clear above is about commitment and not
    // about the history being edgeless.
    let as_if = vec![
        trace_out,
        trace_pivot,
        trace_refused.committed_at(CommitSeq(4)),
    ];
    // TWO structures, and the second one is instructive: the refused transaction
    // and the pivot are themselves a write-skew pair (each reads what the other
    // writes), so counting the refused transaction manufactures an anomaly
    // between them as well as completing the three-transaction chain. Both are
    // absent above for the same reason.
    assert_eq!(
        dangerous_structures(&as_if),
        vec![
            DangerousStructure {
                pivot: 1,
                incoming_from: 2,
                outgoing_to: 2,
            },
            DangerousStructure {
                pivot: 2,
                incoming_from: 1,
                outgoing_to: 3,
            },
        ],
        "the structures are there the moment the third transaction counts"
    );
}

/// An antidependency needs the write to be INVISIBLE to the reader. A read of
/// something written before the reader's snapshot is an ordinary dependency, and
/// counting it would make every history dangerous.
#[test]
fn a_visible_write_is_not_an_antidependency() {
    let mut db = seeded();

    // Sequence 2 writes v1, and commits before anyone else begins.
    let mut early = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    early.execute(&[set(1, 50)]).expect("executes");
    let (_, trace_early) = commit(&mut db, early, 2, 1);

    // Two concurrent transactions now both READ v1 — seeing the committed value
    // — and write different things. Neither read is an antidependency to `early`.
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t3 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    assert_eq!(
        t2.read_property(ElementId::Vertex(VId(1)), PROP),
        Some(int(50))
    );
    t3.read_property(ElementId::Vertex(VId(1)), PROP);
    // 7, not 1: v2 already HOLDS 1, so the old value made this a no-op — which
    // commits nothing and consumes no sequence, leaving the next hardcoded
    // sequence one ahead of the stream. A no-op transaction is a legitimate
    // outcome; a fixture that assumed it lands is not.
    t2.execute(&[set(2, 7)]).expect("executes");
    t3.execute(&[create(3, 1)]).expect("executes");

    let (_, trace2) = commit(&mut db, t2, 3, 2);
    let (_, trace3) = commit(&mut db, t3, 4, 3);
    assert_eq!(dangerous_structures(&[trace_early, trace2, trace3]), vec![]);
}

/// THREE-TRANSACTION PIVOT: the shape the two-transaction case cannot exhibit.
///
/// T1 reads v1 and writes v2; T2 (the pivot) reads v2 and writes v3; T3 reads v3
/// and writes v1. Each read is of an element a concurrent transaction wrote, so
/// the rw edges chain, and the pivot has an incoming edge from one transaction
/// and an outgoing edge to another — `incoming_from != outgoing_to`, unlike write
/// skew.
#[test]
fn a_three_transaction_pivot_is_found() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t3 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    t1.read_property(ElementId::Vertex(VId(1)), PROP);
    t2.read_property(ElementId::Vertex(VId(2)), PROP);
    t3.read_property(ElementId::Vertex(VId(3)), PROP);
    t1.execute(&[set(2, 10)]).expect("executes");
    t2.execute(&[set(3, 20)]).expect("executes");
    t3.execute(&[set(1, 30)]).expect("executes");

    // Commit order 3, 2, 1 so that each pivot candidate's out-edge target
    // committed before it.
    let (_, trace3) = commit(&mut db, t3, 2, 3);
    let (_, trace2) = commit(&mut db, t2, 3, 2);
    let (_, trace1) = commit(&mut db, t1, 4, 1);

    // The EXACT set, not merely "something was found". Asserting existence
    // cannot distinguish the correct edge direction from an inverted one: the
    // pivot condition (an in-edge and an out-edge) is symmetric under inversion,
    // and only Cahill's commit-order refinement breaks the tie — so only the
    // identity of the named pivot pins the direction.
    assert_eq!(
        dangerous_structures(&[trace1, trace2, trace3]),
        vec![DangerousStructure {
            pivot: 1,
            incoming_from: 2,
            outgoing_to: 3,
        }],
        "t1 is the pivot: t2 read what t1 wrote, and t1 read what t3 wrote"
    );
}

/// The report names the pivot and both endpoints, because "this history is
/// dangerous" is not actionable and "transaction 2 is the pivot between 1 and 3"
/// is — a real SSI implementation has to choose a victim.
#[test]
fn the_report_names_the_pivot_and_both_endpoints() {
    let mut db = seeded();
    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    t1.read_property(ElementId::Vertex(VId(2)), PROP);
    t2.read_property(ElementId::Vertex(VId(1)), PROP);
    t1.execute(&[set(1, 0)]).expect("executes");
    t2.execute(&[set(2, 0)]).expect("executes");

    let (_, trace1) = commit(&mut db, t1, 2, 7);
    let (_, trace2) = commit(&mut db, t2, 3, 9);

    let found = dangerous_structures(&[trace1, trace2]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pivot, 9, "the later committer is the pivot");
    assert_eq!((found[0].incoming_from, found[0].outgoing_to), (7, 7));
}

/// An UNTRACKED read creates no dependency, and that is deliberate: a test
/// inspecting the workspace must not change the history it is checking.
///
/// This law exists so the distinction is a pinned property rather than a comment.
/// It is also the hazard note: modelling a transaction's dependencies through
/// `workspace()` would silently produce empty read sets and an all-clear from the
/// oracle.
#[test]
fn an_untracked_read_creates_no_dependency() {
    let mut db = seeded();
    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    // The same write-skew history as the first law, but read through the
    // untracked view.
    assert!(t1.workspace().vertex(VId(2)).is_some());
    assert!(t2.workspace().vertex(VId(1)).is_some());
    assert!(t1.read_set().is_empty() && t2.read_set().is_empty());

    t1.execute(&[set(1, 0)]).expect("executes");
    t2.execute(&[set(2, 0)]).expect("executes");
    let (_, trace1) = commit(&mut db, t1, 2, 1);
    let (_, trace2) = commit(&mut db, t2, 3, 2);

    assert_eq!(
        dangerous_structures(&[trace1, trace2]),
        vec![],
        "no reads were recorded, so there is no rw edge to find"
    );
}

/// A tracked neighbour read records exactly its positive and negative
/// dependencies: the source vertex, matching outgoing edges, and the
/// relation-qualified adjacency predicate even when the result is empty.
#[test]
fn a_neighbour_read_records_the_edges_it_traversed() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[
        create(1, 1),
        create(2, 1),
        create(3, 1),
        add_edge(50, 1, REL, 2),
        add_edge(51, 1, OTHER_REL, 3),
        add_edge(52, 3, REL, 1),
    ])
    .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    assert_eq!(txn.read_neighbours(VId(1), REL), vec![VId(2)]);
    assert!(
        txn.read_set()
            .contains(&ConflictKey::Element(ElementId::Edge(EId(50)))),
        "the traversed edge is a dependency: {:?}",
        txn.read_set()
    );
    assert!(
        txn.read_set()
            .contains(&ConflictKey::Element(ElementId::Vertex(VId(1))))
    );
    assert!(
        !txn.read_set()
            .contains(&ConflictKey::Element(ElementId::Edge(EId(51)))),
        "another relation is not traversed: {:?}",
        txn.read_set()
    );
    assert!(
        !txn.read_set()
            .contains(&ConflictKey::Element(ElementId::Edge(EId(52)))),
        "an incoming edge is not traversed: {:?}",
        txn.read_set()
    );
    assert!(
        txn.read_set().contains(&ConflictKey::Adjacency {
            vertex: VId(1),
            relation: REL,
        }),
        "the predicate itself is a dependency: {:?}",
        txn.read_set()
    );
}

/// A reader that observes an empty adjacency and a concurrent inserter now form
/// the same two-rw-edge dangerous structure as element-level write skew.
///
/// The first edge is the formerly missing phantom: `reader --rw--> inserter`
/// because the inserter changed the predicate the reader observed as empty. The
/// return edge is element-grained: the inserter read v2 and the reader wrote it.
#[test]
fn an_empty_adjacency_read_and_insert_complete_a_dangerous_structure() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut reader = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut inserter = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    assert_eq!(reader.read_neighbours(VId(1), REL), vec![]);
    reader.execute(&[set(2, 2)]).expect("executes");
    assert_eq!(
        inserter.read_property(ElementId::Vertex(VId(2)), PROP),
        Some(int(1))
    );
    inserter
        .execute(&[add_edge(50, 1, REL, 3)])
        .expect("executes");

    let (read_outcome, reader_trace) = commit(&mut db, reader, 2, 1);
    let (insert_outcome, inserter_trace) = commit(&mut db, inserter, 3, 2);
    assert!(
        read_outcome.is_committed() && insert_outcome.is_committed(),
        "SI admits disjoint ordinary writes"
    );
    let adjacency = ConflictKey::Adjacency {
        vertex: VId(1),
        relation: REL,
    };
    assert!(reader_trace.reads.contains(&adjacency));
    assert!(inserter_trace.writes.contains(&adjacency));
    assert_eq!(
        dangerous_structures(&[reader_trace, inserter_trace]),
        vec![DangerousStructure {
            pivot: 2,
            incoming_from: 1,
            outgoing_to: 1,
        }]
    );
}

/// Predicate writes are SSI evidence, not SI first-committer-wins domains.
///
/// Both transactions change the same logical adjacency, but neither read it and
/// their edge identities differ. They must both commit and form no rw edge. This
/// is also the mutation control: moving `Adjacency` into
/// `CertificationSummary` makes the second commit fail.
#[test]
fn two_inserters_into_one_adjacency_both_commit_under_si() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut left = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut right = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    left.execute(&[add_edge(50, 1, REL, 2)]).expect("executes");
    right.execute(&[add_edge(51, 1, REL, 3)]).expect("executes");

    let (left_outcome, left_trace) = commit(&mut db, left, 2, 1);
    let (right_outcome, right_trace) = commit(&mut db, right, 3, 2);
    assert!(
        left_outcome.is_committed() && right_outcome.is_committed(),
        "same-adjacency writes are independent under SI"
    );
    let adjacency = ConflictKey::Adjacency {
        vertex: VId(1),
        relation: REL,
    };
    assert!(left_trace.writes.contains(&adjacency));
    assert!(right_trace.writes.contains(&adjacency));
    assert_eq!(dangerous_structures(&[left_trace, right_trace]), vec![]);
}

/// Adjacency predicates are relation-qualified. A write through another
/// relation supplies no phantom edge, leaving only the real element-level rw
/// edge and therefore no dangerous structure.
#[test]
fn another_relation_does_not_satisfy_the_adjacency_predicate() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut reader = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut inserter = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    assert_eq!(reader.read_neighbours(VId(1), OTHER_REL), vec![]);
    reader.execute(&[set(2, 2)]).expect("executes");
    inserter.read_property(ElementId::Vertex(VId(2)), PROP);
    inserter
        .execute(&[add_edge(50, 1, REL, 3)])
        .expect("executes");

    let (read_outcome, reader_trace) = commit(&mut db, reader, 2, 1);
    let (insert_outcome, inserter_trace) = commit(&mut db, inserter, 3, 2);
    assert!(read_outcome.is_committed() && insert_outcome.is_committed());
    assert!(reader_trace.reads.contains(&ConflictKey::Adjacency {
        vertex: VId(1),
        relation: OTHER_REL,
    }));
    assert!(inserter_trace.writes.contains(&ConflictKey::Adjacency {
        vertex: VId(1),
        relation: REL,
    }));
    assert_eq!(
        dangerous_structures(&[reader_trace, inserter_trace]),
        vec![],
        "different relations cannot manufacture the missing rw edge"
    );
}

/// A delete row carries only the edge identity, so its SSI predicate witness
/// must be recovered from the pre-effect workspace.
#[test]
fn deleting_an_edge_writes_its_source_adjacency_predicate() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), add_edge(50, 1, REL, 2)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut deletion = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    deletion.execute(&[delete_edge(50)]).expect("executes");
    let (outcome, trace) = commit(&mut db, deletion, 2, 1);
    assert!(outcome.is_committed());
    assert!(trace.writes.contains(&ConflictKey::Adjacency {
        vertex: VId(1),
        relation: REL,
    }));
}

/// A vertex deletion retires both incoming and outgoing edges. Every retired
/// edge changes its SOURCE adjacency, not necessarily the deleted vertex's
/// adjacency, and relations remain distinct.
#[test]
fn a_vertex_delete_cascade_writes_every_retired_source_adjacency() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[
        create(1, 1),
        create(2, 1),
        create(3, 1),
        add_edge(50, 1, REL, 2),
        add_edge(51, 3, OTHER_REL, 2),
        add_edge(52, 2, REL, 3),
    ])
    .expect("executes");
    commit(&mut db, seed, 1, 0);

    let mut deletion = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    deletion.execute(&[delete_vertex(2)]).expect("executes");
    let (outcome, trace) = commit(&mut db, deletion, 2, 1);
    assert!(outcome.is_committed());

    let actual: BTreeSet<_> = trace
        .writes
        .iter()
        .filter(|key| matches!(key, ConflictKey::Adjacency { .. }))
        .copied()
        .collect();
    assert_eq!(
        actual,
        BTreeSet::from([
            ConflictKey::Adjacency {
                vertex: VId(1),
                relation: REL,
            },
            ConflictKey::Adjacency {
                vertex: VId(2),
                relation: REL,
            },
            ConflictKey::Adjacency {
                vertex: VId(3),
                relation: OTHER_REL,
            },
        ])
    );
}

/// The result is a function of the history, not of the order the traces were
/// listed in. Doctrine 4 applies to a verification tool too — a checker whose
/// answer depended on argument order could not be replayed.
///
/// Over a history with TWO structures, because one result has no order to get
/// wrong: the first version of this law used a single write-skew pair and passed
/// against an implementation that reported in raw iteration order.
#[test]
fn the_report_is_order_independent() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1), create(3, 1), create(4, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    // Two independent write-skew pairs: (v1, v2) and (v3, v4).
    let mut traces = Vec::new();
    let mut pending = Vec::new();
    for (id, pair) in [
        (1usize, (1u128, 2u128)),
        (2, (2, 1)),
        (3, (3, 4)),
        (4, (4, 3)),
    ] {
        let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
        txn.read_property(ElementId::Vertex(VId(pair.0)), PROP);
        txn.read_property(ElementId::Vertex(VId(pair.1)), PROP);
        txn.execute(&[set(pair.0, 0)]).expect("executes");
        pending.push((id, txn));
    }
    for (offset, (id, txn)) in pending.into_iter().enumerate() {
        let (outcome, trace) = commit(&mut db, txn, 2 + offset as u64, id);
        assert!(outcome.is_committed(), "the pairs write disjoint elements");
        traces.push(trace);
    }

    let forward = dangerous_structures(&traces);
    assert_eq!(
        forward.len(),
        2,
        "two independent pairs, two pivots: {forward:?}"
    );
    traces.reverse();
    assert_eq!(forward, dangerous_structures(&traces));
}

/// An empty history is provably serializable — the vacuous case answered
/// explicitly, since a checker that returned "dangerous" on no input would be
/// caught by nothing else here.
#[test]
fn an_empty_history_is_provably_serializable() {
    assert_eq!(dangerous_structures(&[]), vec![]);
    assert!(ssi::is_provably_serializable(&[]));
}

/// A transaction that FINISHED BEFORE another began forms no edge with it, even
/// though the later transaction wrote something the earlier one read.
///
/// THE CLAUSE THIS GUARDS, and why it is not obvious. "The reader could not see
/// the write" (`writer.commit > reader.snapshot`) is satisfied here for the
/// innocent reason that the writer did not exist yet — so that clause alone
/// admits an edge between two transactions that never overlapped. Overlap needs
/// the second clause, `reader.commit > writer.snapshot`.
///
/// The history below is serializable: it has exactly ONE real antidependency
/// (pivot reads v2, which `early_out` wrote concurrently), and one edge is never
/// a cycle. With only the first clause, the spurious edge from `finished` supplies
/// the pivot's missing in-edge and the whole thing is falsely flagged — and
/// Cahill's refinement does NOT discard it, because `early_out` genuinely
/// committed first. This law is why that clause exists.
#[test]
fn a_transaction_that_finished_before_another_began_forms_no_edge() {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    seed.execute(&[create(1, 1), create(2, 1)])
        .expect("executes");
    commit(&mut db, seed, 1, 0);

    // `finished` reads v1 and commits at 2, entirely before the other two begin.
    let mut finished = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    finished.read_property(ElementId::Vertex(VId(1)), PROP);
    finished.execute(&[create(3, 1)]).expect("executes");
    let (f, trace_finished) = commit(&mut db, finished, 2, 1);
    assert!(f.is_committed());

    // Now two genuinely concurrent transactions, both reading at sequence 2.
    let mut pivot = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut early_out = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    pivot.read_property(ElementId::Vertex(VId(2)), PROP);
    // The pivot writes v1 — which `finished` READ, so clause one admits an edge.
    pivot.execute(&[set(1, 5)]).expect("executes");
    early_out.execute(&[set(2, 6)]).expect("executes");

    let (e, trace_early) = commit(&mut db, early_out, 3, 3);
    let (p, trace_pivot) = commit(&mut db, pivot, 4, 2);
    assert!(e.is_committed() && p.is_committed(), "disjoint writes");

    let history = vec![trace_finished, trace_pivot, trace_early];
    assert_eq!(
        dangerous_structures(&history),
        vec![],
        "one real antidependency and one non-overlap is not a cycle"
    );
    assert!(ssi::is_provably_serializable(&history));
}
