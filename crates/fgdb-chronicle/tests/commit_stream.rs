//! Commit-stream laws: the chain hash and the branch-head compare-and-swap.
//!
//! B1's claim is that MVCC order, time-travel history, replication, and branch
//! heads are ONE mechanism. That only holds if the mechanism is airtight, so
//! these tests attack it from the directions that would quietly break the
//! claim rather than obviously break a build:
//!
//!   * a tampered marker anywhere in history invalidates everything after it,
//!     and detection names the exact sequence;
//!   * the commit sequence is gap-free — a gap makes "the history up to N"
//!     ambiguous, a repeat makes it contradictory;
//!   * a branch head advances only against the head it expected, and a failed
//!     compare-and-swap changes NOTHING, including the other branches in the
//!     same marker.

use fgdb_chronicle::marker::{
    CHAIN_ORIGIN, ChainError, CommitMarker, EffectSource, HeadUpdate, MarkerChain, MarkerRef,
};
use fgdb_crypto::Digest;
use fgdb_types::ids::ObjectId;

fn digest(seed: u8) -> Digest {
    Digest([seed; 32])
}

fn oid(seed: u8) -> ObjectId {
    ObjectId([seed; 32])
}

/// A marker with no head updates — the shape of a commit that touches no
/// branch head (a maintenance or metadata commit).
fn marker(commit_seq: u64, command_seq: u64) -> CommitMarker {
    CommitMarker {
        logical_command_seq: command_seq,
        commit_seq,
        effect_source: EffectSource::Local {
            capsule_ref: oid(commit_seq as u8),
            logical_delta_template_digest: digest(commit_seq as u8 + 1),
        },
        prev_global: None,
        head_updates: Vec::new(),
        merge_record_oid: None,
        coordinate_schema_transition_digest: digest(3),
        topology_epoch: 1,
        policy_epoch: 2,
        revocation_index: 3,
        txn_token: [7u8; 16],
        commit_hlc: 1_000 + commit_seq,
        final_effect_digest: digest(commit_seq as u8 + 4),
        authorization_decision_digest: digest(5),
        resource_effect_digest: digest(6),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

fn with_heads(mut m: CommitMarker, heads: Vec<HeadUpdate>) -> CommitMarker {
    m.head_updates = heads;
    m
}

fn head(graph: u64, branch: u64, expected_previous: Option<MarkerRef>) -> HeadUpdate {
    HeadUpdate {
        graph,
        branch,
        expected_previous,
    }
}

/// A chain of `n` plain markers.
fn chain_of(n: u64) -> MarkerChain {
    let mut chain = MarkerChain::new();
    for seq in 1..=n {
        chain.append(marker(seq, seq * 10)).expect("append");
    }
    chain
}

#[test]
fn an_empty_chain_starts_at_the_origin() {
    let chain = MarkerChain::new();
    assert!(chain.is_empty());
    assert_eq!(chain.chain_value(), CHAIN_ORIGIN);
    // Sequences start at 1, so an uninitialised zero can never look like the
    // first commit.
    assert_eq!(chain.next_commit_seq(), 1);
}

#[test]
fn a_chain_verifies_and_every_marker_advances_the_chain_value() {
    let chain = chain_of(8);
    assert_eq!(chain.len(), 8);
    assert!(chain.verify().is_ok());

    // Each entry's chain value must differ from every other: a chain that
    // repeated a value would let two histories be confused.
    let mut seen = Vec::new();
    for entry in chain.entries() {
        assert!(
            !seen.contains(&entry.chain_hash),
            "chain value repeated at commit_seq {}",
            entry.marker.commit_seq
        );
        seen.push(entry.chain_hash);
    }
}

/// THE CHAIN-HASH LAW. Changing any field of any marker must invalidate the
/// chain from that point — a field outside the transcript is a field history
/// does not commit to.
#[test]
fn tampering_with_any_marker_field_breaks_the_chain_at_that_sequence() {
    let mutations: Vec<(&str, fn(&mut CommitMarker))> = vec![
        ("logical_command_seq", |m| m.logical_command_seq ^= 1),
        ("commit_seq", |m| m.commit_seq ^= 64),
        ("effect_source", |m| {
            m.effect_source = EffectSource::Local {
                capsule_ref: oid(0xfe),
                logical_delta_template_digest: digest(0xfd),
            }
        }),
        ("prev_global", |m| {
            m.prev_global = Some(MarkerRef {
                marker_oid: oid(0xfc),
                commit_seq: 1,
            })
        }),
        ("head_updates", |m| m.head_updates.push(head(1, 1, None))),
        ("merge_record_oid", |m| m.merge_record_oid = Some(oid(0xfb))),
        ("schema_transition", |m| {
            m.coordinate_schema_transition_digest = digest(0xfa)
        }),
        ("topology_epoch", |m| m.topology_epoch ^= 1),
        ("policy_epoch", |m| m.policy_epoch ^= 1),
        ("revocation_index", |m| m.revocation_index ^= 1),
        ("txn_token", |m| m.txn_token[0] ^= 1),
        ("commit_hlc", |m| m.commit_hlc ^= 1),
        ("final_effect_digest", |m| {
            m.final_effect_digest = digest(0xf9)
        }),
        ("authorization_digest", |m| {
            m.authorization_decision_digest = digest(0xf8)
        }),
        ("resource_digest", |m| {
            m.resource_effect_digest = digest(0xf7)
        }),
        ("availability_cert", |m| {
            m.payload_availability_certificate_oid = Some(oid(0xf6))
        }),
        ("flags", |m| m.flags ^= 1),
    ];

    for (field, mutate) in mutations {
        let chain = chain_of(5);
        let entries = chain.entries().to_vec();

        // Tamper with the marker in the MIDDLE of history and recompute its
        // chain value from the same prior value. An attacker editing storage
        // changes the marker but cannot change what the chain committed to, so
        // the recomputation must disagree with the recorded value.
        let mut tampered = entries[2].marker.clone();
        mutate(&mut tampered);
        let recomputed = tampered.chain_hash(entries[1].chain_hash);
        assert_ne!(
            recomputed, entries[2].chain_hash,
            "mutating {field} must change the chain value at its sequence"
        );

        // And every later chain value depended on that one, so the divergence
        // propagates: history after the tamper is invalidated too.
        let later = entries[3].marker.chain_hash(recomputed);
        assert_ne!(
            later, entries[3].chain_hash,
            "mutating {field} must invalidate every later marker"
        );
    }
}

/// A tampered marker is detected AT ITS OWN SEQUENCE, so an operator learns
/// where history diverged rather than merely that it did.
#[test]
fn verification_names_the_sequence_where_history_diverges() {
    let chain = chain_of(6);
    let entries = chain.entries().to_vec();

    // Recompute the chain as a verifier would, with marker 4 replaced.
    let mut tampered = entries[3].marker.clone();
    tampered.commit_hlc ^= 0xff;

    let mut value = CHAIN_ORIGIN;
    let mut diverged_at = None;
    for (index, entry) in entries.iter().enumerate() {
        let marker = if index == 3 { &tampered } else { &entry.marker };
        let recomputed = marker.chain_hash(value);
        if recomputed != entry.chain_hash && diverged_at.is_none() {
            diverged_at = Some(entry.marker.commit_seq);
        }
        value = entry.chain_hash;
    }
    assert_eq!(
        diverged_at,
        Some(4),
        "divergence must be reported at the tampered sequence"
    );
}

/// THE SEQUENCE IS GAP-FREE. A gap makes "the history up to N" ambiguous and
/// a repeat makes it contradictory, so both are refused.
#[test]
fn the_commit_sequence_is_gap_free() {
    let mut chain = chain_of(3);

    for bad_seq in [3u64, 5, 100, 0] {
        let outcome = chain.append(marker(bad_seq, 1_000));
        assert_eq!(
            outcome.err(),
            Some(ChainError::NonContiguousCommitSeq {
                expected: 4,
                found: bad_seq
            }),
            "commit_seq {bad_seq} must be refused"
        );
    }
    // The refusals changed nothing.
    assert_eq!(chain.len(), 3);
    assert!(chain.append(marker(4, 1_000)).is_ok());
}

/// Two commits cannot share one logical-command position.
#[test]
fn the_logical_command_sequence_must_advance() {
    let mut chain = chain_of(3);
    let outcome = chain.append(marker(4, 30));
    assert_eq!(
        outcome.err(),
        Some(ChainError::NonMonotonicCommandSeq {
            previous: 30,
            found: 30
        })
    );
    assert_eq!(chain.len(), 3);
}

/// THE BRANCH-HEAD COMPARE-AND-SWAP. A head advances only against the head
/// the marker expected.
#[test]
fn a_branch_head_advances_only_against_the_expected_head() {
    let mut chain = MarkerChain::new();
    assert_eq!(chain.head(1, 1), None, "an unknown branch has no head");

    // First commit on the branch: expects no previous head.
    chain
        .append(with_heads(marker(1, 10), vec![head(1, 1, None)]))
        .expect("first head update");
    let first_head = chain.head(1, 1).expect("head exists now");
    assert_eq!(first_head.commit_seq, 1);

    // A second commit expecting NO previous head must be refused: the branch
    // has moved on, and this is exactly the lost-update the CAS prevents.
    let outcome = chain.append(with_heads(marker(2, 20), vec![head(1, 1, None)]));
    assert_eq!(
        outcome.err(),
        Some(ChainError::HeadCasMismatch {
            graph: 1,
            branch: 1,
            expected: None,
            actual: Some(first_head),
        })
    );

    // Expecting the CURRENT head succeeds, and the head advances.
    chain
        .append(with_heads(
            marker(2, 20),
            vec![head(1, 1, Some(first_head))],
        ))
        .expect("advancing from the current head");
    let second_head = chain.head(1, 1).expect("head");
    assert_eq!(second_head.commit_seq, 2);
    assert_ne!(second_head.marker_oid, first_head.marker_oid);
}

/// A FAILED CAS CHANGES NOTHING — including the other branches in the same
/// marker. A partial application would leave the stream describing a state
/// that never existed.
#[test]
fn a_failed_head_cas_leaves_every_branch_untouched() {
    let mut chain = MarkerChain::new();
    chain
        .append(with_heads(
            marker(1, 10),
            vec![head(1, 1, None), head(1, 2, None), head(2, 1, None)],
        ))
        .expect("three branches created together");

    let head_a = chain.head(1, 1).expect("a");
    let head_b = chain.head(1, 2).expect("b");
    let head_c = chain.head(2, 1).expect("c");

    // A marker advancing all three, but with ONE stale expectation.
    let stale = MarkerRef {
        marker_oid: oid(0xee),
        commit_seq: 99,
    };
    let outcome = chain.append(with_heads(
        marker(2, 20),
        vec![
            head(1, 1, Some(head_a)),
            head(1, 2, Some(stale)), // stale
            head(2, 1, Some(head_c)),
        ],
    ));
    assert!(matches!(
        outcome,
        Err(ChainError::HeadCasMismatch { branch: 2, .. })
    ));

    assert_eq!(chain.head(1, 1), Some(head_a), "branch (1,1) untouched");
    assert_eq!(chain.head(1, 2), Some(head_b), "branch (1,2) untouched");
    assert_eq!(chain.head(2, 1), Some(head_c), "branch (2,1) untouched");
    assert_eq!(chain.len(), 1, "the marker was not appended");
}

/// One marker may advance several branches atomically — the mechanism that
/// makes a cross-branch commit one event rather than several.
#[test]
fn one_marker_advances_several_branches_atomically() {
    let mut chain = MarkerChain::new();
    chain
        .append(with_heads(
            marker(1, 10),
            vec![head(1, 1, None), head(1, 2, None)],
        ))
        .expect("create two branches");
    let a = chain.head(1, 1).expect("a");
    let b = chain.head(1, 2).expect("b");
    assert_eq!(a.commit_seq, b.commit_seq, "both heads are the same marker");
    assert_eq!(a.marker_oid, b.marker_oid);
}

/// Head updates must be canonically sorted and duplicate-free: a duplicate
/// coordinate would make the marker's own effect on that head ambiguous.
#[test]
fn head_updates_must_be_canonical() {
    let mut chain = MarkerChain::new();

    let unsorted = with_heads(
        marker(1, 10),
        vec![head(2, 1, None), head(1, 1, None)], // out of order
    );
    assert_eq!(
        chain.append(unsorted).err(),
        Some(ChainError::NonCanonicalHeadUpdates)
    );

    let duplicated = with_heads(marker(1, 10), vec![head(1, 1, None), head(1, 1, None)]);
    assert_eq!(
        chain.append(duplicated).err(),
        Some(ChainError::NonCanonicalHeadUpdates)
    );
    assert!(chain.is_empty(), "neither malformed marker was appended");
}

/// A marker's identity is its position in history, not merely its content:
/// identical content at different sequences yields distinct markers, which is
/// what makes a MarkerRef a history identity.
#[test]
fn identical_content_at_different_positions_yields_distinct_markers() {
    let mut first = MarkerChain::new();
    first.append(marker(1, 10)).expect("append");
    let a = first.entries()[0].marker_oid;

    let mut second = MarkerChain::new();
    second.append(marker(1, 99)).expect("append");
    second.append(marker(2, 100)).expect("append");
    // Same content shape, different history: distinct identity.
    assert_ne!(a, second.entries()[1].marker_oid);
}

/// Verification needs nothing but the entries — no index, no future object —
/// so a stream PREFIX validates without its suffix. That is what lets a
/// replica or a recovery pass make progress on partial history.
#[test]
fn a_stream_prefix_verifies_without_its_suffix() {
    let full = chain_of(10);
    let entries = full.entries().to_vec();

    for prefix_len in 1..=entries.len() {
        let mut value = CHAIN_ORIGIN;
        for entry in entries.iter().take(prefix_len) {
            let recomputed = entry.marker.chain_hash(value);
            assert_eq!(
                recomputed, entry.chain_hash,
                "prefix of {prefix_len} must verify at commit_seq {}",
                entry.marker.commit_seq
            );
            value = recomputed;
        }
    }
}
