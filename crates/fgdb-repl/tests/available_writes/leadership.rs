//! Composes the public owning APIs with the existing memory publication model.
//! The model is not a production authority verifier or durable disk backend.

use super::*;
use fgdb_order::{Error as RaftError, LeadershipTransferPhase, Message};
use fgdb_repl::application::member::write::{SubmitError, UnknownReason, WriteError, WriteState};

fn tracked(
    node: &mut Node,
    command: u64,
) -> fgdb_repl::application::member::write::WriteSubmission<u64> {
    let input = node.state.borrow().expected.clone();
    immediate(node.member.submit_available(
        command,
        &input,
        AvailabilityLimits::default(),
        &mut || Ok::<_, ()>(()),
    ))
    .unwrap()
}

#[test]
fn handoff_rejects_new_authority_without_losing_preexisting_apply_and_audit_waiters() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    nodes[0].state.borrow_mut().hide = true;
    let submitted = tracked(&mut nodes[0], 77);
    pump(
        &mut nodes,
        submitted.output.member.consensus.messages,
        &[1, 2, 3],
    );
    let (read_id, output) = immediate(nodes[0].member.read_index()).unwrap();
    pump(&mut nodes, output.consensus.messages, &[1, 2, 3]);
    let output = immediate(nodes[0].member.step(Event::Heartbeat)).unwrap();
    pump(&mut nodes, output.consensus.messages, &[1, 2, 3]);
    let handoff = immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    assert_eq!(handoff.consensus.role, Role::Leader);
    assert!(handoff.leadership_lost.is_empty());
    let id = nodes[0].member.leadership_transfer().unwrap().unwrap().id();
    assert_eq!(nodes[0].member.pending_writes(), 1);
    assert_eq!(nodes[0].member.pending_reads(), 1);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::PendingApplication { .. }
    ));
    assert!(matches!(
        immediate(nodes[0].member.try_read(&read_id)).unwrap(),
        ReadState::PendingApplication { .. }
    ));

    let input = nodes[0].state.borrow().expected.clone();
    let before = nodes[0].member.durable_state().unwrap().clone();
    let acquires = nodes[0].state.borrow().acquires;
    let mut checkpoints = 0;
    let refused = immediate(nodes[0].member.submit_available(
        88,
        &input,
        AvailabilityLimits::default(),
        &mut || {
            checkpoints += 1;
            Ok::<_, ()>(())
        },
    ));
    assert!(matches!(
        refused,
        Err(SubmitError::Proposal(MemberProposalError::Proposal(
            ProposalError::Raft(RaftError::LeadershipTransferInProgress)
        )))
    ));
    assert_eq!(
        checkpoints, 0,
        "refuse before evaluating even the payload assessment"
    );
    assert_eq!(nodes[0].state.borrow().acquires, acquires);
    assert_eq!(nodes[0].state.borrow().active_permits, 0);
    assert_eq!(
        nodes[0].member.pending_writes(),
        1,
        "failed admission must not leak a slot"
    );
    assert_eq!(nodes[0].member.durable_state().unwrap(), &before);

    apply_all(&mut nodes[0]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::PendingAudit { .. }
    ));
    assert!(matches!(
        immediate(nodes[0].member.try_read(&read_id)).unwrap(),
        ReadState::PendingAudit { .. }
    ));
    immediate(nodes[0].member.step(Event::AbortLeadershipTransfer(id))).unwrap();
    let release = submit(&mut nodes[0], 0);
    pump(&mut nodes, release.member.consensus.messages, &[1, 2, 3]);
    apply_all(&mut nodes[0]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::Visible(_)
    ));
    match immediate(nodes[0].member.try_read(&read_id)).unwrap() {
        ReadState::Ready(read) => assert_eq!(read.view(), &[77, 0]),
        _ => panic!("ordinary ordered audit release must still complete the read"),
    }
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id),
        Err(WriteError::UnknownInvocation)
    ));
}

#[test]
fn actual_transfer_stepdown_cancels_reads_and_keeps_write_unknown_after_later_apply() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    let submitted = tracked(&mut nodes[0], 17);
    pump(
        &mut nodes,
        submitted.output.member.consensus.messages,
        &[1, 2, 3],
    );
    let (read_id, _) = immediate(nodes[0].member.read_index()).unwrap(); // leave probes undelivered
    let output = immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    assert!(matches!(
        immediate(nodes[0].member.try_read(&read_id)).unwrap(),
        ReadState::PendingQuorum
    ));
    let request = output
        .consensus
        .messages
        .into_iter()
        .find(|m| matches!(m.message, Message::TimeoutNow { .. }))
        .unwrap();
    let campaign = immediate(nodes[1].member.step(Event::Receive(request))).unwrap();
    assert_eq!(campaign.consensus.role, Role::Candidate);
    let request = campaign
        .consensus
        .messages
        .into_iter()
        .find(|m| m.to == MemberId(1))
        .unwrap();
    let stepped_down = immediate(nodes[0].member.step(Event::Receive(request))).unwrap();
    assert_eq!(stepped_down.consensus.role, Role::Follower);
    assert_eq!(stepped_down.leadership_lost, [read_id.clone()]);
    assert!(nodes[0].member.leadership_transfer().unwrap().is_none());
    match nodes[0].member.try_write(&submitted.id).unwrap() {
        WriteState::Unknown(unknown) => assert_eq!(unknown.reason(), UnknownReason::LeadershipLost),
        _ => panic!("handoff cannot turn an outstanding invocation into a client result"),
    }
    assert!(immediate(nodes[0].member.try_read(&read_id)).is_err());
    pump(&mut nodes, stepped_down.consensus.messages, &[1, 2, 3]);
    assert_eq!(nodes[1].state.borrow().root.as_ref().unwrap().term(), 2);
    let heartbeat = immediate(nodes[1].member.step(Event::Heartbeat)).unwrap();
    pump(&mut nodes, heartbeat.consensus.messages, &[1, 2, 3]);
    apply_all(&mut nodes[0]);
    assert_eq!(nodes[0].state.borrow().effects, [(2, 17)]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id),
        Err(WriteError::UnknownInvocation)
    ));
    let next = submit(&mut nodes[1], 18);
    pump(&mut nodes, next.member.consensus.messages, &[1, 2, 3]);
    apply_all(&mut nodes[1]);
    assert_eq!(
        nodes[1]
            .state
            .borrow()
            .effects
            .iter()
            .map(|(_, value)| *value)
            .collect::<Vec<_>>(),
        [17, 18]
    );
}

#[test]
fn silent_target_deadline_reopens_owned_admission_without_discarding_prior_commands() {
    let mut nodes = nodes(&configuration(3));
    for node in &mut nodes {
        node.member.configure_append_pipeline(4).unwrap();
    }
    elect(&mut nodes, &[1, 3]);
    let submitted = tracked(&mut nodes[0], 33);
    pump(
        &mut nodes,
        submitted.output.member.consensus.messages,
        &[1, 3],
    );
    let output = immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    assert!(
        !output
            .consensus
            .messages
            .iter()
            .any(|m| matches!(m.message, Message::TimeoutNow { .. }))
    );
    assert_eq!(
        nodes[0]
            .member
            .leadership_transfer()
            .unwrap()
            .unwrap()
            .phase(),
        LeadershipTransferPhase::CatchingUp
    );
    let id = nodes[0].member.leadership_transfer().unwrap().unwrap().id();
    for _ in 0..3 {
        let output = immediate(nodes[0].member.step(Event::Heartbeat)).unwrap();
        pump(&mut nodes, output.consensus.messages, &[1, 3]);
    }
    let expired = immediate(nodes[0].member.step(Event::LivenessTimeout)).unwrap();
    assert_eq!(expired.consensus.role, Role::Leader);
    assert!(nodes[0].member.leadership_transfer().unwrap().is_none());
    assert!(immediate(nodes[0].member.step(Event::AbortLeadershipTransfer(id))).is_err());
    pump(&mut nodes, expired.consensus.messages, &[1, 3]);
    let next = submit(&mut nodes[0], 34);
    pump(&mut nodes, next.member.consensus.messages, &[1, 3]);
    apply_all(&mut nodes[0]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::Visible(_)
    ));
    assert_eq!(nodes[0].state.borrow().effects, [(2, 33), (3, 34)]);
}

struct ElectionPublisher {
    disk: Rc<RefCell<Option<PersistentState<u64>>>>,
    mode: Mode,
}
impl RaftPublisher<u64> for ElectionPublisher {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        *self.disk.borrow_mut() = Some(state.clone());
        let mode = self.mode;
        if mode == Mode::Panic {
            panic!("election publication panic");
        }
        async move {
            if mode == Mode::SuspendAfter {
                pending::<()>().await;
            }
            if mode == Mode::FailAfter {
                Err("uncertain vote publication")
            } else {
                Ok(())
            }
        }
    }
}

#[test]
fn target_election_error_cancellation_and_panic_require_root_recovery() {
    for mode in [Mode::FailAfter, Mode::SuspendAfter, Mode::Panic] {
        let mut nodes = nodes(&configuration(3));
        elect(&mut nodes, &[1, 2, 3]);
        let offered =
            immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
        let request = offered
            .consensus
            .messages
            .into_iter()
            .find(|m| matches!(m.message, Message::TimeoutNow { .. }))
            .unwrap();
        let state = nodes[1].member.durable_state().unwrap().clone();
        let mut replica =
            Replica::recover(MemberId(2), state.clone(), Limits::default(), 16).unwrap();
        let disk = Rc::new(RefCell::new(Some(state.clone())));
        let mut publisher = ElectionPublisher {
            disk: Rc::clone(&disk),
            mode: Mode::Ready,
        };
        // Recovery cleared the leader. Ordinary authenticated traffic must
        // establish it again before TimeoutNow may bypass pre-vote.
        let heartbeat = Envelope {
            domain: configuration(3).domain(),
            configuration: configuration(3).identity(),
            from: MemberId(1),
            to: MemberId(2),
            message: Message::Append {
                term: 1,
                request: 99,
                prev_index: 1,
                prev_term: 1,
                entries: Vec::new(),
                leader_commit: 1,
            },
        };
        immediate(replica.step(&mut publisher, Event::Receive(heartbeat))).unwrap();
        publisher.mode = mode;
        if mode == Mode::SuspendAfter {
            cancel(replica.step(&mut publisher, Event::Receive(request)));
        } else if mode == Mode::Panic {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = immediate(replica.step(&mut publisher, Event::Receive(request)));
                }))
                .is_err()
            );
        } else {
            assert!(immediate(replica.step(&mut publisher, Event::Receive(request))).is_err());
        }
        assert_eq!(replica.role(), Err(RaftError::RecoveryRequired));
        assert!(replica.leadership_transfer().is_err());
        let restored = disk.borrow().clone().unwrap();
        assert_eq!(restored.term(), 2);
        assert_eq!(restored.voted_for(), Some(MemberId(2)));
        let recovered = Replica::recover(MemberId(2), restored, Limits::default(), 16).unwrap();
        assert_eq!(recovered.role().unwrap(), Role::Follower);
        assert_eq!(recovered.pending_reads(), 0);
    }
}

#[test]
fn new_leader_still_needs_current_term_commit_and_a_fresh_read_quorum() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    let transfer = immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    let timeout = transfer
        .consensus
        .messages
        .into_iter()
        .find(|m| matches!(m.message, Message::TimeoutNow { .. }))
        .unwrap();
    let campaign = immediate(nodes[1].member.step(Event::Receive(timeout))).unwrap();
    let request = campaign
        .consensus
        .messages
        .into_iter()
        .find(|m| m.to == MemberId(1))
        .unwrap();
    let vote = immediate(nodes[0].member.step(Event::Receive(request)))
        .unwrap()
        .consensus
        .messages
        .remove(0);
    let elected = immediate(nodes[1].member.step(Event::Receive(vote))).unwrap();
    assert_eq!(elected.consensus.role, Role::Leader);
    assert_eq!(nodes[1].member.durable_state().unwrap().commit_index(), 1);
    assert!(matches!(
        immediate(nodes[1].member.read_index()),
        Err(fgdb_repl::application::member::MemberError::Replica(
            fgdb_repl::replica::ReplicaError::CurrentTermNotCommitted
        ))
    ));
    let unrelated = Envelope {
        domain: configuration(3).domain(),
        configuration: configuration(3).identity(),
        from: MemberId(1),
        to: MemberId(2),
        message: Message::QuorumReply { term: 2, round: 10 },
    };
    immediate(nodes[1].member.step(Event::Receive(unrelated))).unwrap();
    assert_eq!(nodes[1].member.durable_state().unwrap().commit_index(), 1);
    pump(&mut nodes, elected.consensus.messages, &[1, 2, 3]);
    apply_all(&mut nodes[1]);
    let (read, output) = immediate(nodes[1].member.read_index()).unwrap();
    assert!(matches!(
        immediate(nodes[1].member.try_read(&read)).unwrap(),
        ReadState::PendingQuorum
    ));
    pump(&mut nodes, output.consensus.messages, &[1, 2, 3]);
    match immediate(nodes[1].member.try_read(&read)).unwrap() {
        ReadState::Ready(result) => assert!(result.view().is_empty()),
        _ => panic!("fresh quorum and visible application should complete this read"),
    }
}

#[test]
fn equal_wire_rounds_cannot_reuse_another_incarnations_handoff_cancellation() {
    let mut left = nodes(&configuration(3));
    let mut right = nodes(&configuration(3));
    elect(&mut left, &[1, 2, 3]);
    elect(&mut right, &[1, 2, 3]);
    let l = immediate(left[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    let r = immediate(right[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    let round = |messages: &[Envelope<u64>]| {
        messages
            .iter()
            .find_map(|envelope| {
                if let Message::TimeoutNow { round, .. } = &envelope.message {
                    Some(*round)
                } else {
                    None
                }
            })
            .unwrap()
    };
    assert_eq!(round(&l.consensus.messages), round(&r.consensus.messages));
    let a = left[0].member.leadership_transfer().unwrap().unwrap().id();
    let b = right[0].member.leadership_transfer().unwrap().unwrap().id();
    assert_ne!(a, b);
    assert!(immediate(right[0].member.step(Event::AbortLeadershipTransfer(a))).is_err());
    assert_eq!(
        right[0].member.leadership_transfer().unwrap().unwrap().id(),
        b
    );
}

#[test]
fn grouped_direct_proposals_cannot_bypass_owned_handoff_or_release_hidden_writes() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    nodes[0].state.borrow_mut().hide = true;
    let submitted = tracked(&mut nodes[0], 77);
    pump(
        &mut nodes,
        submitted.output.member.consensus.messages,
        &[1, 2, 3],
    );
    apply_all(&mut nodes[0]);
    immediate(nodes[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    let id = nodes[0].member.leadership_transfer().unwrap().unwrap().id();
    let before = nodes[0].member.durable_state().unwrap().clone();
    let generation = nodes[0].state.borrow().generation;
    assert!(matches!(
        immediate(nodes[0].member.step(Event::ProposeBatch(vec![0, 88]))),
        Err(fgdb_repl::application::member::MemberError::Replica(
            fgdb_repl::replica::ReplicaError::Sequence(fgdb_repl::driver::SequenceError::Raft(
                RaftError::LeadershipTransferInProgress
            ))
        ))
    ));
    assert_eq!(nodes[0].member.durable_state().unwrap(), &before);
    assert_eq!(nodes[0].state.borrow().generation, generation);
    assert_eq!(nodes[0].member.pending_writes(), 1);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::PendingAudit { .. }
    ));
    immediate(nodes[0].member.step(Event::AbortLeadershipTransfer(id))).unwrap();
    // Trusted already-authorized group: the model's ordered release command is
    // effective only after normal quorum and separate application publication.
    let out = immediate(nodes[0].member.step(Event::ProposeBatch(vec![0, 88]))).unwrap();
    pump(&mut nodes, out.consensus.messages, &[1, 2, 3]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::PendingAudit { .. }
    ));
    apply_all(&mut nodes[0]);
    assert!(matches!(
        nodes[0].member.try_write(&submitted.id).unwrap(),
        WriteState::Visible(_)
    ));
    assert_eq!(nodes[0].state.borrow().effects, [(2, 77), (3, 0), (4, 88)]);
}
