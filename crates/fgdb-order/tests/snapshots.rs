//! Snapshot transfer, publication and absolute-index recovery regressions.
//! This driver persists the candidate before releasing any output; its in-memory
//! disk is a fault-test fixture, not a production durability implementation.
use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotCut,
};
use std::collections::{BTreeMap, VecDeque};

fn config() -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], (1..=3).map(MemberId), []).unwrap()
}

fn cut_for(configuration: &Configuration, index: u64, term: u64) -> SnapshotCut {
    SnapshotCut::from_authenticated_parts(configuration, [3; 32], [4; 32], [5; 32], index, term)
        .unwrap()
}

fn cut(index: u64, term: u64) -> SnapshotCut {
    cut_for(&config(), index, term)
}

fn publish(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

fn message(from: u128, to: u128, message: Message<u64>) -> Event<u64> {
    Event::Receive(Envelope {
        domain: Domain([1; 32]),
        configuration: [2; 32],
        from: MemberId(from),
        to: MemberId(to),
        message,
    })
}

fn offer(node: &mut Raft<u64>, request: u64, snapshot: SnapshotCut) -> Output<u64> {
    let to = node.id().0;
    publish(
        node,
        message(
            1,
            to,
            Message::InstallSnapshot {
                term: 4,
                request,
                snapshot,
            },
        ),
    )
}

#[test]
fn compacted_one_member_can_sequence_far_beyond_its_log_capacity() {
    let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap();
    let limits = Limits {
        max_log_entries: 4,
        max_append_entries: 2,
    };
    let mut node = Raft::new(MemberId(1), configuration.clone(), limits).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    let mut previous = 1;
    for command in 1..=100 {
        if node.durable_state().unwrap().entries().len() == 4 {
            let snapshot = cut_for(&configuration, previous, 1);
            let pending = node.step(Event::Compact(snapshot.clone())).unwrap();
            assert!(pending.requires_write());
            let id = pending.id();
            let output = node.persisted(id).unwrap();
            assert!(output.installed_snapshot.is_none());
            assert!(output.committed.is_empty());
            assert!(node.durable_state().unwrap().entries().is_empty());
            let pending = node.step(Event::Compact(snapshot)).unwrap();
            assert!(!pending.requires_write());
            let id = pending.id();
            node.persisted(id).unwrap();
        }
        let output = publish(&mut node, Event::Propose(command));
        previous += 1;
        assert_eq!(output.committed.len(), 1);
        assert_eq!(output.committed[0].index, previous);
        assert_eq!(output.committed[0].entry.command, Some(command));
        assert!(node.durable_state().unwrap().entries().len() <= limits.max_log_entries);
    }
    let saved = node.durable_state().unwrap().clone();
    let base = saved.snapshot().unwrap().index();
    let mut recovered = Raft::recover(MemberId(1), saved, limits).unwrap();
    assert_eq!(
        recovered.committed_after(base - 1),
        Err(Error::SnapshotRequired)
    );
    assert_eq!(
        recovered
            .committed_after(base)
            .unwrap()
            .last()
            .unwrap()
            .index,
        101
    );
    publish(&mut recovered, Event::ElectionTimeout);
    assert_eq!(recovered.durable_state().unwrap().commit_index(), 102);
}

#[test]
fn recovery_checks_cut_configuration_term_prefix_and_absolute_overflow() {
    let snapshot = cut(10, 3);
    for (term, commit, entries) in [
        (2, 10, vec![]),
        (3, 9, vec![]),
        (3, 11, vec![]),
        (
            3,
            10,
            vec![Entry {
                term: 2,
                command: Some(7),
            }],
        ),
        (
            3,
            10,
            vec![Entry {
                term: 4,
                command: Some(7),
            }],
        ),
    ] {
        let state = PersistentState::from_authenticated_snapshot(
            config(),
            term,
            None,
            commit,
            snapshot.clone(),
            entries,
        );
        assert_eq!(
            Raft::recover(MemberId(2), state, Limits::default()).err(),
            Some(Error::InvalidRecoveryState)
        );
    }
    let other = Configuration::stable(Domain([1; 32]), [9; 32], (1..=3).map(MemberId), []).unwrap();
    let state =
        PersistentState::<u64>::from_authenticated_snapshot(other, 3, None, 10, snapshot, vec![]);
    assert_eq!(
        Raft::recover(MemberId(2), state, Limits::default()).err(),
        Some(Error::InvalidRecoveryState)
    );
    let state = PersistentState::from_authenticated_snapshot(
        config(),
        3,
        None,
        u64::MAX - 1,
        cut(u64::MAX - 1, 3),
        vec![Entry {
            term: 3,
            command: Some(1),
        }],
    );
    assert_eq!(
        Raft::recover(MemberId(2), state, Limits::default()).err(),
        Some(Error::InvalidRecoveryState)
    );
    for (index, term) in [(0, 1), (1, 0), (u64::MAX, 1)] {
        assert_eq!(
            SnapshotCut::from_authenticated_parts(
                &config(),
                [3; 32],
                [4; 32],
                [5; 32],
                index,
                term
            ),
            Err(Error::InvalidSnapshot)
        );
    }
}

#[test]
fn compact_refuses_uncommitted_wrong_term_and_regressing_cuts_without_mutation() {
    let state = PersistentState::from_authenticated_parts(
        config(),
        2,
        None,
        1,
        vec![
            Entry {
                term: 1,
                command: Some(1),
            },
            Entry {
                term: 2,
                command: Some(2),
            },
        ],
    );
    let mut node = Raft::recover(MemberId(2), state.clone(), Limits::default()).unwrap();
    for snapshot in [cut(2, 2), cut(1, 2), cut(3, 2)] {
        assert_eq!(
            node.step(Event::Compact(snapshot)).err(),
            Some(Error::InvalidSnapshot)
        );
        assert_eq!(node.durable_state().unwrap(), &state);
    }
    publish(&mut node, Event::Compact(cut(1, 1)));
    assert_eq!(node.committed_after(0), Err(Error::SnapshotRequired));
    assert_eq!(node.durable_state().unwrap().entries()[0].command, Some(2));
    let different =
        SnapshotCut::from_authenticated_parts(&config(), [8; 32], [4; 32], [5; 32], 1, 1).unwrap();
    assert_eq!(
        node.step(Event::Compact(different)).err(),
        Some(Error::InvalidSnapshot)
    );
}

#[test]
fn offered_snapshot_is_not_installed_until_completion_and_root_publication() {
    let mut node = Raft::<u64>::new(MemberId(2), config(), Limits::default()).unwrap();
    let output = offer(&mut node, 7, cut(100, 3));
    assert!(output.messages.is_empty());
    assert_eq!(node.durable_state().unwrap().commit_index(), 0);
    assert!(node.durable_state().unwrap().snapshot().is_none());
    let transfer = output.snapshot_transfers[0].clone();
    assert_eq!(transfer.source(), MemberId(1));
    assert_eq!(transfer.snapshot(), &cut(100, 3));
    let pending = node.step(Event::SnapshotReady(transfer.id())).unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().snapshot(), Some(&cut(100, 3)));
    assert_eq!(pending.state().commit_index(), 100);
    let id = pending.id();
    assert_eq!(node.committed_after(100), Err(Error::AwaitingDurability));
    let output = node.persisted(id).unwrap();
    assert_eq!(output.installed_snapshot, Some(cut(100, 3)));
    assert!(output.committed.is_empty());
    assert!(matches!(
        output.messages[0].message,
        Message::SnapshotInstalled {
            term: 4,
            request: 7
        }
    ));
    assert_eq!(node.committed_after(99), Err(Error::SnapshotRequired));
    assert!(node.committed_after(100).unwrap().is_empty());
    assert_eq!(
        node.step(Event::SnapshotReady(transfer.id())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
}

#[test]
fn failed_install_publication_releases_no_acknowledgment() {
    let mut node = Raft::<u64>::new(MemberId(2), config(), Limits::default()).unwrap();
    let transfer = offer(&mut node, 1, cut(10, 3)).snapshot_transfers.remove(0);
    let id = node.step(Event::SnapshotReady(transfer.id())).unwrap().id();
    node.publication_failed();
    assert_eq!(node.persisted(id), Err(Error::RecoveryRequired));
    assert_eq!(node.durable_state().err(), Some(Error::RecoveryRequired));
}

#[test]
fn duplicate_offers_reuse_transfer_but_cancel_and_newer_offer_fence_old_tokens() {
    let mut node = Raft::<u64>::new(MemberId(2), config(), Limits::default()).unwrap();
    let first = offer(&mut node, 1, cut(10, 3)).snapshot_transfers.remove(0);
    let duplicate = offer(&mut node, 1, cut(10, 3)).snapshot_transfers.remove(0);
    assert_eq!(first.id(), duplicate.id());
    let second = offer(&mut node, 2, cut(20, 3)).snapshot_transfers.remove(0);
    assert!(
        offer(&mut node, 1, cut(10, 3))
            .snapshot_transfers
            .is_empty()
    );
    assert_eq!(
        node.step(Event::SnapshotReady(first.id())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    let cancelled = publish(&mut node, Event::SnapshotFailed(second.id()));
    assert!(cancelled.messages.is_empty());
    let retry = offer(&mut node, 2, cut(20, 3)).snapshot_transfers.remove(0);
    assert_ne!(second.id(), retry.id());
    assert_eq!(
        node.step(Event::SnapshotReady(second.id())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    publish(&mut node, Event::SnapshotReady(retry.id()));
    assert_eq!(node.durable_state().unwrap().commit_index(), 20);
}

#[test]
fn transfer_tokens_cannot_cross_nodes_restarts_or_leader_terms() {
    let mut node = Raft::<u64>::new(MemberId(2), config(), Limits::default()).unwrap();
    let mut other = Raft::<u64>::new(MemberId(3), config(), Limits::default()).unwrap();
    let token = offer(&mut node, 1, cut(10, 3))
        .snapshot_transfers
        .remove(0)
        .id();
    offer(&mut other, 1, cut(10, 3));
    assert_eq!(
        other.step(Event::SnapshotReady(token.clone())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    let disk = node.durable_state().unwrap().clone();
    let mut restarted = Raft::recover(MemberId(2), disk, Limits::default()).unwrap();
    offer(&mut restarted, 1, cut(10, 3));
    assert_eq!(
        restarted.step(Event::SnapshotReady(token.clone())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    publish(
        &mut node,
        message(
            3,
            2,
            Message::RequestVote {
                term: 5,
                last_index: 0,
                last_term: 0,
            },
        ),
    );
    assert_eq!(
        node.step(Event::SnapshotReady(token)).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    assert_eq!(node.durable_state().unwrap().commit_index(), 0);
}

#[test]
fn matching_snapshot_keeps_suffix_but_mismatched_snapshot_discards_it() {
    for snapshot_term in [2, 3] {
        let state = PersistentState::from_authenticated_parts(
            config(),
            4,
            None,
            1,
            vec![
                Entry {
                    term: 1,
                    command: Some(1),
                },
                Entry {
                    term: 2,
                    command: Some(2),
                },
                Entry {
                    term: 4,
                    command: Some(3),
                },
            ],
        );
        let mut node = Raft::recover(MemberId(2), state, Limits::default()).unwrap();
        let transfer = offer(&mut node, 1, cut(2, snapshot_term))
            .snapshot_transfers
            .remove(0);
        publish(&mut node, Event::SnapshotReady(transfer.id()));
        let suffix = node.durable_state().unwrap().entries();
        assert_eq!(suffix.len(), usize::from(snapshot_term == 2));
        if snapshot_term == 2 {
            assert_eq!(suffix[0].command, Some(3));
            let output = publish(
                &mut node,
                message(
                    1,
                    2,
                    Message::Append {
                        term: 4,
                        request: 2,
                        prev_index: 3,
                        prev_term: 4,
                        entries: vec![],
                        leader_commit: 3,
                    },
                ),
            );
            assert_eq!(output.committed[0].index, 3);
            assert_eq!(output.committed[0].entry.command, Some(3));
        }
    }
}

#[test]
fn committed_progress_obsoletes_transfer_and_late_offer_cannot_roll_back() {
    let state = PersistentState::from_authenticated_parts(
        config(),
        4,
        None,
        1,
        vec![
            Entry {
                term: 3,
                command: Some(1),
            },
            Entry {
                term: 3,
                command: Some(2),
            },
        ],
    );
    let mut node = Raft::recover(MemberId(2), state, Limits::default()).unwrap();
    let transfer = offer(&mut node, 1, cut(2, 3)).snapshot_transfers.remove(0);
    publish(
        &mut node,
        message(
            1,
            2,
            Message::Append {
                term: 4,
                request: 2,
                prev_index: 2,
                prev_term: 3,
                entries: vec![],
                leader_commit: 2,
            },
        ),
    );
    assert_eq!(
        node.step(Event::SnapshotReady(transfer.id())).err(),
        Some(Error::StaleSnapshotTransfer)
    );
    let output = offer(&mut node, 1, cut(2, 3));
    assert!(output.snapshot_transfers.is_empty());
    assert!(output.installed_snapshot.is_none());
    assert!(node.durable_state().unwrap().snapshot().is_none());
    assert!(matches!(
        output.messages[0].message,
        Message::SnapshotInstalled { request: 1, .. }
    ));
    assert_eq!(
        node.step(message(
            1,
            2,
            Message::InstallSnapshot {
                term: 4,
                request: 3,
                snapshot: cut(2, 2),
            }
        ))
        .err(),
        Some(Error::SnapshotConflict)
    );
}

#[test]
fn distant_append_produces_conflict_instead_of_lifetime_capacity_error() {
    let mut node = Raft::<u64>::new(
        MemberId(2),
        config(),
        Limits {
            max_log_entries: 4,
            max_append_entries: 2,
        },
    )
    .unwrap();
    let output = publish(
        &mut node,
        message(
            1,
            2,
            Message::Append {
                term: 4,
                request: 1,
                prev_index: 1_000_000,
                prev_term: 3,
                entries: vec![],
                leader_commit: 1_000_000,
            },
        ),
    );
    assert!(matches!(
        output.messages[0].message,
        Message::Appended {
            success: false,
            conflict_next: 1,
            ..
        }
    ));
    assert_eq!(node.durable_state().unwrap().commit_index(), 0);
}

#[test]
fn large_absolute_indices_use_bounded_append_and_commit_offsets() {
    let base = 1_000_000;
    let state =
        PersistentState::from_authenticated_snapshot(config(), 4, None, base, cut(base, 3), vec![]);
    let mut node = Raft::recover(
        MemberId(2),
        state,
        Limits {
            max_log_entries: 2,
            max_append_entries: 2,
        },
    )
    .unwrap();
    let output = publish(
        &mut node,
        message(
            1,
            2,
            Message::Append {
                term: 4,
                request: 1,
                prev_index: base,
                prev_term: 3,
                entries: vec![
                    Entry {
                        term: 4,
                        command: Some(10),
                    },
                    Entry {
                        term: 4,
                        command: Some(20),
                    },
                ],
                leader_commit: base + 2,
            },
        ),
    );
    assert_eq!(
        output
            .committed
            .iter()
            .map(|item| item.index)
            .collect::<Vec<_>>(),
        vec![base + 1, base + 2]
    );
    let saved = node.durable_state().unwrap().clone();
    let pressure = publish(
        &mut node,
        message(
            1,
            2,
            Message::Append {
                term: 4,
                request: 2,
                prev_index: base + 2,
                prev_term: 4,
                entries: vec![Entry {
                    term: 4,
                    command: Some(30),
                }],
                leader_commit: base + 3,
            },
        ),
    );
    assert_eq!(pressure.requested_compaction_through, Some(base + 1));
    assert!(pressure.messages.is_empty());
    assert!(pressure.committed.is_empty());
    assert_eq!(node.durable_state().unwrap(), &saved);
}

#[test]
fn three_member_catchup_survives_lost_install_ack_and_restart() {
    let limits = Limits {
        max_log_entries: 16,
        max_append_entries: 2,
    };
    let initial =
        PersistentState::from_authenticated_snapshot(config(), 3, None, 100, cut(100, 3), vec![]);
    let mut nodes = BTreeMap::from([
        (
            MemberId(1),
            Raft::recover(MemberId(1), initial, limits).unwrap(),
        ),
        (
            MemberId(2),
            Raft::new(MemberId(2), config(), limits).unwrap(),
        ),
        (
            MemberId(3),
            Raft::new(MemberId(3), config(), limits).unwrap(),
        ),
    ]);
    let mut disk = BTreeMap::new();
    let mut queue = VecDeque::from([(MemberId(1), Event::ElectionTimeout)]);
    let mut transfers = 0;
    let mut lost_ack = false;
    for round in 0..=6 {
        let mut budget = 1000;
        while let Some((member, event)) = queue.pop_front() {
            assert!(budget > 0, "snapshot catch-up did not quiesce");
            budget -= 1;
            let node = nodes.get_mut(&member).unwrap();
            let pending = node.step(event).unwrap();
            disk.insert(member, pending.state().clone());
            let id = pending.id();
            let output = node.persisted(id).unwrap();
            for transfer in output.snapshot_transfers {
                transfers += 1;
                queue.push_back((member, Event::SnapshotReady(transfer.id())));
            }
            if member == MemberId(2) && output.installed_snapshot.is_some() && !lost_ack {
                // Crash after installation publication but before its reply.
                *node = Raft::recover(member, disk[&member].clone(), limits).unwrap();
                lost_ack = true;
                continue;
            }
            for envelope in output.messages {
                queue.push_back((envelope.to, Event::Receive(envelope)));
            }
        }
        assert_eq!(nodes[&MemberId(1)].role().unwrap(), Role::Leader);
        if round < 5 {
            queue.push_back((MemberId(1), Event::Propose(10 + round)));
        } else if round == 5 {
            queue.push_back((MemberId(1), Event::Heartbeat));
        }
    }
    assert!(lost_ack);
    assert_eq!(
        transfers, 2,
        "a durable installed cut must not be downloaded twice"
    );
    for state in disk.values() {
        assert_eq!(state.snapshot(), Some(&cut(100, 3)));
        assert_eq!(state.commit_index(), 106);
        assert_eq!(state.entries().len(), 6);
        assert_eq!(
            state.entries()[0],
            Entry {
                term: 4,
                command: None
            }
        );
        assert_eq!(state.entries()[5].command, Some(14));
    }
}

#[test]
fn capacity_limited_follower_publishes_only_matching_prefix_then_retries_after_restart() {
    let limits = Limits {
        max_log_entries: 2,
        max_append_entries: 2,
    };
    let before = PersistentState::from_authenticated_parts(
        config(),
        1,
        Some(MemberId(2)),
        0,
        vec![Entry {
            term: 1,
            command: Some(11),
        }],
    );
    let mut node = Raft::recover(MemberId(2), before, limits).unwrap();
    let append = || {
        message(
            1,
            2,
            Message::Append {
                term: 2,
                request: 70,
                prev_index: 1,
                prev_term: 1,
                entries: vec![
                    Entry {
                        term: 2,
                        command: Some(22),
                    },
                    Entry {
                        term: 2,
                        command: Some(33),
                    },
                ],
                leader_commit: 3,
            },
        )
    };

    let pending = node.step(append()).unwrap();
    assert!(pending.requires_write());
    let saved = pending.state().clone();
    assert_eq!(
        (saved.term(), saved.voted_for(), saved.commit_index()),
        (2, None, 2)
    );
    assert_eq!(saved.entries().len(), 2);
    let token = pending.id();
    let first = node.persisted(token).unwrap();
    assert_eq!(first.requested_compaction_through, Some(1));
    assert!(first.messages.is_empty());
    assert_eq!(
        first
            .committed
            .iter()
            .map(|c| c.entry.command)
            .collect::<Vec<_>>(),
        [Some(11), Some(22)]
    );

    // Reopen exactly the state the operation could have durably published
    // before a crash. Retrying a full request never produces a false ack.
    let mut node = Raft::recover(MemberId(2), saved, limits).unwrap();
    let retry = node.step(append()).unwrap();
    assert!(!retry.requires_write());
    let token = retry.id();
    let retry = node.persisted(token).unwrap();
    assert_eq!(retry.requested_compaction_through, Some(1));
    assert!(retry.messages.is_empty());
    assert!(retry.committed.is_empty());

    publish(&mut node, Event::Compact(cut(1, 1)));
    let complete = publish(&mut node, append());
    assert_eq!(complete.requested_compaction_through, None);
    assert_eq!(complete.committed.len(), 1);
    assert_eq!(
        (
            complete.committed[0].index,
            complete.committed[0].entry.command
        ),
        (3, Some(33))
    );
    assert!(matches!(
        complete.messages.as_slice(),
        [Envelope {
            message: Message::Appended {
                term: 2,
                request: 70,
                success: true,
                ..
            },
            ..
        }]
    ));
    assert_eq!(node.durable_state().unwrap().entries().len(), 2);
}

#[test]
fn higher_term_append_demotes_a_full_leader_before_it_can_serve_again() {
    let limits = Limits {
        max_log_entries: 1,
        max_append_entries: 1,
    };
    let mut node = Raft::<u64>::new(MemberId(2), config(), limits).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    let election = publish(
        &mut node,
        message(
            1,
            2,
            Message::Vote {
                term: 1,
                granted: true,
            },
        ),
    );
    assert_eq!(election.role, fgdb_order::Role::Leader);

    let incoming = message(
        1,
        2,
        Message::Append {
            term: 2,
            request: 100,
            prev_index: 1,
            prev_term: 1,
            entries: vec![Entry {
                term: 2,
                command: Some(123),
            }],
            leader_commit: 2,
        },
    );
    let pending = node.step(incoming).unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().term(), 2);
    assert_eq!(pending.state().commit_index(), 1);
    let token = pending.id();
    let outcome = node.persisted(token).unwrap();
    assert_eq!(outcome.role, fgdb_order::Role::Follower);
    assert_eq!(outcome.leader, Some(MemberId(1)));
    assert!(outcome.reset_election_timer);
    assert_eq!(outcome.requested_compaction_through, Some(1));
    assert!(outcome.messages.is_empty());
    assert!(matches!(
        node.step(Event::Propose(1)),
        Err(Error::NotLeader)
    ));
}

#[test]
fn verified_compaction_after_election_unblocks_reads_without_a_client_write() {
    let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap();
    let limits = Limits {
        max_log_entries: 2,
        max_append_entries: 1,
    };
    let state = PersistentState::from_authenticated_parts(
        configuration.clone(),
        1,
        None,
        1,
        vec![
            Entry {
                term: 1,
                command: Some(11),
            },
            Entry {
                term: 1,
                command: Some(22),
            },
        ],
    );
    let mut node = Raft::recover(MemberId(1), state, limits).unwrap();
    let election = publish(&mut node, Event::ElectionTimeout);
    assert_eq!(election.requested_compaction_through, Some(1));
    assert!(matches!(
        node.begin_read_index(),
        Err(fgdb_order::ReadIndexError::CurrentTermNotCommitted)
    ));
    assert!(matches!(
        node.step(Event::IncreaseLogCapacity(1)),
        Err(Error::InvalidLimits)
    ));
    let snapshot = cut_for(&configuration, 1, 1);
    let pending = node.step(Event::Compact(snapshot.clone())).unwrap();
    assert!(pending.requires_write());
    let token = pending.id();
    let committed = node.persisted(token).unwrap();
    assert_eq!(
        committed
            .committed
            .iter()
            .map(|entry| (entry.index, entry.entry.command))
            .collect::<Vec<_>>(),
        [(2, Some(22)), (3, None)]
    );
    let mut round = node.begin_read_index().unwrap();
    assert_eq!(round.try_complete(&node).unwrap().unwrap().index(), 3);
    // Replayed admin calls and the normal heartbeat never append another fence.
    for event in [
        Event::Compact(snapshot),
        Event::IncreaseLogCapacity(2),
        Event::Heartbeat,
    ] {
        let pending = node.step(event).unwrap();
        assert!(!pending.requires_write());
        let token = pending.id();
        assert!(node.persisted(token).unwrap().committed.is_empty());
    }
}

#[test]
fn growing_a_full_leader_during_handoff_keeps_the_frozen_log_endpoint() {
    let state = PersistentState::from_authenticated_parts(
        config(),
        1,
        None,
        0,
        vec![Entry {
            term: 1,
            command: Some(17),
        }],
    );
    let limits = Limits {
        max_log_entries: 1,
        max_append_entries: 1,
    };
    let mut node = Raft::recover(MemberId(1), state, limits).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    publish(
        &mut node,
        message(
            2,
            1,
            Message::Vote {
                term: 2,
                granted: true,
            },
        ),
    );
    publish(&mut node, Event::TransferLeadership(MemberId(2)));
    let handoff = node.leadership_transfer().unwrap().unwrap().id();
    publish(&mut node, Event::IncreaseLogCapacity(2));
    publish(&mut node, Event::Heartbeat);
    assert_eq!(node.durable_state().unwrap().entries().len(), 1);
    publish(&mut node, Event::AbortLeadershipTransfer(handoff));
    let append = node.step(Event::Heartbeat).unwrap();
    assert!(append.requires_write());
    assert_eq!(
        append.state().entries(),
        [
            Entry {
                term: 1,
                command: Some(17)
            },
            Entry {
                term: 2,
                command: None
            }
        ]
    );
    assert_eq!(append.state().commit_index(), 0);
    let token = append.id();
    node.persisted(token).unwrap();
}
