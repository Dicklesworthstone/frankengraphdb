//! Executes the real transition kernel. Publication below is an explicit
//! in-memory root model, not Chronicle serialization/fsync/retention evidence.
use crate::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotCut,
};
use std::collections::{BTreeMap, VecDeque};

fn config() -> Configuration {
    Configuration::stable(Domain([7; 32]), [8; 32], (1..=3).map(MemberId), []).unwrap()
}

fn publish(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let pending = node.step(event).unwrap();
    let root = pending.state().clone();
    let id = pending.id();
    assert_eq!(node.durable_state().err(), Some(Error::AwaitingDurability));
    let output = node.persisted(id).unwrap();
    assert_eq!(node.durable_state().unwrap(), &root);
    output
}

fn receive(node: &mut Raft<u64>, from: u128, message: Message<u64>) -> Output<u64> {
    let c = node.durable_state().unwrap().configuration();
    let envelope = Envelope {
        domain: c.domain(),
        configuration: c.identity(),
        from: MemberId(from),
        to: node.id(),
        message,
    };
    publish(node, Event::Receive(envelope))
}

type Cluster = BTreeMap<MemberId, Raft<u64>>;

fn nodes(configuration: &Configuration, window: usize) -> Cluster {
    configuration
        .voters()
        .union(configuration.learners())
        .map(|id| {
            let mut node = Raft::new(
                *id,
                configuration.clone(),
                Limits {
                    max_log_entries: 128,
                    max_append_entries: 2,
                },
            )
            .unwrap();
            node.configure_append_pipeline(window).unwrap();
            (*id, node)
        })
        .collect()
}

fn common_prefixes(nodes: &Cluster) {
    for node in nodes.values() {
        let state = node.durable_state().unwrap();
        let base = state.snapshot().map_or(0, SnapshotCut::index);
        for other in nodes.values() {
            let other = other.durable_state().unwrap();
            let other_base = other.snapshot().map_or(0, SnapshotCut::index);
            let first = base.max(other_base) + 1;
            let last = state.commit_index().min(other.commit_index());
            for index in first..=last {
                assert_eq!(
                    state.entries()[(index - base - 1) as usize],
                    other.entries()[(index - other_base - 1) as usize]
                );
            }
        }
    }
}

fn pump(nodes: &mut Cluster, messages: Vec<Envelope<u64>>, reachable: &[u128]) {
    let mut queue: VecDeque<_> = messages.into();
    let mut steps = 0;
    while let Some(envelope) = queue.pop_front() {
        steps += 1;
        assert!(steps < 20_000, "replication must quiesce");
        if !reachable.contains(&envelope.from.0) || !reachable.contains(&envelope.to.0) {
            continue;
        }
        let node = nodes.get_mut(&envelope.to).unwrap();
        let output = publish(node, Event::Receive(envelope));
        queue.extend(output.messages);
        common_prefixes(nodes);
    }
}

fn elected(window: usize) -> Cluster {
    let mut nodes = nodes(&config(), window);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::ElectionTimeout);
    pump(&mut nodes, out.messages, &[1, 2, 3]);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    pump(&mut nodes, out.messages, &[1, 2, 3]);
    assert_eq!(nodes[&MemberId(1)].role(), Ok(Role::Leader));
    nodes
}

fn burst(nodes: &mut Cluster, count: u64) -> Vec<Envelope<u64>> {
    let mut messages = Vec::new();
    for command in 10..10 + count {
        messages.extend(
            publish(
                nodes.get_mut(&MemberId(1)).unwrap(),
                Event::Propose(command),
            )
            .messages,
        );
    }
    messages
}

fn to(messages: &[Envelope<u64>], peer: u128) -> Vec<Envelope<u64>> {
    messages
        .iter()
        .filter(|message| message.to == MemberId(peer))
        .cloned()
        .collect()
}

fn coordinates(envelope: &Envelope<u64>) -> (u64, u64, u64, u64) {
    match &envelope.message {
        Message::Append {
            term,
            request,
            prev_index,
            entries,
            ..
        } => (
            *term,
            *request,
            *prev_index,
            *prev_index + entries.len() as u64,
        ),
        _ => panic!("expected append"),
    }
}

fn reply(nodes: &mut Cluster, request: Envelope<u64>) -> Envelope<u64> {
    let out = publish(nodes.get_mut(&request.to).unwrap(), Event::Receive(request));
    assert_eq!(out.messages.len(), 1);
    out.messages.into_iter().next().unwrap()
}

#[test]
fn bursts_fill_independent_bounded_windows_without_retransmitting_on_each_proposal() {
    for width in [1, 2, 4, 16, 64] {
        let mut nodes = elected(width);
        let messages = burst(&mut nodes, 70);
        for peer in [2, 3] {
            let requests = to(&messages, peer);
            assert_eq!(requests.len(), width);
            let mut next = 1;
            let mut previous_request = 0;
            for message in &requests {
                let (_, request, prev, last) = coordinates(message);
                assert_eq!(prev, next);
                assert_eq!(last, prev + 1);
                assert!(request > previous_request);
                previous_request = request;
                next = last;
            }
        }
        assert_eq!(
            nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
            1
        );
        let retries = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
        assert_eq!(to(&retries.messages, 2), to(&messages, 2));
        pump(&mut nodes, retries.messages, &[1, 2, 3]);
        let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
        pump(&mut nodes, out.messages, &[1, 2, 3]);
        for node in nodes.values() {
            assert_eq!(node.durable_state().unwrap().commit_index(), 71);
        }
    }
}

#[test]
fn later_success_proves_prefix_and_retires_earlier_requests_without_reusing_ids() {
    let mut nodes = elected(4);
    let messages = burst(&mut nodes, 4);
    let requests = to(&messages, 2);
    let responses: Vec<_> = requests
        .iter()
        .cloned()
        .map(|r| reply(&mut nodes, r))
        .collect();
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(responses[3].clone()),
    );
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        5
    );
    assert_eq!(out.committed.len(), 4);
    let last_request = coordinates(&requests[3]).1;
    assert!(
        to(&out.messages, 2)
            .iter()
            .all(|r| coordinates(r).1 > last_request)
    );
    for request in &requests {
        let (term, serial, _, _) = coordinates(request);
        assert!(
            !nodes[&MemberId(1)]
                .pending_append_reply(MemberId(2), term, serial)
                .unwrap()
        );
    }
    for old in responses {
        let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Receive(old));
        assert!(out.messages.is_empty());
        assert!(out.committed.is_empty());
    }
}

#[test]
fn partial_acknowledgement_refills_only_the_freed_slots_with_contiguous_batches() {
    let mut nodes = elected(3);
    let messages = burst(&mut nodes, 9);
    let initial = to(&messages, 2);
    let response = reply(&mut nodes, initial[0].clone());
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(response),
    );
    let refill = to(&out.messages, 2);
    assert_eq!(refill.len(), 1);
    assert_eq!(coordinates(&refill[0]).2, 4);
    assert_eq!(coordinates(&refill[0]).3, 6);
    assert_eq!(
        nodes[&MemberId(1)].progress[&MemberId(2)].in_flight.len(),
        3
    );
    assert_eq!(nodes[&MemberId(1)].progress[&MemberId(2)].matched, 2);
    let (_, request, _, _) = coordinates(&initial[1]);
    assert!(
        nodes[&MemberId(1)]
            .pending_append_reply(MemberId(2), 1, request)
            .unwrap()
    );
}

#[test]
fn rejecting_a_later_range_clears_speculation_and_probes_before_refilling() {
    let mut nodes = elected(4);
    let messages = burst(&mut nodes, 8);
    let initial = to(&messages, 2);
    let rejection = reply(&mut nodes, initial[3].clone());
    assert!(matches!(
        rejection.message,
        Message::Appended { success: false, .. }
    ));
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(rejection),
    );
    let probe = to(&out.messages, 2);
    assert_eq!(probe.len(), 1);
    assert_eq!(coordinates(&probe[0]).2, 1);
    assert_eq!(nodes[&MemberId(1)].progress[&MemberId(2)].matched, 1);
    assert!(nodes[&MemberId(1)].progress[&MemberId(2)].probing);
    for old in &initial {
        let (term, request, _, _) = coordinates(old);
        assert!(
            !nodes[&MemberId(1)]
                .pending_append_reply(MemberId(2), term, request)
                .unwrap()
        );
    }
    // The follower may still durably accept delayed old requests. Their replies
    // were invalidated locally and cannot stand in for the new probe round.
    for old in initial {
        let response = reply(&mut nodes, old);
        let out = publish(
            nodes.get_mut(&MemberId(1)).unwrap(),
            Event::Receive(response),
        );
        assert!(out.committed.is_empty());
    }
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        1
    );
    let response = reply(&mut nodes, probe[0].clone());
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(response),
    );
    assert!(!nodes[&MemberId(1)].progress[&MemberId(2)].probing);
    assert!(to(&out.messages, 2).len() > 1);
    pump(&mut nodes, out.messages, &[1, 2]);
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        9
    );
}

fn permutations() -> Vec<[usize; 4]> {
    let mut out = Vec::new();
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    if a != b && a != c && a != d && b != c && b != d && c != d {
                        out.push([a, b, c, d]);
                    }
                }
            }
        }
    }
    out
}

#[test]
fn every_four_request_and_four_response_permutation_converges_with_a_silent_minority() {
    for delivery in permutations() {
        for replies in permutations() {
            let mut nodes = elected(4);
            let initial = to(&burst(&mut nodes, 8), 2);
            let responses: Vec<_> = delivery
                .iter()
                .map(|i| reply(&mut nodes, initial[*i].clone()))
                .collect();
            let mut recovery = Vec::new();
            for i in replies {
                recovery.extend(
                    publish(
                        nodes.get_mut(&MemberId(1)).unwrap(),
                        Event::Receive(responses[i].clone()),
                    )
                    .messages,
                );
                common_prefixes(&nodes);
            }
            pump(&mut nodes, recovery, &[1, 2]);
            let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
            pump(&mut nodes, out.messages, &[1, 2]);
            let leader = nodes[&MemberId(1)].durable_state().unwrap();
            let follower = nodes[&MemberId(2)].durable_state().unwrap();
            assert_eq!(leader.commit_index(), 9, "{delivery:?} / {replies:?}");
            assert_eq!(follower.commit_index(), 9);
            assert_eq!(leader.entries(), follower.entries());
        }
    }
}

#[test]
fn a_new_leader_does_not_speculate_before_its_first_matching_append_reply() {
    let mut nodes = nodes(&config(), 4);
    publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::ElectionTimeout);
    let out = receive(
        nodes.get_mut(&MemberId(1)).unwrap(),
        2,
        Message::Vote {
            term: 1,
            granted: true,
        },
    );
    let first = to(&out.messages, 2);
    assert_eq!(first.len(), 1);
    assert!(burst(&mut nodes, 12).is_empty());
    let retry = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    assert_eq!(to(&retry.messages, 2), first);
    let response = reply(&mut nodes, first[0].clone());
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(response),
    );
    assert_eq!(to(&out.messages, 2).len(), 4);
}

#[test]
fn compaction_invalidates_windows_and_rewinds_optimism_to_the_proved_frontier() {
    let mut nodes = elected(4);
    let data = burst(&mut nodes, 2);
    pump(&mut nodes, data, &[1, 2]);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    pump(&mut nodes, out.messages, &[1, 2]);
    let outstanding = burst(&mut nodes, 6);
    let cut = SnapshotCut::from_authenticated_parts(&config(), [21; 32], [22; 32], [23; 32], 3, 1)
        .unwrap();
    publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Compact(cut));
    for old in to(&outstanding, 2) {
        let (term, request, _, _) = coordinates(&old);
        assert!(
            !nodes[&MemberId(1)]
                .pending_append_reply(MemberId(2), term, request)
                .unwrap()
        );
    }
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    let probe = to(&out.messages, 2);
    assert_eq!(probe.len(), 1);
    assert_eq!(coordinates(&probe[0]).2, 3);
    let snapshot = to(&out.messages, 3);
    assert_eq!(snapshot.len(), 1);
    let Message::InstallSnapshot { request, .. } = snapshot[0].message else {
        panic!()
    };
    assert!(
        !nodes[&MemberId(1)]
            .pending_append_reply(MemberId(3), 1, request)
            .unwrap()
    );
    // A forged wrong-kind append reply must not consume a snapshot flight.
    let ignored = receive(
        nodes.get_mut(&MemberId(1)).unwrap(),
        3,
        Message::Appended {
            term: 1,
            request,
            success: true,
            conflict_next: 0,
        },
    );
    assert!(ignored.messages.is_empty());
    let offer = publish(
        nodes.get_mut(&MemberId(3)).unwrap(),
        Event::Receive(snapshot[0].clone()),
    );
    let transfer = offer.snapshot_transfers[0].id();
    // The test model publishes an atomic installed root, not real disk I/O.
    let installed = publish(
        nodes.get_mut(&MemberId(3)).unwrap(),
        Event::SnapshotReady(transfer),
    );
    let out = publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(installed.messages[0].clone()),
    );
    assert_eq!(to(&out.messages, 3).len(), 3); // six suffix entries, batches of two
    assert_eq!(coordinates(&to(&out.messages, 3)[0]).2, 3);
    pump(&mut nodes, out.messages, &[1, 3]);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    pump(&mut nodes, out.messages, &[1, 2, 3]);
    for node in nodes.values() {
        assert_eq!(node.durable_state().unwrap().commit_index(), 9);
    }
}

#[test]
fn learners_and_pooled_joint_responses_never_replace_independent_majorities() {
    let config = Configuration::joint(
        Domain([7; 32]),
        [8; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [MemberId(6)],
    )
    .unwrap();
    let mut nodes = nodes(&config, 4);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::ElectionTimeout);
    pump(&mut nodes, out.messages, &[1, 2, 3, 4, 5, 6]);
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Heartbeat);
    pump(&mut nodes, out.messages, &[1, 2, 3, 4, 5, 6]);
    let initial = burst(&mut nodes, 4);
    for peer in [2, 4, 6] {
        let replies: Vec<_> = to(&initial, peer)
            .into_iter()
            .map(|m| reply(&mut nodes, m))
            .collect();
        publish(
            nodes.get_mut(&MemberId(1)).unwrap(),
            Event::Receive(replies.last().unwrap().clone()),
        );
    }
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        1
    );
    let replies: Vec<_> = to(&initial, 3)
        .into_iter()
        .map(|m| reply(&mut nodes, m))
        .collect();
    publish(
        nodes.get_mut(&MemberId(1)).unwrap(),
        Event::Receive(replies.last().unwrap().clone()),
    );
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        5
    );
}

#[test]
fn window_configuration_is_validated_and_cannot_discard_live_or_unpublished_requests() {
    let mut node = Raft::<u64>::new(MemberId(1), config(), Limits::default()).unwrap();
    for maximum in [0, 65, usize::MAX] {
        assert_eq!(
            node.configure_append_pipeline(maximum),
            Err(Error::InvalidLimits)
        );
        assert_eq!(node.append_pipeline_window(), 1);
    }
    node.configure_append_pipeline(4).unwrap();
    let token = node.step(Event::Heartbeat).unwrap().id();
    assert_eq!(
        node.configure_append_pipeline(2),
        Err(Error::AwaitingDurability)
    );
    node.persisted(token).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    assert_eq!(
        node.configure_append_pipeline(2),
        Err(Error::PipelineConfigurationBusy)
    );
    receive(
        &mut node,
        2,
        Message::Vote {
            term: 1,
            granted: true,
        },
    );
    assert_eq!(
        node.configure_append_pipeline(2),
        Err(Error::PipelineConfigurationBusy)
    );
    let state = node.durable_state().unwrap().clone();
    let recovered = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    assert_eq!(recovered.append_pipeline_window(), 1);
    assert!(!recovered.pending_append_reply(MemberId(2), 1, 1).unwrap());
}

#[test]
fn higher_term_fences_all_old_requests_and_preserves_committed_state() {
    let mut nodes = elected(4);
    let old = to(&burst(&mut nodes, 4), 2);
    receive(
        nodes.get_mut(&MemberId(1)).unwrap(),
        2,
        Message::Vote {
            term: 2,
            granted: false,
        },
    );
    assert_eq!(nodes[&MemberId(1)].role(), Ok(Role::Follower));
    for request in old {
        let (term, serial, _, _) = coordinates(&request);
        assert!(
            !nodes[&MemberId(1)]
                .pending_append_reply(MemberId(2), term, serial)
                .unwrap()
        );
    }
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        1
    );
}

#[test]
fn publication_failure_or_counter_exhaustion_never_releases_a_partial_window() {
    let mut nodes = elected(4);
    let pending = nodes
        .get_mut(&MemberId(1))
        .unwrap()
        .step(Event::Propose(99))
        .unwrap()
        .id();
    let node = nodes.get_mut(&MemberId(1)).unwrap();
    assert_eq!(
        node.pending_append_reply(MemberId(2), 1, 1),
        Err(Error::AwaitingDurability)
    );
    node.publication_failed();
    assert_eq!(node.persisted(pending).err(), Some(Error::RecoveryRequired));
    let mut nodes = elected(4);
    let node = nodes.get_mut(&MemberId(1)).unwrap();
    node.request = u64::MAX - 1;
    assert_eq!(
        node.step(Event::Propose(88)).err(),
        Some(Error::CounterExhausted)
    );
    assert_eq!(node.durable_state().err(), Some(Error::RecoveryRequired));
}

#[test]
fn inherited_old_term_windows_do_not_commit_until_a_current_term_entry_is_matched() {
    let configuration = config();
    let old = vec![
        Entry {
            term: 1,
            command: Some(11)
        };
        6
    ];
    let mut nodes = nodes(&configuration, 4);
    let mut leader = Raft::recover(
        MemberId(1),
        PersistentState::from_authenticated_parts(configuration, 1, None, 0, old),
        Limits {
            max_log_entries: 128,
            max_append_entries: 2,
        },
    )
    .unwrap();
    leader.configure_append_pipeline(4).unwrap();
    publish(&mut leader, Event::ElectionTimeout);
    let output = receive(
        &mut leader,
        2,
        Message::Vote {
            term: 2,
            granted: true,
        },
    );
    nodes.insert(MemberId(1), leader);
    let reject = reply(&mut nodes, to(&output.messages, 2)[0].clone());
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Receive(reject));
    let first = reply(&mut nodes, to(&out.messages, 2)[0].clone());
    let out = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Receive(first));
    assert_eq!(
        nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
        0
    );
    let requests = to(&out.messages, 2);
    assert_eq!(requests.len(), 3);
    for request in requests {
        let last = coordinates(&request).3;
        let response = reply(&mut nodes, request);
        publish(
            nodes.get_mut(&MemberId(1)).unwrap(),
            Event::Receive(response),
        );
        assert_eq!(
            nodes[&MemberId(1)].durable_state().unwrap().commit_index(),
            if last < 7 { 0 } else { 7 }
        );
    }
}

#[test]
fn draining_an_older_window_propagates_commit_without_waiting_for_another_heartbeat() {
    for window in [1, 2, 4, 16] {
        let mut nodes = elected(window);
        // Both peers initially receive data carrying the old commit frontier.
        // One peer commits it; the other's outstanding request cannot be
        // replaced or called acknowledged merely to advertise that new value.
        let messages = burst(&mut nodes, 20);
        pump(&mut nodes, messages, &[1, 2, 3]);
        for node in nodes.values() {
            assert_eq!(node.durable_state().unwrap().commit_index(), 21);
        }
        // Replies to the final notification must quiesce, not cause a new
        // empty append on every empty-append acknowledgement forever.
        for value in 30..40 {
            let output = publish(nodes.get_mut(&MemberId(1)).unwrap(), Event::Propose(value));
            pump(&mut nodes, output.messages, &[1, 2, 3]);
            let committed = nodes[&MemberId(1)].durable_state().unwrap().commit_index();
            assert!(
                nodes
                    .values()
                    .all(|node| node.durable_state().unwrap().commit_index() == committed)
            );
        }
    }
}
