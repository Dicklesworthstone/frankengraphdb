//! The SSI oracle: reconstruct the rw-dependency graph from traces and report
//! any committed dangerous structure.
//!
//! §15 asks for exactly this, and for a specific reason: "our own cycle detection
//! verifies our own serialization graphs". The check here is not an admission
//! rule bolted onto [`crate::txn`] — it is a *checker over a history*, so a real
//! transaction manager's decisions can be replayed against it after the fact.
//! That is the only way to test admission control: an implementation that both
//! makes and grades its own decisions agrees with itself by construction.
//!
//! **THE THEOREM THIS RESTS ON** (Fekete, Liarokapis, O'Neil, O'Neil, Shasha,
//! 2005): every non-serializable execution under snapshot isolation contains a
//! cycle in the dependency graph with **two consecutive rw-antidependency
//! edges**. So it suffices to look for a *pivot* — one transaction with both an
//! incoming and an outgoing rw edge — rather than to enumerate cycles. Cahill's
//! implementation adds the refinement this module also uses: the transaction the
//! pivot points AT must commit first, because a structure whose out-edge target
//! has not committed yet cannot have closed a cycle.
//!
//! **AN RW-ANTIDEPENDENCY, PRECISELY.** `A --rw--> B` when A read an item B
//! wrote, and A could not have seen that write — i.e. B committed after A's
//! snapshot was taken, so the two are concurrent. Direction matters and is easy
//! to invert: the edge runs from the READER to the WRITER, because the reader
//! must be ordered *before* the writer in any equivalent serial history.
//!
//! **THIS CHECK IS CONSERVATIVE, AND THAT IS BY DESIGN.** Two consecutive rw
//! edges are *necessary* for a non-serializable SI execution, not sufficient. A
//! reported structure therefore means "not provably serializable", never
//! "definitely wrong", and a real SSI implementation aborting on it may abort a
//! history that was in fact serializable. Stating that here matters more than it
//! might seem: the temptation when a false positive turns up is to weaken the
//! rule until it goes away, which converts a conservative-and-correct checker
//! into an unsound one.
//!
//! **PREDICATE READS ARE LOGICAL, NOT PHYSICAL.**
//! [`crate::txn::Transaction::read_neighbours`] records a relation-qualified
//! adjacency domain even when the result is empty, and final edge creates or
//! deletes add that same domain only to the transaction TRACE write set. It is
//! deliberately absent from SI first-committer-wins certification: two writers
//! adding distinct edges at one hub remain independent, while either conflicts
//! with a reader that depended on the adjacency predicate. This is the reference
//! oracle's coarse logical witness; production witnesses retain §7.3's exact
//! ranges, gaps, generations, and refinement evidence.

use crate::ConflictKey;
use fgdb_types::CommitSeq;
use std::collections::BTreeSet;

/// One transaction's observable footprint.
///
/// Assembled by [`crate::txn::Transaction::trace`] and marked committed from the
/// outcome, so the sequence recorded here is the one the transaction actually
/// landed at rather than one the caller chose to claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnTrace {
    pub id: usize,
    /// The sequence this transaction read at.
    pub snapshot_high: CommitSeq,
    /// Where it committed, or `None` if it did not.
    ///
    /// An `Option` rather than a sentinel sequence: "did not commit" is not a
    /// point on the timeline, and a sentinel would sort somewhere and form edges.
    pub commit_seq: Option<CommitSeq>,
    /// Logical elements and predicates this transaction observed.
    pub reads: BTreeSet<ConflictKey>,
    /// Final logical effects, including trace-only predicate domains.
    pub writes: BTreeSet<ConflictKey>,
}

impl TxnTrace {
    /// Mark this trace as having committed at `seq`.
    pub fn committed_at(mut self, seq: CommitSeq) -> Self {
        self.commit_seq = Some(seq);
        self
    }

    fn committed(&self) -> Option<CommitSeq> {
        self.commit_seq
    }
}

/// A pivot with both an incoming and an outgoing rw edge.
///
/// `incoming_from` and `outgoing_to` may be the SAME transaction: that is the
/// two-transaction write-skew shape, where each of the pair reads what the other
/// writes. A checker that required three distinct transactions would miss the
/// single most common SI anomaly there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DangerousStructure {
    pub pivot: usize,
    pub incoming_from: usize,
    pub outgoing_to: usize,
}

/// Does `reader --rw--> writer` hold?
///
/// Both must have committed, they must be distinct, the writer must have written
/// something the reader read, and the two must be CONCURRENT — which takes two
/// clauses, not one:
///
/// * `writer.commit > reader.snapshot` — the reader could not see the write.
/// * `reader.commit > writer.snapshot` — the reader had not already finished
///   when the writer began.
///
/// **BOTH ARE LOAD-BEARING, and the second was missing here.** With only the
/// first, a strictly sequential history forms edges: a transaction reading at
/// sequence 1 and committing at 2 would antidepend on one that began at 2 and
/// committed at 3, purely because 3 > 1. Nothing about that pair is concurrent.
/// It was invisible because Cahill's refinement downstream happened to discard
/// the resulting structures — a *forward*-time chain never has its out-edge
/// target committing first — so the serial-history law passed for a reason that
/// had nothing to do with concurrency. Found by mutating the refinement and
/// watching a law fail that should not have depended on it.
///
/// A write the reader could already see is an ordinary wr dependency, not an
/// antidependency; overlap is what makes the reader have to be ordered *before*
/// the writer despite reading an older value.
fn antidependency(reader: &TxnTrace, writer: &TxnTrace) -> bool {
    if reader.id == writer.id {
        return false;
    }
    let (Some(reader_commit), Some(writer_commit)) = (reader.committed(), writer.committed())
    else {
        return false;
    };
    if writer_commit.0 <= reader.snapshot_high.0 || reader_commit.0 <= writer.snapshot_high.0 {
        return false;
    }
    reader.reads.intersection(&writer.writes).next().is_some()
}

/// Every dangerous structure in `history`, sorted.
///
/// Sorted so the result is a function of the history and not of iteration order
/// — doctrine 4 applies to a verification tool as much as to a query.
pub fn dangerous_structures(history: &[TxnTrace]) -> Vec<DangerousStructure> {
    let mut found = BTreeSet::new();
    for pivot in history {
        if pivot.committed().is_none() {
            continue;
        }
        for incoming in history {
            if !antidependency(incoming, pivot) {
                continue;
            }
            for outgoing in history {
                if !antidependency(pivot, outgoing) {
                    continue;
                }
                // CAHILL'S REFINEMENT: the out-edge target must commit before the
                // pivot. Without it every single rw edge between two concurrent
                // transactions reports twice — once with each as pivot — and the
                // check degenerates into "abort on any read-write conflict",
                // which refuses serializable histories wholesale.
                let (Some(pivot_commit), Some(out_commit)) =
                    (pivot.committed(), outgoing.committed())
                else {
                    continue;
                };
                if out_commit.0 >= pivot_commit.0 {
                    continue;
                }
                found.insert(DangerousStructure {
                    pivot: pivot.id,
                    incoming_from: incoming.id,
                    outgoing_to: outgoing.id,
                });
            }
        }
    }
    found.into_iter().collect()
}

/// Is this history free of committed dangerous structures?
///
/// The name says `provably` on purpose. `true` means the necessary condition for
/// an SI anomaly is absent, so the history IS serializable. `false` means only
/// that this check cannot rule an anomaly out — see the conservatism note in the
/// module docs. Calling it `is_serializable` would invert the strength of the
/// negative answer, which is the direction that matters.
pub fn is_provably_serializable(history: &[TxnTrace]) -> bool {
    dangerous_structures(history).is_empty()
}
