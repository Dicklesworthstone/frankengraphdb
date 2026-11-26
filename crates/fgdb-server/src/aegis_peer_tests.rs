use super::*;
use asupersync::runtime::RuntimeBuilder;
use std::collections::VecDeque;

struct Member {
    coordinator: AegisReplicaCoordinator<u64>,
    publisher: SingleMemberPublisher<u64>,
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
                id, config.clone(), Limits::default(), 8,
            )
            .unwrap(),
            publisher: SingleMemberPublisher::new(),
        });

        let first = &mut nodes[0];
        let election = first.coordinator.campaign(&mut first.publisher).await.unwrap();
        let misattributed = election.consensus.messages[0].clone();
        let receiver = nodes.iter_mut().find(|n| n.coordinator.member_id() == misattributed.to).unwrap();
        assert!(matches!(
            receiver.coordinator.receive(&mut receiver.publisher, MemberId(9), misattributed).await,
            Err(ReplicaError::Raft(OrderError::InvalidMessage))
        ));
        assert_eq!(receiver.publisher.publications(), 0);
        deliver(&mut nodes, voters[0], election, &voters).await;
        assert_eq!(nodes[0].coordinator.role(), Ok(Role::Leader));

        let first = &mut nodes[0];
        let proposal = first.coordinator.propose(&mut first.publisher, 42).await.unwrap();
        deliver(&mut nodes, voters[0], proposal, &voters).await;
        let first = &mut nodes[0];
        let pulse = first.coordinator.heartbeat(&mut first.publisher).await.unwrap();
        deliver(&mut nodes, voters[0], pulse, &voters).await;
        assert!(nodes.iter().all(|n| n.committed_commands() == [42]));

        // The old leader can publish its local suffix but cannot commit it.
        let first = &mut nodes[0];
        let minority = first.coordinator.propose(&mut first.publisher, 99).await.unwrap();
        deliver(&mut nodes, voters[0], minority, &voters[..1]).await;
        assert_eq!(nodes[0].committed_commands(), [42]);

        // Both followers' local election deadlines expire during the partition.
        // The third member's discovery packets are delayed. It can now grant
        // the second's pre-vote without treating the old leader as recent.
        let third = &mut nodes[2];
        let _delayed = third.coordinator.liveness_timeout(&mut third.publisher).await.unwrap();
        let second = &mut nodes[1];
        let election = second.coordinator.liveness_timeout(&mut second.publisher).await.unwrap();
        deliver(&mut nodes, voters[1], election, &voters[1..]).await;
        assert_eq!(nodes[1].coordinator.role(), Ok(Role::Leader));
        let second = &mut nodes[1];
        let proposal = second.coordinator.propose(&mut second.publisher, 84).await.unwrap();
        deliver(&mut nodes, voters[1], proposal, &voters[1..]).await;

        let root = nodes[0].publisher.latest().unwrap().clone();
        let foreign = Configuration::stable(config.domain(), [3; 32], voters, []).unwrap();
        assert!(matches!(
            AegisReplicaCoordinator::recover(voters[0], &foreign, root.clone(), Limits::default(), 8),
            Err(OrderError::WrongConfiguration)
        ));
        nodes[0].coordinator = AegisReplicaCoordinator::recover(
            voters[0], &config, root, Limits::default(), 8,
        ).unwrap();
        assert_eq!(nodes[0].coordinator.role(), Ok(Role::Follower));

        for _ in 0..4 {
            let leader = &mut nodes[1];
            let heartbeat = leader.coordinator.heartbeat(&mut leader.publisher).await.unwrap();
            deliver(&mut nodes, voters[1], heartbeat, &voters).await;
        }
        assert!(nodes.iter().all(|n| n.committed_commands() == [42, 84]));
        let leader = &mut nodes[1];
        let (read, probe) = leader.coordinator.read_index(&mut leader.publisher).await.unwrap();
        assert!(probe.reads.is_empty());
        let floors = deliver(&mut nodes, voters[1], probe, &voters).await;
        let commit_index = nodes[1].publisher.latest().unwrap().commit_index();
        assert_eq!(floors, [(read, commit_index)]);
    });
}
