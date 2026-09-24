//! Actual batch/availability/Raft/application drivers with a deliberately
//! memory-only authority model. No production receipt or disk claim.
use super::*;
use fgdb_repl::application::member::proposal::batch::{
    AssessedBatch, BatchProposalError, BatchProposalLimits, MemberBatchProposalOutput,
    PayloadBatchAuthority, ProposalRange,
};
use fgdb_repl::availability::{AvailabilityError, assess_systematic};

struct BatchPermit<'a> {
    backend: &'a mut Backend,
    positions: ProposalRange,
    commands: Vec<u64>,
}
impl Drop for BatchPermit<'_> {
    fn drop(&mut self) {
        let mut s = self.backend.0.borrow_mut();
        s.active_permits -= 1;
        s.trace.push("batch-permit-drop");
    }
}
impl PayloadBatchAuthority<u64> for Backend {
    type Error = &'static str;
    type Permit<'a> = BatchPermit<'a> where Self: 'a;
    fn acquire_batch<'a>(&'a mut self, batch: AssessedBatch<'_, u64>)
        -> impl Future<Output = Result<Self::Permit<'a>, Self::Error>> {
        async move {
            let mode = {
                let mut s = self.0.borrow_mut();
                s.acquires += 1;
                s.trace.push("batch-acquire");
                s.acquire
            };
            if mode == Mode::Suspend { pending::<()>().await; }
            if mode == Mode::Refuse { return Err("authority unavailable"); }
            if mode == Mode::Panic { panic!("batch authority panic"); }
            let mut s = self.0.borrow_mut();
            // Independent expected commands live in the modeled authoritative
            // store, not in the driver's submitted range or assessment vector.
            let root = s.root.as_ref().unwrap();
            let last = root.snapshot().map_or(0, |cut| cut.index()) + root.entries().len() as u64;
            if batch.range().len() != s.batch_commands.len() { return Err("group mismatch"); }
            for (offset, (position, command, assessment)) in batch.entries().enumerate() {
                if *command != s.batch_commands[offset] || assessment.input() != &s.expected
                    || position.index != last + offset as u64 + 1 || position.term != root.term()
                    || position.configuration != root.configuration().identity()
                    || position.domain != root.configuration().domain()
                { return Err("ordered command authority mismatch"); }
            }
            let commands = s.batch_commands.clone();
            s.active_permits += 1;
            drop(s);
            Ok(BatchPermit { backend: self, positions: batch.range(), commands })
        }
    }
}
impl RaftPublisher<u64> for BatchPermit<'_> {
    type Error = &'static str;
    fn publish(&mut self, state: &PersistentState<u64>) -> impl Future<Output = Result<(), Self::Error>> {
        let s = self.backend.0.borrow();
        assert_eq!(s.active_permits, 1, "whole-group custody spans publication");
        let previous = s.root.as_ref().unwrap();
        assert_eq!(state.term(), self.positions.first().term);
        assert_eq!(state.configuration(), previous.configuration());
        assert_eq!(state.snapshot(), previous.snapshot());
        assert_eq!(&state.entries()[..previous.entries().len()], previous.entries());
        let appended = &state.entries()[previous.entries().len()..];
        assert_eq!(appended.len(), self.commands.len());
        for (entry, command) in appended.iter().zip(&self.commands) {
            assert_eq!(entry.term, state.term());
            assert_eq!(entry.command, Some(*command));
        }
        let mode = s.publish;
        let fresh = s.acquire == Mode::Ready && s.batch_commands == self.commands;
        drop(s);
        self.backend.0.borrow_mut().trace.push("batch-raft");
        if mode == Mode::Panic { panic!("batch publisher panic"); }
        if fresh && mode != Mode::Refuse { self.backend.store(state); }
        async move {
            if mode == Mode::SuspendAfter { pending::<()>().await; }
            if !fresh || matches!(mode, Mode::Refuse | Mode::FailAfter) { Err("publication uncertain") }
            else { Ok(()) }
        }
    }
}
fn inputs(node: &Node, count: usize) -> Vec<AvailabilityInput> {
    vec![node.state.borrow().expected.clone(); count]
}
fn submit_batch(node: &mut Node, commands: Vec<u64>) -> MemberBatchProposalOutput<u64> {
    node.state.borrow_mut().batch_commands = commands.clone();
    let data = inputs(node, commands.len());
    immediate(node.member.propose_batch_available(commands, &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))).unwrap()
}

#[test]
fn whole_authorized_group_publishes_once_and_applies_in_bounded_prefixes() {
    let mut group = nodes(&configuration(1)); elect(&mut group, &[1]);
    let mut scalar = nodes(&configuration(1)); elect(&mut scalar, &[1]);
    group[0].state.borrow_mut().trace.clear();
    let before = group[0].member.progress().unwrap();
    let out = submit_batch(&mut group[0], vec![11, 22, 33]);
    assert_eq!(out.positions.len(), 3);
    assert_eq!(out.positions.first().index, 2);
    assert_eq!(out.positions.position(2).unwrap().index, 4);
    assert!(out.positions.position(3).is_none());
    assert_eq!(out.member.consensus.committed.len(), 3);
    assert_eq!(group[0].member.progress().unwrap(), before);
    assert_eq!(group[0].state.borrow().trace, ["batch-acquire", "batch-raft", "batch-permit-drop"]);
    for command in [11, 22, 33] { submit(&mut scalar[0], command); }
    assert_eq!(group[0].member.durable_state().unwrap(), scalar[0].member.durable_state().unwrap());
    for expected in 2..=4 {
        assert_eq!(immediate(group[0].member.apply_next()).unwrap().unwrap().applied.index, expected);
    }
    assert!(immediate(group[0].member.apply_next()).unwrap().is_none());
    apply_all(&mut scalar[0]);
    assert_eq!(group[0].state.borrow().effects, scalar[0].state.borrow().effects);
}

#[test]
fn any_invalid_member_of_the_group_refuses_before_authority_or_append() {
    for mutation in 0..6 {
        let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
        let node = &mut cluster[0];
        let before = node.member.durable_state().unwrap().clone();
        let mut commands = vec![11, 22];
        let mut data = inputs(node, 2);
        let mut limits = BatchProposalLimits::default();
        match mutation {
            0 => { commands.clear(); data.clear(); }
            1 => { data.pop(); }
            2 => limits.max_commands = 1,
            3 => data[1].policy.basis.configuration[0] ^= 1,
            4 => data[1].receipts.clear(),
            _ => { commands = vec![11; 129]; data = inputs(node, 129); }
        }
        let result = immediate(node.member.propose_batch_available(commands, &data, limits, &mut || Ok::<_, ()>(())));
        assert!(result.is_err());
        if mutation == 3 { assert!(matches!(result, Err(BatchProposalError::WrongBasis { offset: 1 }))); }
        if mutation == 4 { assert!(matches!(result, Err(BatchProposalError::Availability { offset: 1, .. }))); }
        assert_eq!(node.member.durable_state().unwrap(), &before);
        assert_eq!(node.state.borrow().acquires, 0);
        assert_eq!(node.state.borrow().active_permits, 0);
    }
}

#[test]
fn aggregate_work_and_failure_case_budgets_do_not_reset_per_command() {
    let expected = availability(&configuration(1));
    let one = assess_systematic(&expected, AvailabilityLimits::default(), &mut || Ok::<_, ()>(())).unwrap();
    assert!(one.work() > 0 && one.checked_failure_cases() > 0);
    for (work, cases, succeeds) in [
        (one.work() * 2, one.checked_failure_cases() * 2, true),
        (one.work() * 2 - 1, one.checked_failure_cases() * 2, false),
        (one.work() * 2, one.checked_failure_cases() * 2 - 1, false),
    ] {
        let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
        let node = &mut cluster[0];
        node.state.borrow_mut().batch_commands = vec![11, 22];
        let before = node.member.durable_state().unwrap().clone();
        let data = inputs(node, 2);
        let limits = BatchProposalLimits { max_work: work, max_failure_cases: cases, ..BatchProposalLimits::default() };
        let result = immediate(node.member.propose_batch_available(vec![11, 22], &data, limits, &mut || Ok::<_, ()>(())));
        assert_eq!(result.is_ok(), succeeds);
        if !succeeds {
            assert!(matches!(result, Err(BatchProposalError::WorkBudget | BatchProposalError::FailureCaseBudget)
                | Err(BatchProposalError::Availability { error: AvailabilityError::WorkBudget | AvailabilityError::FailureCaseBudget, .. })));
            assert_eq!(node.member.durable_state().unwrap(), &before);
            assert_eq!(node.state.borrow().acquires, 0);
        }
    }
}

#[test]
fn scalar_authority_cannot_authorize_a_group_or_skip_its_later_command() {
    let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
    let node = &mut cluster[0];
    let data = inputs(node, 2);
    let before = node.member.durable_state().unwrap().clone();
    node.state.borrow_mut().batch_commands = vec![11, 22];
    let result = immediate(node.member.propose_batch_available(vec![11, 99], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
    assert!(matches!(result, Err(BatchProposalError::Authority(_))));
    assert_eq!(node.member.durable_state().unwrap(), &before);
    assert_eq!(node.state.borrow().trace.last(), Some(&"batch-acquire"));
    assert_eq!(node.state.borrow().active_permits, 0);
}

#[test]
fn cancelled_acquisition_and_post_acquisition_refusal_publish_nothing() {
    for after in [false, true] {
        let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
        let node = &mut cluster[0];
        let before = node.member.durable_state().unwrap().clone();
        node.state.borrow_mut().batch_commands = vec![11, 22];
        let data = inputs(node, 2);
        if after {
            let store = Rc::clone(&node.state);
            let result = immediate(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || {
                if store.borrow().active_permits != 0 { Err(()) } else { Ok(()) }
            }));
            assert!(matches!(result, Err(BatchProposalError::Interrupted(()))));
        } else {
            node.state.borrow_mut().acquire = Mode::Suspend;
            cancel(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
        }
        assert_eq!(node.member.durable_state().unwrap(), &before);
        assert_eq!(node.state.borrow().active_permits, 0);
        assert!(!node.state.borrow().trace.contains(&"batch-raft"));
    }
}

#[test]
fn uncertain_batch_publication_fences_member_and_never_exposes_partial_local_append() {
    for mode in [Mode::Refuse, Mode::FailAfter, Mode::SuspendAfter, Mode::Panic] {
        let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
        let node = &mut cluster[0];
        node.state.borrow_mut().batch_commands = vec![11, 22];
        node.state.borrow_mut().publish = mode;
        let data = inputs(node, 2);
        if mode == Mode::SuspendAfter {
            cancel(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
        } else if mode == Mode::Panic {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = immediate(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
            })).is_err());
        } else {
            assert!(immediate(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
        }
        assert!(node.member.progress().is_err());
        assert!(immediate(node.member.step(Event::Heartbeat)).is_err());
        assert_eq!(node.state.borrow().active_permits, 0);
        assert_eq!(node.state.borrow().root.as_ref().unwrap().entries().len(),
            if matches!(mode, Mode::FailAfter | Mode::SuspendAfter) { 3 } else { 1 });
    }
}

#[test]
fn current_authority_is_rechecked_at_publication_not_only_at_acquisition() {
    let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
    let node = &mut cluster[0];
    node.state.borrow_mut().batch_commands = vec![11, 22];
    let data = inputs(node, 2);
    let store = Rc::clone(&node.state);
    let result = immediate(node.member.propose_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || {
        if store.borrow().active_permits != 0 { store.borrow_mut().acquire = Mode::Refuse; }
        Ok::<_, ()>(())
    }));
    assert!(matches!(result, Err(BatchProposalError::Replica(_))));
    assert!(node.member.progress().is_err());
    assert_eq!(node.state.borrow().root.as_ref().unwrap().entries().len(), 1);
    assert_eq!(node.state.borrow().active_permits, 0);
}

#[test]
fn joint_consensus_and_audit_remain_independent_of_the_authorized_batch() {
    let config = Configuration::joint(Domain([1; 32]), [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)], [MemberId(3), MemberId(4), MemberId(5)], []).unwrap();
    let mut cluster = nodes(&config); elect(&mut cluster, &[1, 2, 3, 4, 5]);
    cluster[0].state.borrow_mut().hide = true;
    let out = submit_batch(&mut cluster[0], vec![11, 22]);
    assert!(out.member.consensus.committed.is_empty());
    // Old majority but NOT a new majority: the union must not suffice.
    pump(&mut cluster, out.member.consensus.messages, &[1, 2, 4]);
    assert_eq!(cluster[0].member.durable_state().unwrap().commit_index(), 1);
    assert!(immediate(cluster[0].member.apply_next()).unwrap().is_none());
    let out = immediate(cluster[0].member.step(Event::Heartbeat)).unwrap();
    pump(&mut cluster, out.consensus.messages, &[1, 2, 3, 4, 5]);
    apply_all(&mut cluster[0]);
    assert_eq!(cluster[0].member.progress().unwrap().applied.index, 3);
    assert_eq!(cluster[0].member.progress().unwrap().visible_index, 1);
    let out = submit_batch(&mut cluster[0], vec![0]);
    pump(&mut cluster, out.member.consensus.messages, &[1, 2, 3, 4, 5]);
    apply_all(&mut cluster[0]);
    assert_eq!(cluster[0].member.progress().unwrap().visible_index, 4);
}

#[test]
fn followers_and_handoffs_refuse_before_any_payload_calculation() {
    let mut cluster = nodes(&configuration(3));
    let data = inputs(&cluster[0], 1);
    assert!(matches!(immediate(cluster[0].member.propose_batch_available(vec![11], &data,
        BatchProposalLimits::default(), &mut || -> Result<(), ()> { panic!("preflight must precede calculation") })),
        Err(BatchProposalError::Raft(fgdb_order::Error::NotLeader))));
    elect(&mut cluster, &[1, 2, 3]);
    immediate(cluster[0].member.step(Event::TransferLeadership(MemberId(2)))).unwrap();
    assert!(matches!(immediate(cluster[0].member.propose_batch_available(vec![11], &data,
        BatchProposalLimits::default(), &mut || -> Result<(), ()> { panic!("handoff freezes admission") })),
        Err(BatchProposalError::Raft(fgdb_order::Error::LeadershipTransferInProgress))));
    assert_eq!(cluster[0].state.borrow().acquires, 0);
}

use fgdb_repl::application::member::write::{
    BatchSubmitError, UnknownReason, WriteBatchSubmission, WriteError, WriteState,
};
use fgdb_order::{Entry, Message};

fn track_group(node: &mut Node, commands: Vec<u64>) -> WriteBatchSubmission<u64> {
    node.state.borrow_mut().batch_commands = commands.clone();
    let data = inputs(node, commands.len());
    immediate(node.member.submit_batch_available(commands, &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))).unwrap()
}

#[test]
fn each_grouped_invocation_keeps_its_own_apply_and_audit_wait() {
    let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
    let node = &mut cluster[0];
    node.state.borrow_mut().hide = true;
    let submitted = track_group(node, vec![11, 22, 33]);
    assert_eq!(submitted.ids.len(), 3);
    assert_eq!(node.member.pending_writes(), 3);
    for (offset, id) in submitted.ids.iter().enumerate() {
        assert!(matches!(node.member.try_write(id).unwrap(), WriteState::PendingApplication { required, .. }
            if required == 2 + offset as u64));
    }
    immediate(node.member.apply_next()).unwrap().unwrap();
    assert!(matches!(node.member.try_write(&submitted.ids[0]).unwrap(), WriteState::PendingAudit { required: 2, visible: 1 }));
    assert!(matches!(node.member.try_write(&submitted.ids[1]).unwrap(), WriteState::PendingApplication { required: 3, applied: 2 }));
    apply_all(node);
    for id in &submitted.ids { assert!(matches!(node.member.try_write(id).unwrap(), WriteState::PendingAudit { .. })); }
    submit_batch(node, vec![0]);
    apply_all(node);
    for (offset, id) in submitted.ids.iter().enumerate() {
        match node.member.try_write(id).unwrap() {
            WriteState::Visible(visible) => {
                assert_eq!(visible.id(), id);
                assert_eq!(visible.position(), submitted.output.positions.position(offset).unwrap());
            }
            other => panic!("ordered release must resolve each original invocation: {other:?}"),
        }
        assert!(matches!(node.member.try_write(id), Err(WriteError::UnknownInvocation)));
    }
    assert_eq!(node.member.pending_writes(), 0);
}

#[test]
fn scalar_and_group_waiters_share_capacity_and_cancellation_never_aborts_entries() {
    let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
    let node = &mut cluster[0];
    node.member.set_write_limit(3).unwrap();
    let data = inputs(node, 1);
    let scalar = immediate(node.member.submit_available(9, &data[0], AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).unwrap();
    let submitted = track_group(node, vec![11, 22]);
    assert_eq!(node.member.pending_writes(), 3);
    assert_eq!(node.member.set_write_limit(2), Err(WriteError::Backpressure));
    let acquires = node.state.borrow().acquires;
    assert!(matches!(immediate(node.member.submit_batch_available(vec![33], &data,
        BatchProposalLimits::default(), &mut || -> Result<(), ()> { panic!("capacity must precede assessment") })),
        Err(BatchSubmitError::Admission(WriteError::Backpressure))));
    assert_eq!(node.state.borrow().acquires, acquires);
    assert!(node.member.cancel_write(&submitted.ids[0]));
    apply_all(node);
    assert_eq!(node.state.borrow().effects, [(2, 9), (3, 11), (4, 22)]);
    assert!(matches!(node.member.try_write(&submitted.ids[0]), Err(WriteError::UnknownInvocation)));
    assert!(matches!(node.member.try_write(&scalar.id).unwrap(), WriteState::Visible(_)));
    assert!(matches!(node.member.try_write(&submitted.ids[1]).unwrap(), WriteState::Visible(_)));
}

#[test]
fn failed_or_cancelled_group_admission_releases_all_and_only_its_reservations() {
    for mode in [Mode::Refuse, Mode::Suspend, Mode::Panic] {
        let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
        let node = &mut cluster[0];
        let data = inputs(node, 2);
        let existing = immediate(node.member.submit_available(9, &data[0], AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).unwrap();
        node.state.borrow_mut().batch_commands = vec![11, 22];
        node.state.borrow_mut().acquire = mode;
        let before = node.member.durable_state().unwrap().clone();
        if mode == Mode::Suspend {
            cancel(node.member.submit_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
        } else if mode == Mode::Panic {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = immediate(node.member.submit_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
            })).is_err());
        } else {
            assert!(immediate(node.member.submit_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
        }
        assert_eq!(node.member.durable_state().unwrap(), &before);
        assert_eq!(node.member.pending_writes(), 1);
        assert!(matches!(node.member.try_write(&existing.id).unwrap(), WriteState::PendingApplication { .. }));
        assert_eq!(node.state.borrow().active_permits, 0);
        node.state.borrow_mut().acquire = Mode::Ready;
        let retry = track_group(node, vec![11, 22]);
        assert_eq!(retry.ids.len(), 2);
        assert_eq!(node.member.pending_writes(), 3);
    }
}

#[test]
fn an_uncertain_group_append_preserves_the_recovery_fence_for_earlier_waiters() {
    let mut cluster = nodes(&configuration(1)); elect(&mut cluster, &[1]);
    let node = &mut cluster[0];
    let data = inputs(node, 2);
    let existing = immediate(node.member.submit_available(9, &data[0], AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).unwrap();
    node.state.borrow_mut().batch_commands = vec![11, 22];
    node.state.borrow_mut().publish = Mode::SuspendAfter;
    cancel(node.member.submit_batch_available(vec![11, 22], &data, BatchProposalLimits::default(), &mut || Ok::<_, ()>(()))) ;
    assert_eq!(node.member.pending_writes(), 1, "unreturned batch waiters must not leak");
    match node.member.try_write(&existing.id).unwrap() {
        WriteState::Unknown(value) => assert_eq!(value.reason(), UnknownReason::RecoveryRequired),
        other => panic!("uncertain member cannot release success: {other:?}"),
    }
    assert_eq!(node.member.pending_writes(), 0);
    assert_eq!(node.state.borrow().root.as_ref().unwrap().entries().len(), 4);
    assert!(node.state.borrow().effects.is_empty(), "publication is not application");
}

#[test]
fn quorum_can_commit_a_prefix_inside_a_locally_published_tracked_group() {
    let config = configuration(3);
    let mut cluster = nodes(&config);
    // Only the recovering leader retains two old-term uncommitted entries.
    // Catch-up from index 1 in two-entry windows will later split the group.
    let old = PersistentState::from_authenticated_parts(config.clone(), 1, None, 0,
        vec![Entry { term: 1, command: Some(7) }, Entry { term: 1, command: Some(8) }]);
    let replica = Replica::recover(MemberId(1), old,
        Limits { max_log_entries: 16, max_append_entries: 2 }, 16).unwrap();
    let backend = Backend(Rc::clone(&cluster[0].state));
    cluster[0].member = immediate(AppliedReplica::recover(replica, backend, 1, 16)).unwrap();
    let campaign = immediate(cluster[0].member.step(Event::ElectionTimeout)).unwrap();
    let vote_request = campaign.consensus.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let vote = immediate(cluster[1].member.step(Event::Receive(vote_request))).unwrap();
    let leader = immediate(cluster[0].member.step(Event::Receive(vote.consensus.messages.into_iter().next().unwrap()))).unwrap();
    assert_eq!(leader.consensus.role, Role::Leader);
    let initial_probe = leader.consensus.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let submitted = track_group(&mut cluster[0], vec![11, 22]);
    assert_eq!(submitted.output.positions.first().index, 4);
    assert!(submitted.output.member.consensus.messages.is_empty(), "probe lane is still occupied");
    // Empty follower rejects predecessor 2, then receives 1..2 and 3..4.
    let rejection = immediate(cluster[1].member.step(Event::Receive(initial_probe))).unwrap();
    let first = immediate(cluster[0].member.step(Event::Receive(rejection.consensus.messages.into_iter().next().unwrap()))).unwrap();
    let append12 = first.consensus.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let reply12 = immediate(cluster[1].member.step(Event::Receive(append12))).unwrap();
    let next = immediate(cluster[0].member.step(Event::Receive(reply12.consensus.messages.into_iter().next().unwrap()))).unwrap();
    assert_eq!(cluster[0].member.durable_state().unwrap().commit_index(), 0, "old-term quorum cannot commit alone");
    let append34 = next.consensus.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let reply34 = immediate(cluster[1].member.step(Event::Receive(append34))).unwrap();
    let after = immediate(cluster[0].member.step(Event::Receive(reply34.consensus.messages.into_iter().next().unwrap()))).unwrap();
    assert_eq!(cluster[0].member.durable_state().unwrap().commit_index(), 4);
    assert!(matches!(cluster[0].member.try_write(&submitted.ids[0]).unwrap(), WriteState::PendingApplication { required: 4, .. }));
    assert!(matches!(cluster[0].member.try_write(&submitted.ids[1]).unwrap(), WriteState::PendingCommit { required: 5, committed: 4 }));
    assert!(after.consensus.messages.iter().any(|m| matches!(&m.message,
        Message::Append { prev_index: 4, entries, .. } if entries.len() == 1)));
    apply_all(&mut cluster[0]);
    assert!(matches!(cluster[0].member.try_write(&submitted.ids[0]).unwrap(), WriteState::Visible(_)));
    // Do not deliver the final append. A higher term makes only the remaining
    // original invocation Unknown, not successful or definitively aborted.
    immediate(cluster[0].member.step(Event::Receive(Envelope {
        domain: config.domain(), configuration: config.identity(), from: MemberId(3), to: MemberId(1),
        message: Message::RequestVote { term: 3, last_index: 5, last_term: 2 },
    }))).unwrap();
    match cluster[0].member.try_write(&submitted.ids[1]).unwrap() {
        WriteState::Unknown(value) => assert_eq!(value.reason(), UnknownReason::LeadershipLost),
        other => panic!("remaining invocation must not inherit prefix completion: {other:?}"),
    }
    assert_eq!(cluster[0].state.borrow().effects, [(1, 7), (2, 8), (4, 11)]);
}
