use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use fgdb_order::{Configuration, Envelope, Limits, PersistentState, SnapshotCut};
use fgdb_types::ObjectId;

use super::*;
use crate::availability::{
    AvailabilityPolicy, EncodingRequirement, FailureDomain, PayloadBasis, ReceiptCoverage,
    SourceCoverage, StorageLocation,
};
use crate::driver::SequenceError;

fn immediate<F: Future>(future: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    match pin!(future).as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test future unexpectedly suspended"),
    }
}
fn poll_and_cancel<F: Future>(future: F) {
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    assert!(future.as_mut().poll(&mut cx).is_pending());
}

#[derive(Default)]
struct Root {
    trace: Rc<RefCell<Trace>>,
}
impl RaftPublisher<u64> for Root {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.trace.borrow_mut().root = Some(state.clone());
        ready(Ok(()))
    }
}

fn config(count: u128) -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap()
}
fn input(configuration: &Configuration) -> AvailabilityInput {
    let basis = PayloadBasis {
        domain: configuration.domain(),
        configuration: configuration.identity(),
        predicate_digest: [3; 32],
        base_closure_digest: [4; 32],
    };
    let members: Vec<_> = configuration.voters().iter().copied().collect();
    // These are synthetic exact projections for the explicit authority MODEL
    // below, not signed production certificates or payload-durability evidence.
    let storage_sets = match configuration.joint_voters() {
        Some((old, new)) => StorageSets::Joint {
            old: old.iter().copied().collect(),
            new: new.iter().copied().collect(),
        },
        None => StorageSets::Stable(members.clone()),
    };
    let locations = members
        .iter()
        .map(|member| StorageLocation {
            member: *member,
            placement_id: [member.0 as u8; 32],
            failure_domains: vec![FailureDomain(member.0)],
        })
        .collect();
    let receipts = members
        .iter()
        .map(|member| ReceiptCoverage {
            receipt_id: [member.0 as u8 + 10; 32],
            basis: basis.clone(),
            storage_member: *member,
            prepared_ownership_id: [member.0 as u8 + 20; 32],
            coverage: vec![SourceCoverage {
                object_id: ObjectId([30; 32]),
                encoding_id: [31; 32],
                placement_id: [member.0 as u8; 32],
                source_block: 0,
                first_esi: 0,
                end_esi: 3,
            }],
        })
        .collect();
    AvailabilityInput {
        policy: AvailabilityPolicy {
            basis,
            storage_sets,
            locations,
            tolerated_domain_failures: 0,
        },
        requirements: vec![EncodingRequirement {
            object_id: ObjectId([30; 32]),
            encoding_id: [31; 32],
            source_symbols: vec![3],
        }],
        receipts,
    }
}
fn leader(configuration: Configuration, limits: Limits) -> (Replica<u64>, Root) {
    let mut replica = Replica::new(MemberId(1), configuration, limits, 16).unwrap();
    let mut root = Root::default();
    let output = immediate(replica.step(&mut root, Event::ElectionTimeout)).unwrap();
    assert_eq!(output.consensus.role, Role::Leader);
    (replica, root)
}

#[derive(Default)]
struct Trace {
    acquires: usize,
    publishes: usize,
    permits: usize,
    drops: usize,
    fresh: bool,
    // A failed/cancelled publish may already have reached this recovery root.
    root: Option<PersistentState<u64>>,
    positions: Vec<ProposalPosition>,
}
#[derive(Clone, Copy)]
enum AcquireMode {
    Ready,
    Refused,
    Suspended,
}
#[derive(Clone, Copy)]
enum PublishMode {
    Ready,
    FailedBefore,
    FailedAfter,
    SuspendedAfter,
    Panics,
}

// Explicitly a read/verify/ephemeral-custody model. It byte-compares projections
// with its independent expected closure. It does NOT implement signature, time
// authority, prepared-root ownership or durable-filesystem verification.
struct Authority {
    expected: AvailabilityInput,
    command: u64,
    trace: Rc<RefCell<Trace>>,
    acquisition: AcquireMode,
    publication: PublishMode,
}
impl Authority {
    fn new(expected: &AvailabilityInput, command: u64, root: &Root) -> Self {
        root.trace.borrow_mut().fresh = true;
        Self {
            expected: expected.clone(),
            command,
            trace: Rc::clone(&root.trace),
            acquisition: AcquireMode::Ready,
            publication: PublishMode::Ready,
        }
    }
}
struct Permit<'a> {
    owner: &'a mut Authority,
    position: ProposalPosition,
    command: u64,
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut trace = self.owner.trace.borrow_mut();
        trace.permits -= 1;
        trace.drops += 1;
        // No durable object/receipt is removed when the permit is released.
    }
}
impl PayloadProposalAuthority<u64> for Authority {
    type Error = &'static str;
    type Permit<'a>
        = Permit<'a>
    where
        Self: 'a;

    fn acquire<'a>(
        &'a mut self,
        command: &u64,
        position: ProposalPosition,
        assessment: &SystematicAssessment<'_>,
    ) -> impl Future<Output = Result<Self::Permit<'a>, Self::Error>> {
        let command = *command;
        let matches_closure = assessment.input() == &self.expected;
        async move {
            self.trace.borrow_mut().acquires += 1;
            if matches!(self.acquisition, AcquireMode::Suspended) {
                pending::<()>().await;
            }
            if matches!(self.acquisition, AcquireMode::Refused) {
                return Err("authority unavailable");
            }
            if !self.trace.borrow().fresh {
                return Err("proposal freshness expired");
            }
            if !matches_closure || command != self.command {
                return Err("canonical closure mismatch");
            }
            {
                let mut trace = self.trace.borrow_mut();
                trace.permits += 1;
                trace.positions.push(position);
            }
            Ok(Permit {
                owner: self,
                position,
                command,
            })
        }
    }
}
impl RaftPublisher<u64> for Permit<'_> {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut trace = self.owner.trace.borrow_mut();
        assert_eq!(
            trace.permits, 1,
            "authority must still be held at publication"
        );
        trace.publishes += 1;
        assert_eq!(state.term(), self.position.term);
        assert_eq!(state.configuration().domain(), self.position.domain);
        assert_eq!(
            state.configuration().identity(),
            self.position.configuration
        );
        let base = state.snapshot().map_or(0, |cut| cut.index());
        assert_eq!(base + state.entries().len() as u64, self.position.index);
        assert_eq!(state.entries().last().unwrap().command, Some(self.command));
        if matches!(self.owner.publication, PublishMode::Panics) {
            panic!("synchronous publisher failure before returning its future");
        }
        // This check is purposefully INSIDE publication, after acquisition. It
        // models the mandatory final freshness/fence use guard, not a real clock.
        let fresh = trace.fresh;
        let mode = self.owner.publication;
        if fresh && !matches!(mode, PublishMode::FailedBefore) {
            trace.root = Some(state.clone());
        }
        drop(trace);
        async move {
            if !fresh {
                return Err("proposal freshness expired at publication");
            }
            if matches!(mode, PublishMode::SuspendedAfter) {
                pending::<()>().await;
            }
            if matches!(mode, PublishMode::FailedBefore | PublishMode::FailedAfter) {
                Err("root publication outcome unknown")
            } else {
                Ok(())
            }
        }
    }
}

fn submit(
    replica: &mut Replica<u64>,
    input: &AvailabilityInput,
    authority: &mut Authority,
) -> Result<ProposalOutput<u64>, ProposalError<&'static str, ()>> {
    immediate(propose(
        replica,
        42,
        input,
        AvailabilityLimits::default(),
        authority,
        &mut || Ok(()),
    ))
}

#[test]
fn exact_payload_and_authority_precede_local_append() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let mut authority = Authority::new(&input, 42, &root);
    let output = submit(&mut replica, &input, &mut authority).unwrap();
    assert_eq!((output.position.term, output.position.index), (1, 2));
    assert_eq!(output.position.member, MemberId(1));
    assert!(
        output
            .replica
            .consensus
            .committed
            .iter()
            .any(|entry| entry.entry.command == Some(42))
    );
    let trace = authority.trace.borrow();
    assert_eq!(
        (trace.acquires, trace.publishes, trace.permits, trace.drops),
        (1, 1, 0, 1)
    );
    assert_eq!(trace.positions, [output.position]);
    assert_eq!(trace.root.as_ref(), Some(replica.durable_state().unwrap()));
}

#[test]
fn missing_payload_refuses_before_authority_or_consensus_io() {
    let configuration = config(1);
    let mut input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let before = replica.durable_state().unwrap().clone();
    let mut authority = Authority::new(&input, 42, &root);
    input.receipts[0].coverage[0].end_esi = 2;
    assert!(matches!(
        submit(&mut replica, &input, &mut authority),
        Err(ProposalError::Availability(
            AvailabilityError::Unrecoverable(_)
        ))
    ));
    assert_eq!(authority.trace.borrow().acquires, 0);
    assert_eq!(replica.durable_state().unwrap(), &before);
}

#[test]
fn mathematical_success_does_not_authenticate_receipts_or_the_command() {
    let configuration = config(1);
    let original = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let before = replica.durable_state().unwrap().clone();
    let mut authority = Authority::new(&original, 42, &root);
    for mutation in 0..5 {
        let mut forged = original.clone();
        match mutation {
            0 => forged.receipts[0].receipt_id = [99; 32],
            1 => forged.receipts[0].prepared_ownership_id = [99; 32],
            2 => {
                forged.policy.basis.predicate_digest = [99; 32];
                forged.receipts[0].basis.predicate_digest = [99; 32];
            }
            3 => {
                forged.policy.basis.base_closure_digest = [99; 32];
                forged.receipts[0].basis.base_closure_digest = [99; 32];
            }
            _ => authority.command = 99,
        }
        assert!(matches!(
            submit(&mut replica, &forged, &mut authority),
            Err(ProposalError::Authority("canonical closure mismatch"))
        ));
        assert_eq!(replica.durable_state().unwrap(), &before);
    }
    assert_eq!(authority.trace.borrow().publishes, 0);
}

#[test]
fn follower_domain_and_configuration_preflight_precedes_calculation() {
    let configuration = config(1);
    let original = input(&configuration);
    let mut replica =
        Replica::new(MemberId(1), configuration.clone(), Limits::default(), 16).unwrap();
    let mut root = Root::default();
    let mut authority = Authority::new(&original, 42, &root);
    assert!(matches!(
        submit(&mut replica, &original, &mut authority),
        Err(ProposalError::Raft(RaftError::NotLeader))
    ));
    immediate(replica.step(&mut root, Event::ElectionTimeout)).unwrap();
    for domain in [false, true] {
        let mut wrong = original.clone();
        if domain {
            wrong.policy.basis.domain = Domain([99; 32]);
        } else {
            wrong.policy.basis.configuration = [99; 32];
        }
        assert!(matches!(
            submit(&mut replica, &wrong, &mut authority),
            Err(ProposalError::WrongBasis)
        ));
    }
    let mut wrong = original.clone();
    wrong.policy.storage_sets = StorageSets::Joint {
        old: vec![MemberId(1)],
        new: vec![MemberId(1)],
    };
    assert!(matches!(
        submit(&mut replica, &wrong, &mut authority),
        Err(ProposalError::WrongStorageForm)
    ));
    assert_eq!(authority.trace.borrow().acquires, 0);
}

#[test]
fn joint_configuration_uses_both_storage_predicates_before_submission() {
    let configuration =
        Configuration::joint(Domain([1; 32]), [2; 32], [MemberId(1)], [MemberId(1)], []).unwrap();
    let mut input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let mut authority = Authority::new(&input, 42, &root);
    submit(&mut replica, &input, &mut authority).unwrap();
    input.policy.storage_sets = StorageSets::Stable(vec![MemberId(1)]);
    assert!(matches!(
        submit(&mut replica, &input, &mut authority),
        Err(ProposalError::WrongStorageForm)
    ));
    assert_eq!(authority.trace.borrow().acquires, 1);
}

#[test]
fn every_new_proposal_reacquires_freshness() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let mut authority = Authority::new(&input, 42, &root);
    assert_eq!(
        submit(&mut replica, &input, &mut authority)
            .unwrap()
            .position
            .index,
        2
    );
    authority.trace.borrow_mut().fresh = false;
    assert!(matches!(
        submit(&mut replica, &input, &mut authority),
        Err(ProposalError::Authority("proposal freshness expired"))
    ));
    assert_eq!(replica.durable_state().unwrap().entries().len(), 2);
    authority.trace.borrow_mut().fresh = true;
    assert_eq!(
        submit(&mut replica, &input, &mut authority)
            .unwrap()
            .position
            .index,
        3
    );
    let trace = authority.trace.borrow();
    assert_eq!((trace.acquires, trace.publishes), (3, 2));
}

#[test]
fn cancelled_or_refused_acquisition_leaves_raft_reusable() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let before = replica.durable_state().unwrap().clone();
    let mut authority = Authority::new(&input, 42, &root);
    authority.acquisition = AcquireMode::Suspended;
    poll_and_cancel(propose(
        &mut replica,
        42,
        &input,
        AvailabilityLimits::default(),
        &mut authority,
        &mut || Ok::<(), ()>(()),
    ));
    assert_eq!(replica.durable_state().unwrap(), &before);
    assert_eq!(authority.trace.borrow().permits, 0);
    authority.acquisition = AcquireMode::Refused;
    assert!(matches!(
        submit(&mut replica, &input, &mut authority),
        Err(ProposalError::Authority(_))
    ));
    assert_eq!(replica.durable_state().unwrap(), &before);
    authority.acquisition = AcquireMode::Ready;
    submit(&mut replica, &input, &mut authority).unwrap();
}

#[test]
fn cancellation_after_authority_drops_only_the_ephemeral_permit() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let before = replica.durable_state().unwrap().clone();
    let mut authority = Authority::new(&input, 42, &root);
    let trace = Rc::clone(&authority.trace);
    let result = immediate(propose(
        &mut replica,
        42,
        &input,
        AvailabilityLimits::default(),
        &mut authority,
        &mut || {
            if trace.borrow().permits > 0 {
                Err("cancel before append")
            } else {
                Ok(())
            }
        },
    ));
    assert!(matches!(
        result,
        Err(ProposalError::Interrupted("cancel before append"))
    ));
    assert_eq!(replica.durable_state().unwrap(), &before);
    assert_eq!(
        (
            trace.borrow().permits,
            trace.borrow().drops,
            trace.borrow().publishes
        ),
        (0, 1, 0)
    );
    submit(&mut replica, &input, &mut authority).unwrap();
}

#[test]
fn expiry_between_acquisition_and_publication_cannot_release_messages() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, root) = leader(configuration, Limits::default());
    let before = replica.durable_state().unwrap().clone();
    let mut authority = Authority::new(&input, 42, &root);
    let trace = Rc::clone(&authority.trace);
    let result = immediate(propose(
        &mut replica,
        42,
        &input,
        AvailabilityLimits::default(),
        &mut authority,
        &mut || {
            let acquired = trace.borrow().permits > 0;
            if acquired {
                trace.borrow_mut().fresh = false;
            }
            Ok::<(), ()>(())
        },
    ));
    assert!(matches!(
        result,
        Err(ProposalError::Replica(ReplicaError::Sequence(
            SequenceError::Publication("proposal freshness expired at publication")
        )))
    ));
    assert_eq!(replica.role(), Err(RaftError::RecoveryRequired));
    assert_eq!(trace.borrow().root.as_ref(), Some(&before));
    assert_eq!(trace.borrow().drops, 1);
}

#[test]
fn uncertain_publication_cancellation_and_panic_fence_the_voter() {
    for mode in [
        PublishMode::FailedBefore,
        PublishMode::FailedAfter,
        PublishMode::SuspendedAfter,
        PublishMode::Panics,
    ] {
        let configuration = config(1);
        let input = input(&configuration);
        let (mut replica, root) = leader(configuration, Limits::default());
        let mut authority = Authority::new(&input, 42, &root);
        authority.publication = mode;
        match mode {
            PublishMode::SuspendedAfter => poll_and_cancel(propose(
                &mut replica,
                42,
                &input,
                AvailabilityLimits::default(),
                &mut authority,
                &mut || Ok::<(), ()>(()),
            )),
            PublishMode::Panics => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = submit(&mut replica, &input, &mut authority);
                }));
                assert!(result.is_err());
            }
            _ => assert!(matches!(
                submit(&mut replica, &input, &mut authority),
                Err(ProposalError::Replica(ReplicaError::Sequence(
                    SequenceError::Publication(_)
                )))
            )),
        }
        assert_eq!(replica.role(), Err(RaftError::RecoveryRequired));
        let trace = authority.trace.borrow();
        assert_eq!((trace.permits, trace.drops), (0, 1));
        let published = trace.root.as_ref().unwrap();
        let reopened =
            Replica::recover(MemberId(1), published.clone(), Limits::default(), 16).unwrap();
        let has_command = reopened
            .committed_after(0)
            .unwrap()
            .iter()
            .any(|entry| entry.entry.command == Some(42));
        assert_eq!(
            has_command,
            matches!(mode, PublishMode::FailedAfter | PublishMode::SuspendedAfter)
        );
    }
}

#[test]
fn full_log_refusal_releases_authority_without_poisoning_or_publishing() {
    let configuration = config(1);
    let input = input(&configuration);
    let limits = Limits {
        max_log_entries: 1,
        max_append_entries: 1,
    };
    let (mut replica, root) = leader(configuration, limits);
    let mut authority = Authority::new(&input, 42, &root);
    assert!(matches!(
        submit(&mut replica, &input, &mut authority),
        Err(ProposalError::Replica(ReplicaError::Sequence(
            SequenceError::Raft(RaftError::LogFull)
        )))
    ));
    assert_eq!(replica.role(), Ok(Role::Leader));
    let trace = authority.trace.borrow();
    assert_eq!(
        (trace.acquires, trace.publishes, trace.permits, trace.drops),
        (1, 0, 0, 1)
    );
}

#[test]
fn compacted_prefix_is_included_in_the_exact_proposal_position() {
    let configuration = config(1);
    let input = input(&configuration);
    let (mut replica, mut root) = leader(configuration.clone(), Limits::default());
    let cut =
        SnapshotCut::from_authenticated_parts(&configuration, [40; 32], [41; 32], [42; 32], 1, 1)
            .unwrap();
    immediate(replica.step(&mut root, Event::Compact(cut))).unwrap();
    assert!(replica.durable_state().unwrap().entries().is_empty());
    let mut authority = Authority::new(&input, 42, &root);
    assert_eq!(
        submit(&mut replica, &input, &mut authority)
            .unwrap()
            .position
            .index,
        2
    );
}

struct Node {
    replica: Replica<u64>,
    root: Root,
}
fn pump(nodes: &mut [Node], messages: Vec<Envelope<u64>>) {
    let mut messages: VecDeque<_> = messages.into();
    let mut steps = 0;
    while let Some(message) = messages.pop_front() {
        steps += 1;
        assert!(steps < 4096, "native Raft dispatch must converge");
        let node = &mut nodes[message.to.0 as usize - 1];
        let output = immediate(node.replica.step(&mut node.root, Event::Receive(message))).unwrap();
        messages.extend(output.consensus.messages);
    }
}

#[test]
fn payload_availability_and_local_durability_do_not_replace_a_raft_quorum() {
    let configuration = config(3);
    let input = input(&configuration);
    let mut nodes: Vec<_> = (1..=3)
        .map(|id| Node {
            replica: Replica::new(MemberId(id), configuration.clone(), Limits::default(), 16)
                .unwrap(),
            root: Root::default(),
        })
        .collect();
    let first = &mut nodes[0];
    let election = immediate(first.replica.step(&mut first.root, Event::ElectionTimeout)).unwrap();
    pump(&mut nodes, election.consensus.messages);
    let mut authority = Authority::new(&input, 42, &nodes[0].root);
    let proposal = submit(&mut nodes[0].replica, &input, &mut authority).unwrap();
    assert!(proposal.replica.consensus.committed.is_empty());
    assert!(
        nodes[0]
            .replica
            .committed_after(0)
            .unwrap()
            .iter()
            .all(|entry| entry.entry.command != Some(42))
    );
    // All three payload projections are present, yet withholding voter replies
    // keeps the proposed entry uncommitted. Ordinary authenticated Raft traffic
    // supplies the separate consensus quorum, without another authority check.
    pump(&mut nodes, proposal.replica.consensus.messages);
    for node in &nodes {
        assert!(
            node.replica
                .committed_after(0)
                .unwrap()
                .iter()
                .any(|entry| entry.entry.command == Some(42))
        );
    }
    assert_eq!(authority.trace.borrow().acquires, 1);
    // The permit and subsequent ordinary consensus transitions publish through
    // the same recovery-root model, rather than two independent memory stores.
    assert_eq!(
        authority.trace.borrow().root.as_ref(),
        Some(nodes[0].replica.durable_state().unwrap())
    );
}
