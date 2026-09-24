use super::*;
use fgdb_order::{Message, SnapshotCut};
use fgdb_repl::application::member::write::{
    SubmitError, UnknownReason, WriteError, WriteId, WriteState, WriteSubmission,
};

fn submit_tracked(node: &mut Node, command: u64) -> WriteSubmission<u64> {
    let input = node.state.borrow().expected.clone();
    immediate(node.member.submit_available(
        command,
        &input,
        AvailabilityLimits::default(),
        &mut || Ok::<_, ()>(()),
    ))
    .unwrap()
}
fn assert_visible(node: &mut Node, id: &WriteId, expected_index: u64) {
    match node.member.try_write(id).unwrap() {
        WriteState::Visible(result) => {
            assert_eq!(result.id(), id);
            assert_eq!(result.position().index, expected_index);
            assert!(result.progress().visible_index >= expected_index);
        }
        result => panic!("write was not visible: {result:?}"),
    }
    assert!(matches!(
        node.member.try_write(id),
        Err(WriteError::UnknownInvocation)
    ));
}

#[test]
fn tracked_write_waits_for_each_distinct_gate_and_consumes_visibility_once() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    nodes[0].state.borrow_mut().hide = true;
    let submission = submit_tracked(&mut nodes[0], 77);
    let id = submission.id;
    assert!(matches!(
        nodes[0].member.try_write(&id).unwrap(),
        WriteState::PendingCommit {
            required: 2,
            committed: 1
        }
    ));
    pump(
        &mut nodes,
        submission.output.member.consensus.messages,
        &[1, 2],
    );
    assert!(matches!(
        nodes[0].member.try_write(&id).unwrap(),
        WriteState::PendingApplication {
            required: 2,
            applied: 1
        }
    ));
    apply_all(&mut nodes[0]);
    assert!(matches!(
        nodes[0].member.try_write(&id).unwrap(),
        WriteState::PendingAudit {
            required: 2,
            visible: 1
        }
    ));
    assert_eq!(nodes[0].member.pending_writes(), 1);
    // Audit controls cannot be blocked by a wait for the write they release.
    let out = submit(&mut nodes[0], 0);
    pump(&mut nodes, out.member.consensus.messages, &[1, 2]);
    apply_all(&mut nodes[0]);
    assert_visible(&mut nodes[0], &id, 2);
    assert_eq!(nodes[0].member.pending_writes(), 0);
}

#[test]
fn write_admission_counts_apply_and_audit_waits_before_authority_work() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    node.member.set_write_limit(1).unwrap();
    node.state.borrow_mut().hide = true;
    let first = submit_tracked(node, 77);
    let input = node.state.borrow().expected.clone();
    for apply in [false, true] {
        if apply {
            apply_all(node);
        }
        let acquisitions = node.state.borrow().acquires;
        assert!(matches!(
            immediate(node.member.submit_available(
                88,
                &input,
                AvailabilityLimits::default(),
                &mut || Ok::<_, ()>(())
            )),
            Err(SubmitError::Admission(WriteError::Backpressure))
        ));
        assert_eq!(node.state.borrow().acquires, acquisitions);
        assert_eq!(node.member.pending_writes(), 1);
    }
    // Reads have a separate bound. The pending write does not consume it.
    let (read, _) = immediate(node.member.read_index()).unwrap();
    assert!(node.member.cancel_read(&read));
    assert_eq!(node.member.pending_writes(), 1);
    assert!(node.member.cancel_write(&first.id));
    assert!(!node.member.cancel_write(&first.id));
    let second = submit_tracked(node, 88);
    assert_ne!(second.id, first.id);
}

#[test]
fn duplicate_commands_are_distinct_invocations_and_limits_never_evict_them() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    node.member.set_write_limit(2).unwrap();
    let first = submit_tracked(node, 77);
    let second = submit_tracked(node, 77);
    assert_ne!(first.id, second.id);
    assert_eq!(
        node.member.set_write_limit(1),
        Err(WriteError::Backpressure)
    );
    assert_eq!(
        node.member.set_write_limit(0),
        Err(WriteError::InvalidLimit)
    );
    assert_eq!(
        node.member.set_write_limit(1025),
        Err(WriteError::InvalidLimit)
    );
    assert_eq!(node.member.pending_writes(), 2);
    // The application driver has batch size one: one completion cannot imply
    // that the immediately following identical command was applied too.
    immediate(node.member.apply_next()).unwrap().unwrap();
    assert_visible(node, &first.id, 2);
    assert!(matches!(
        node.member.try_write(&second.id).unwrap(),
        WriteState::PendingApplication {
            required: 3,
            applied: 2
        }
    ));
    apply_all(node);
    assert_visible(node, &second.id, 3);
    assert_eq!(node.state.borrow().effects, [(2, 77), (3, 77)]);
}

#[test]
fn cancelling_a_waiter_does_not_abort_or_remove_its_later_committed_effect() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    let submission = submit_tracked(&mut nodes[0], 77);
    assert!(nodes[0].member.cancel_write(&submission.id));
    pump(
        &mut nodes,
        submission.output.member.consensus.messages,
        &[1, 2],
    );
    apply_all(&mut nodes[0]);
    assert_eq!(nodes[0].state.borrow().effects, [(2, 77)]);
    assert_eq!(nodes[0].member.pending_writes(), 0);
    assert!(matches!(
        nodes[0].member.try_write(&submission.id),
        Err(WriteError::UnknownInvocation)
    ));
}

#[test]
fn cancelled_or_refused_admission_never_leaks_a_bounded_slot() {
    for mode in [Mode::Refuse, Mode::Suspend, Mode::Panic] {
        let mut nodes = nodes(&configuration(1));
        elect(&mut nodes, &[1]);
        let node = &mut nodes[0];
        node.member.set_write_limit(1).unwrap();
        node.state.borrow_mut().acquire = mode;
        let input = node.state.borrow().expected.clone();
        let before = node.member.durable_state().unwrap().clone();
        if mode == Mode::Suspend {
            cancel(node.member.submit_available(
                77,
                &input,
                AvailabilityLimits::default(),
                &mut || Ok::<_, ()>(()),
            ));
        } else if mode == Mode::Panic {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    submit_tracked(node, 77);
                }))
                .is_err()
            );
        } else {
            assert!(
                immediate(node.member.submit_available(
                    77,
                    &input,
                    AvailabilityLimits::default(),
                    &mut || Ok::<_, ()>(())
                ))
                .is_err()
            );
        }
        assert_eq!(node.member.pending_writes(), 0);
        assert_eq!(node.member.durable_state().unwrap(), &before);
        node.state.borrow_mut().acquire = Mode::Ready;
        submit_tracked(node, 88);
        assert_eq!(node.member.pending_writes(), 1);
    }
}

#[test]
fn uncertainty_after_a_second_append_makes_earlier_waits_unknown_not_rejected() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    let first = submit_tracked(node, 77);
    node.state.borrow_mut().publish = Mode::SuspendAfter;
    let input = node.state.borrow().expected.clone();
    cancel(
        node.member
            .submit_available(88, &input, AvailabilityLimits::default(), &mut || {
                Ok::<_, ()>(())
            }),
    );
    assert_eq!(node.member.pending_writes(), 1); // Only the unreturned waiter was removed.
    assert_eq!(
        node.state.borrow().root.as_ref().unwrap().entries()[2].command,
        Some(88)
    );
    match node.member.try_write(&first.id).unwrap() {
        WriteState::Unknown(result) => {
            assert_eq!(result.reason(), UnknownReason::RecoveryRequired);
            assert_eq!(result.id(), &first.id);
        }
        state => panic!("uncertain member returned {state:?}"),
    }
    assert_eq!(node.member.pending_writes(), 0);
    assert!(matches!(
        node.member.try_write(&first.id),
        Err(WriteError::UnknownInvocation)
    ));
}

#[test]
fn application_errors_cancellation_and_panic_cannot_release_visible_writes() {
    for mode in [
        Mode::Refuse,
        Mode::FailAfter,
        Mode::SuspendAfter,
        Mode::Panic,
    ] {
        let mut nodes = nodes(&configuration(1));
        elect(&mut nodes, &[1]);
        let node = &mut nodes[0];
        let submission = submit_tracked(node, 77);
        node.state.borrow_mut().apply = mode;
        if mode == Mode::SuspendAfter {
            cancel(node.member.apply_next());
        } else if mode == Mode::Panic {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = immediate(node.member.apply_next());
                }))
                .is_err()
            );
        } else {
            assert!(immediate(node.member.apply_next()).is_err());
        }
        match node.member.try_write(&submission.id).unwrap() {
            WriteState::Unknown(result) => {
                assert_eq!(result.reason(), UnknownReason::RecoveryRequired)
            }
            state => panic!("uncertain application returned {state:?}"),
        }
        if mode != Mode::Refuse {
            assert_eq!(node.state.borrow().progress.visible_index, 2); // The root MAY already have advanced.
        }
    }
}

#[test]
fn leadership_loss_is_sticky_even_if_the_same_command_commits_after_reelection() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    let submission = submit_tracked(&mut nodes[0], 77);
    let config = configuration(3);
    immediate(nodes[0].member.step(Event::Receive(Envelope {
        domain: config.domain(),
        configuration: config.identity(),
        from: MemberId(2),
        to: MemberId(1),
        message: Message::RequestVote {
            term: 2,
            last_index: 1,
            last_term: 1,
        },
    })))
    .unwrap();
    elect(&mut nodes, &[1, 2, 3]);
    assert!(nodes[0].state.borrow().effects.contains(&(2, 77)));
    match nodes[0].member.try_write(&submission.id).unwrap() {
        WriteState::Unknown(result) => assert_eq!(result.reason(), UnknownReason::LeadershipLost),
        state => panic!("lost invocation was resurrected: {state:?}"),
    }
    assert!(matches!(
        nodes[0].member.try_write(&submission.id),
        Err(WriteError::UnknownInvocation)
    ));
}

#[test]
fn durable_commit_evidence_survives_compaction_without_retaining_command_copies() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    let first = submit_tracked(node, 77);
    let second = submit_tracked(node, 88);
    apply_all(node);
    let progress = node.member.progress().unwrap();
    let cut = SnapshotCut::from_authenticated_parts(
        &configuration(1),
        [201; 32],
        progress.state_root.0,
        [202; 32],
        progress.applied.index,
        progress.applied.term,
    )
    .unwrap();
    // Snapshot/floor objects are a verifier projection MODEL here, not real
    // on-disk manifests. The actual kernel still retires the retained suffix.
    immediate(node.member.step(Event::Compact(cut))).unwrap();
    assert!(node.member.durable_state().unwrap().entries().is_empty());
    assert_visible(node, &first.id, 2);
    assert_visible(node, &second.id, 3);
    let third = submit_tracked(node, 99);
    assert_eq!(third.output.position.index, 4);
    apply_all(node);
    assert_visible(node, &third.id, 4);
}

#[test]
fn recovery_and_other_members_cannot_complete_an_old_numeric_write_position() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let original = submit_tracked(&mut nodes[0], 77);
    apply_all(&mut nodes[0]);
    let mut different = super::nodes(&configuration(1));
    elect(&mut different, &[1]);
    let same_position = submit_tracked(&mut different[0], 77);
    assert_eq!(same_position.output.position, original.output.position);
    assert_ne!(same_position.id, original.id);
    assert!(matches!(
        different[0].member.try_write(&original.id),
        Err(WriteError::UnknownInvocation)
    ));
    assert!(!different[0].member.cancel_write(&original.id));
    assert_eq!(different[0].member.pending_writes(), 1);
    let Node { member, state } = nodes.remove(0);
    drop(member); // Model the exclusive member fence before recovery.
    let disk = state.borrow().root.as_ref().unwrap().clone();
    let replica = Replica::recover(MemberId(1), disk, Limits::default(), 16).unwrap();
    let mut recovered = immediate(AppliedReplica::recover(replica, Backend(state), 1, 16)).unwrap();
    assert_eq!(recovered.pending_writes(), 0);
    assert!(matches!(
        recovered.try_write(&original.id),
        Err(WriteError::UnknownInvocation)
    ));
    assert!(!recovered.cancel_write(&original.id));
}

#[test]
fn joint_membership_needs_both_majorities_before_write_application_can_complete() {
    let config = Configuration::joint(
        Domain([1; 32]),
        [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [],
    )
    .unwrap();
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 2, 3, 4, 5]);
    let submission = submit_tracked(&mut nodes[0], 77);
    pump(
        &mut nodes,
        submission.output.member.consensus.messages,
        &[1, 2, 3],
    );
    assert!(matches!(
        nodes[0].member.try_write(&submission.id).unwrap(),
        WriteState::PendingCommit { .. }
    ));
    let out = immediate(nodes[0].member.step(Event::Heartbeat)).unwrap();
    pump(&mut nodes, out.consensus.messages, &[1, 2, 3, 4]);
    apply_all(&mut nodes[0]);
    assert_visible(&mut nodes[0], &submission.id, 2);
}
