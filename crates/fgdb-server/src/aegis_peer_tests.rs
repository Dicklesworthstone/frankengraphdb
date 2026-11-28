use super::*;
use asupersync::runtime::RuntimeBuilder;
use std::collections::VecDeque;

#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod bonded;

struct Member {
    coordinator: AegisReplicaCoordinator<u64>,
    publisher: SingleMemberPublisher<u64>,
}

#[test]
fn lagging_member_installs_the_compacted_cut_and_recovers_its_suffix() {
    use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile, encode_container};
    use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec};
    use fgdb_chronicle::store::RootPublicationEvidence;
    use fgdb_repl::SnapshotPublication;
    use fgdb_repl::driver::directory::{CapsuleDirectorySource, CapsuleSourceError};
    use fgdb_types::ObjectId;
    use fgdb_types::PurposeContexts;
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;

    // Capture the one installation image for recovery. The test exercises
    // real object verification and consensus; this publisher models storage.
    #[derive(Default)]
    struct Store {
        objects: BTreeSet<ObjectId>,
        installed: Option<(SeedAnchor, PersistentState<u64>)>,
    }
    impl SeedPublisher<u64> for Store {
        type Error = core::convert::Infallible;

        async fn publish_object(
            &mut self,
            object: ObjectPublication<'_>,
        ) -> Result<(), Self::Error> {
            self.objects.insert(object.object().object_id());
            Ok(())
        }
        async fn publish_snapshot(
            &mut self,
            snapshot: SnapshotPublication<'_, u64>,
        ) -> Result<RootPublicationEvidence, Self::Error> {
            let seed = snapshot.seed();
            let anchor = seed.plan().anchor();
            assert_eq!(
                self.objects,
                seed.incoming_encodings().map(|(id, _)| id).collect()
            );
            assert_eq!(snapshot.consensus().commit_index(), anchor.raft_index);
            assert!(self.installed.is_none());
            self.installed = Some((anchor.clone(), snapshot.consensus().clone()));
            Ok(RootPublicationEvidence {
                written_index: 1,
                slot_generation: anchor.publication_generation,
                root_manifest_oid: anchor.publication_root.0,
            })
        }
    }

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).repl();
    runtime.block_on(async {
        let voters = [MemberId(1), MemberId(2), MemberId(3)];
        let config = Configuration::stable(Domain([1; 32]), [2; 32], voters, []).unwrap();
        let mut nodes = voters.map(|id| Member {
            coordinator: AegisReplicaCoordinator::from_configuration(
                id,
                config.clone(),
                Limits::default(),
                8,
            )
            .unwrap(),
            publisher: SingleMemberPublisher::new(),
        });
        let leader = &mut nodes[0];
        let election = leader
            .coordinator
            .campaign(&mut leader.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], election, &voters[..2]).await;
        let leader = &mut nodes[0];
        let proposal = leader
            .coordinator
            .propose(&mut leader.publisher, 42)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], proposal, &voters[..2]).await;

        let leader = &mut nodes[0];
        let state = leader.publisher.latest().unwrap();
        assert_eq!(state.commit_index(), 2);
        let objects = [1, 2, 3, 4].map(bonded::Fixture::new);
        // This closure also owns one ordinary native Chronicle capsule on a
        // mounted donor. Its file is the only source used for this object.
        let keys = CapsuleKeys::new(
            bonded::KEY,
            bonded::namespace(),
            bonded::DEK,
            bonded::KIND,
            CapsuleProfile::balanced(),
        );
        let payload = b"replicated commit capsule";
        let mut sealed = keys.seal(payload).unwrap();
        let capsule = SeedObjectSpec {
            object_id: sealed.object_id,
            object_kind: keys.object_kind(),
            compressed_len: payload.len() as u64,
        };
        let directory =
            std::env::temp_dir().join(format!("fgdb-aegis-donor-{}", std::process::id()));
        let capsule_dir = directory.join(fgdb_chronicle::commit::CAPSULE_DIR);
        std::fs::create_dir_all(&capsule_dir).unwrap();
        let name: String = capsule
            .object_id
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = capsule_dir.join(format!("{name}.capsule"));
        // One bad source symbol is recoverable using the actual erasure decoder.
        *sealed.symbols[0].last_mut().unwrap() ^= 1;
        let container = encode_container(&sealed);
        std::fs::write(&path, &container).unwrap();

        let anchor = SeedAnchor {
            namespace: bonded::namespace(),
            consensus_domain: config.domain().0,
            configuration: config.identity(),
            snapshot_manifest: objects[0].encoding.object_id(),
            state_root: objects[1].encoding.object_id(),
            retention_floor: objects[2].encoding.object_id(),
            publication_root: objects[3].encoding.object_id(),
            publication_generation: 10,
            raft_index: state.commit_index(),
            raft_term: state.term(),
            logical_command_seq: 1,
            commit_seq: 1,
        };
        let cut = SnapshotCut::from_authenticated_parts(
            &config,
            anchor.snapshot_manifest.0,
            anchor.state_root.0,
            anchor.retention_floor.0,
            anchor.raft_index,
            anchor.raft_term,
        )
        .unwrap();
        leader
            .coordinator
            .compact(&mut leader.publisher, cut.clone())
            .await
            .unwrap();
        let pulse = leader
            .coordinator
            .heartbeat(&mut leader.publisher)
            .await
            .unwrap();
        let offer = pulse
            .consensus
            .messages
            .into_iter()
            .find(|m| m.to == voters[2])
            .unwrap();
        let lagging = &mut nodes[2];
        let mut offered = lagging
            .coordinator
            .receive(&mut lagging.publisher, voters[0], offer)
            .await
            .unwrap();
        assert!(offered.consensus.messages.is_empty());
        let transfer = offered.consensus.snapshot_transfers.pop().unwrap();
        let plan = SeedPlan::from_authenticated_inventory(
            anchor.clone(),
            objects
                .iter()
                .map(|o| SeedObjectSpec {
                    object_id: o.encoding.object_id(),
                    object_kind: bonded::KIND,
                    compressed_len: o.plaintext.len() as u64,
                })
                .chain([capsule]),
            SeedLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            lagging.coordinator.begin_snapshot_download(
                DatabaseSecurityNamespaceId([0; 32]),
                transfer.clone(),
                plan.clone()
            ),
            Err(CatchupError::WrongNamespace)
        ));
        let mut download = lagging
            .coordinator
            .begin_snapshot_download(anchor.namespace, transfer, plan)
            .unwrap();
        let mut store = Store::default();
        for object in &objects {
            download.stage(object.verified()).unwrap();
            download.publish_next(&mut store).await.unwrap();
        }
        let mut verification = Vec::new();
        let short = NonZeroUsize::new(container.len() - 1).unwrap();
        let mut source =
            CapsuleDirectorySource::new(&cx, &directory, &keys, short, &mut verification);
        assert!(matches!(
            download.recover_next(&mut source).await,
            Err(SeedDriveError::Source(CapsuleSourceError::Capsule(_)))
        ));
        assert_eq!(download.published_count(), objects.len());

        let mut source = CapsuleDirectorySource::new(
            &cx,
            &directory,
            &keys,
            NonZeroUsize::new(container.len()).unwrap(),
            &mut verification,
        );
        assert_eq!(
            download.recover_next(&mut source).await.unwrap(),
            Some(capsule.object_id)
        );
        download.publish_next(&mut store).await.unwrap();
        assert_eq!(download.recover_next(&mut source).await.unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), container);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(capsule_dir).unwrap();
        std::fs::remove_dir(directory).unwrap();

        let ack = lagging
            .coordinator
            .install_download(download, &mut store)
            .await
            .unwrap();
        assert_eq!(ack.consensus.installed_snapshot.as_ref(), Some(&cut));
        let (installed, state) = store.installed.unwrap();
        assert_eq!(installed, anchor);
        assert_eq!(state.snapshot(), Some(&cut));
        lagging.coordinator =
            AegisReplicaCoordinator::recover(voters[2], &config, state, Limits::default(), 8)
                .unwrap();
        deliver(&mut nodes, voters[2], ack, &voters).await;
        let leader = &mut nodes[0];
        let suffix = leader
            .coordinator
            .propose(&mut leader.publisher, 84)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], suffix, &voters).await;
        let leader = &mut nodes[0];
        let pulse = leader
            .coordinator
            .heartbeat(&mut leader.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], pulse, &voters).await;
        let recovered = nodes[2].coordinator.committed_after(cut.index()).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].index, 3);
        assert_eq!(recovered[0].entry.command, Some(84));
    });
}

impl Member {
    fn committed_commands(&self) -> Vec<u64> {
        self.coordinator
            .committed_after(0)
            .unwrap()
            .into_iter()
            .filter_map(|entry| entry.entry.command)
            .collect()
    }
}

// Route just the traffic that can cross the current partition. Take the sender
// identity from the sending coordinator, independently of the envelope header.
async fn deliver(
    members: &mut [Member],
    sender: MemberId,
    output: ReplicaOutput<u64>,
    online: &[MemberId],
) -> Vec<(ReadIndexId, u64)> {
    let mut pending = VecDeque::from([(sender, output)]);
    let mut ready = Vec::new();
    let mut deliveries = 0;
    while let Some((peer, output)) = pending.pop_front() {
        for read in output.reads {
            if let ReadResolution::Ready(floor) = read {
                ready.push((floor.id().clone(), floor.index()));
            }
        }
        for envelope in output.consensus.messages {
            if online.contains(&peer) && online.contains(&envelope.to) {
                deliveries += 1;
                assert!(deliveries < 4096, "replica delivery did not quiesce");
                let target = members
                    .iter_mut()
                    .find(|node| node.coordinator.member_id() == envelope.to)
                    .unwrap();
                let reply = target
                    .coordinator
                    .receive(&mut target.publisher, peer, envelope)
                    .await
                    .unwrap();
                pending.push_back((target.coordinator.member_id(), reply));
            }
        }
    }
    ready
}

#[test]
fn quorum_failover_and_recovery_replace_an_uncommitted_minority_suffix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    runtime.block_on(async {
        let voters = [MemberId(1), MemberId(2), MemberId(3)];
        let config = Configuration::stable(Domain([1; 32]), [2; 32], voters, []).unwrap();
        let mut nodes = voters.map(|id| Member {
            coordinator: AegisReplicaCoordinator::from_configuration(
                id,
                config.clone(),
                Limits::default(),
                8,
            )
            .unwrap(),
            publisher: SingleMemberPublisher::new(),
        });

        let first = &mut nodes[0];
        let election = first
            .coordinator
            .campaign(&mut first.publisher)
            .await
            .unwrap();
        let misattributed = election.consensus.messages[0].clone();
        let receiver = nodes
            .iter_mut()
            .find(|n| n.coordinator.member_id() == misattributed.to)
            .unwrap();
        assert!(matches!(
            receiver
                .coordinator
                .receive(&mut receiver.publisher, MemberId(9), misattributed)
                .await,
            Err(ReplicaError::Raft(OrderError::InvalidMessage))
        ));
        assert_eq!(receiver.publisher.publications(), 0);
        deliver(&mut nodes, voters[0], election, &voters).await;
        assert_eq!(nodes[0].coordinator.role(), Ok(Role::Leader));

        let first = &mut nodes[0];
        let proposal = first
            .coordinator
            .propose(&mut first.publisher, 42)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], proposal, &voters).await;
        let first = &mut nodes[0];
        let pulse = first
            .coordinator
            .heartbeat(&mut first.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], pulse, &voters).await;
        assert!(nodes.iter().all(|n| n.committed_commands() == [42]));

        // The old leader can publish its local suffix but cannot commit it.
        let first = &mut nodes[0];
        let minority = first
            .coordinator
            .propose(&mut first.publisher, 99)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], minority, &voters[..1]).await;
        assert_eq!(nodes[0].committed_commands(), [42]);

        // Both followers' local election deadlines expire during the partition.
        // The third member's discovery packets are delayed. It can now grant
        // the second's pre-vote without treating the old leader as recent.
        let third = &mut nodes[2];
        let _delayed = third
            .coordinator
            .liveness_timeout(&mut third.publisher)
            .await
            .unwrap();
        let second = &mut nodes[1];
        let election = second
            .coordinator
            .liveness_timeout(&mut second.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[1], election, &voters[1..]).await;
        assert_eq!(nodes[1].coordinator.role(), Ok(Role::Leader));
        let second = &mut nodes[1];
        let proposal = second
            .coordinator
            .propose(&mut second.publisher, 84)
            .await
            .unwrap();
        deliver(&mut nodes, voters[1], proposal, &voters[1..]).await;

        let root = nodes[0].publisher.latest().unwrap().clone();
        let foreign = Configuration::stable(config.domain(), [3; 32], voters, []).unwrap();
        assert!(matches!(
            AegisReplicaCoordinator::recover(
                voters[0],
                &foreign,
                root.clone(),
                Limits::default(),
                8
            ),
            Err(OrderError::WrongConfiguration)
        ));
        nodes[0].coordinator =
            AegisReplicaCoordinator::recover(voters[0], &config, root, Limits::default(), 8)
                .unwrap();
        assert_eq!(nodes[0].coordinator.role(), Ok(Role::Follower));

        for _ in 0..4 {
            let leader = &mut nodes[1];
            let heartbeat = leader
                .coordinator
                .heartbeat(&mut leader.publisher)
                .await
                .unwrap();
            deliver(&mut nodes, voters[1], heartbeat, &voters).await;
        }
        assert!(nodes.iter().all(|n| n.committed_commands() == [42, 84]));
        let leader = &mut nodes[1];
        let (read, probe) = leader
            .coordinator
            .read_index(&mut leader.publisher)
            .await
            .unwrap();
        assert!(probe.reads.is_empty());
        let floors = deliver(&mut nodes, voters[1], probe, &voters).await;
        let commit_index = nodes[1].publisher.latest().unwrap().commit_index();
        assert_eq!(floors, [(read, commit_index)]);
    });
}

#[test]
fn cancelled_and_delayed_read_rounds_release_one_slot_and_one_floor() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    runtime.block_on(async {
        let voters = [MemberId(1), MemberId(2), MemberId(3)];
        let config = Configuration::stable(Domain([1; 32]), [2; 32], voters, []).unwrap();
        let mut nodes = voters.map(|id| Member {
            coordinator: AegisReplicaCoordinator::from_configuration(
                id,
                config.clone(),
                Limits::default(),
                1,
            )
            .unwrap(),
            publisher: SingleMemberPublisher::new(),
        });
        let leader = &mut nodes[0];
        let election = leader
            .coordinator
            .campaign(&mut leader.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], election, &voters).await;

        // Hold all ordinary append replies. Dedicated probes still refer to
        // the published leader and do not advance either replica's data log.
        let leader = &mut nodes[0];
        let (old, _) = leader
            .coordinator
            .read_index(&mut leader.publisher)
            .await
            .unwrap();
        assert!(matches!(
            leader.coordinator.read_index(&mut leader.publisher).await,
            Err(ReplicaError::ReadBackpressure)
        ));
        let old_probe = leader.coordinator.read_index_probes(&old).unwrap()[0];
        let follower = &nodes[1].coordinator;
        assert_eq!(old_probe.to, follower.member_id());
        assert!(
            follower
                .acknowledge_read_index(voters[2], old_probe)
                .is_err()
        );
        let delayed = follower
            .acknowledge_read_index(voters[0], old_probe)
            .unwrap();

        let leader = &mut nodes[0];
        assert!(leader.coordinator.cancel_read(&old));
        assert!(!leader.coordinator.cancel_read(&old));
        let (current, _) = leader
            .coordinator
            .read_index(&mut leader.publisher)
            .await
            .unwrap();
        assert!(
            leader
                .coordinator
                .confirm_read_index(&old, voters[1], delayed)
                .is_err()
        );
        assert!(
            leader
                .coordinator
                .confirm_read_index(&current, voters[1], delayed)
                .is_err()
        );
        let probe = leader.coordinator.read_index_probes(&current).unwrap()[0];
        let ack = nodes[1]
            .coordinator
            .acknowledge_read_index(voters[0], probe)
            .unwrap();
        let leader = &mut nodes[0];
        assert!(
            leader
                .coordinator
                .confirm_read_index(&current, voters[2], ack)
                .is_err()
        );
        let ready = leader
            .coordinator
            .confirm_read_index(&current, voters[1], ack)
            .unwrap()
            .unwrap();
        assert_eq!(ready.id(), &current);
        assert_eq!(
            ready.index(),
            leader.publisher.latest().unwrap().commit_index()
        );
        assert!(
            leader
                .coordinator
                .confirm_read_index(&current, voters[1], ack)
                .is_err()
        );
        assert!(!leader.coordinator.cancel_read(&current));
    });
}

#[test]
fn planned_handoff_catches_up_a_voter_and_reopens_service_in_its_new_term() {
    use fgdb_order::LeadershipTransferPhase;
    use fgdb_repl::driver::SequenceError;

    let runtime = RuntimeBuilder::new().build().unwrap();
    runtime.block_on(async {
        let voters = [MemberId(1), MemberId(2), MemberId(3)];
        let config = Configuration::stable(Domain([1; 32]), [2; 32], voters, []).unwrap();
        let mut nodes = voters.map(|id| Member {
            coordinator: AegisReplicaCoordinator::from_configuration(
                id,
                config.clone(),
                Limits::default(),
                8,
            )
            .unwrap(),
            publisher: SingleMemberPublisher::new(),
        });
        let leader = &mut nodes[0];
        let election = leader
            .coordinator
            .campaign(&mut leader.publisher)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], election, &voters).await;

        // Only the old leader and the third voter receive the last command.
        let leader = &mut nodes[0];
        let old_term = leader.publisher.latest().unwrap().term();
        let proposal = leader
            .coordinator
            .propose(&mut leader.publisher, 42)
            .await
            .unwrap();
        deliver(&mut nodes, voters[0], proposal, &[voters[0], voters[2]]).await;
        assert_eq!(nodes[0].committed_commands(), [42]);
        assert!(nodes[1].committed_commands().is_empty());

        let leader = &mut nodes[0];
        let (pending, _) = leader
            .coordinator
            .read_index(&mut leader.publisher)
            .await
            .unwrap();
        let probe = leader.coordinator.read_index_probes(&pending).unwrap()[0];
        let ack = nodes[1]
            .coordinator
            .acknowledge_read_index(voters[0], probe)
            .unwrap();
        let leader = &mut nodes[0];
        leader
            .coordinator
            .transfer_leadership(&mut leader.publisher, voters[1])
            .await
            .unwrap();
        let old = leader
            .coordinator
            .leadership_transfer()
            .unwrap()
            .unwrap()
            .id();
        leader
            .coordinator
            .abort_leadership_transfer(&mut leader.publisher, old.clone())
            .await
            .unwrap();
        assert!(leader.coordinator.leadership_transfer().unwrap().is_none());

        let transfer = leader
            .coordinator
            .transfer_leadership(&mut leader.publisher, voters[1])
            .await
            .unwrap();
        let attempt = leader.coordinator.leadership_transfer().unwrap().unwrap();
        assert_eq!(attempt.target(), voters[1]);
        assert_eq!(attempt.phase(), LeadershipTransferPhase::CatchingUp);
        assert_ne!(attempt.id(), old);
        assert!(matches!(
            leader
                .coordinator
                .abort_leadership_transfer(&mut leader.publisher, old)
                .await,
            Err(ReplicaError::Sequence(SequenceError::Raft(
                OrderError::StaleLeadershipTransfer
            )))
        ));
        assert!(matches!(
            leader.coordinator.propose(&mut leader.publisher, 99).await,
            Err(ReplicaError::Sequence(SequenceError::Raft(
                OrderError::LeadershipTransferInProgress
            )))
        ));
        assert!(matches!(
            leader.coordinator.read_index(&mut leader.publisher).await,
            Err(ReplicaError::Raft(OrderError::LeadershipTransferInProgress))
        ));
        assert!(matches!(
            leader
                .coordinator
                .confirm_read_index(&pending, voters[1], ack),
            Err(ReadIndexError::Raft(
                OrderError::LeadershipTransferInProgress
            ))
        ));

        // Heal the target. The ordinary append/retry path carries the
        // missing command before the target can campaign and win a quorum.
        assert!(
            deliver(&mut nodes, voters[0], transfer, &voters)
                .await
                .is_empty()
        );
        for _ in 0..4 {
            if nodes[1].coordinator.role() == Ok(Role::Leader) {
                break;
            }
            let leader = &mut nodes[0];
            let pulse = leader
                .coordinator
                .heartbeat(&mut leader.publisher)
                .await
                .unwrap();
            assert!(
                deliver(&mut nodes, voters[0], pulse, &voters)
                    .await
                    .is_empty()
            );
        }
        assert_eq!(nodes[0].coordinator.role(), Ok(Role::Follower));
        assert!(
            nodes[0]
                .coordinator
                .leadership_transfer()
                .unwrap()
                .is_none()
        );
        assert_eq!(nodes[1].coordinator.role(), Ok(Role::Leader));
        let successor = &mut nodes[1];
        assert!(successor.publisher.latest().unwrap().term() > old_term);
        let proposal = successor
            .coordinator
            .propose(&mut successor.publisher, 84)
            .await
            .unwrap();
        deliver(&mut nodes, voters[1], proposal, &voters).await;
        let successor = &mut nodes[1];
        let (read, probes) = successor
            .coordinator
            .read_index(&mut successor.publisher)
            .await
            .unwrap();
        let floor = successor.publisher.latest().unwrap().commit_index();
        assert_eq!(
            deliver(&mut nodes, voters[1], probes, &voters).await,
            [(read, floor)]
        );
        assert!(nodes.iter().all(|n| n.committed_commands() == [42, 84]));
    });
}
