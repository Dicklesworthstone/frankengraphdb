use std::cell::RefCell;
use std::rc::Rc;

use asupersync::runtime::RuntimeBuilder;
use fgdb_order::{Domain, Entry, Envelope, Error as OrderError, Message};
use fgdb_repl::application::member::MemberError;
use fgdb_repl::application::{ApplicationBatch, ApplicationProgress, AppliedPosition};
use fgdb_repl::replica::ReplicaError;
use fgdb_types::ObjectId;

use super::*;

struct Root {
    progress: ApplicationProgress,
    values: Vec<u64>,
    batches: Vec<(u64, u64)>,
    lose_reply: bool,
    raft: PersistentState<u64>,
}

#[derive(Clone)]
struct Backend(Rc<RefCell<Root>>);

impl Application<u64> for Backend {
    type Error = &'static str;

    async fn load(&mut self) -> Result<ApplicationProgress, Self::Error> {
        Ok(self.0.borrow().progress)
    }

    async fn apply(
        &mut self,
        batch: ApplicationBatch<'_, u64>,
    ) -> Result<ApplicationProgress, Self::Error> {
        let mut root = self.0.borrow_mut();
        assert_eq!(batch.basis(), &root.progress);
        assert_eq!(batch.first_index(), root.progress.applied.index + 1);
        root.values
            .extend(batch.commands().map(|(_, value)| *value));
        root.batches.push((batch.first_index(), batch.last().index));
        root.progress.applied = batch.last();
        // Index three starts an audit-hidden suffix.
        root.progress.visible_index = batch.last().index.min(2);
        root.progress.state_root = ObjectId([batch.last().index as u8; 32]);
        root.progress.publication_generation += 1;
        root.progress.publication_root = ObjectId([root.progress.publication_generation as u8; 32]);
        if std::mem::take(&mut root.lose_reply) {
            return Err("publication reply lost");
        }
        Ok(root.progress)
    }
}

impl RaftPublisher<u64> for Backend {
    type Error = core::convert::Infallible;

    async fn publish(&mut self, state: &PersistentState<u64>) -> Result<(), Self::Error> {
        self.0.borrow_mut().raft = state.clone();
        Ok(())
    }
}

#[test]
fn restart_resumes_an_ambiguous_publication_and_preserves_hidden_and_uncommitted_suffixes() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    runtime.block_on(async {
        let voters = [MemberId(1), MemberId(2), MemberId(3)];
        let config = Configuration::stable(Domain([1; 32]), [2; 32], voters, []).unwrap();
        let entries = [None, Some(42), Some(84), Some(126), Some(999)]
            .into_iter()
            .map(|command| Entry { term: 1, command })
            .collect();
        let state = PersistentState::from_authenticated_parts(
            config.clone(),
            1,
            Some(voters[0]),
            4,
            entries,
        );
        let backend = Backend(Rc::new(RefCell::new(Root {
            progress: ApplicationProgress {
                domain: config.domain(),
                configuration: config.identity(),
                applied: AppliedPosition { index: 0, term: 0 },
                visible_index: 0,
                state_root: ObjectId([0; 32]),
                publication_root: ObjectId([1; 32]),
                publication_generation: 1,
            },
            batches: Vec::new(),
            values: Vec::new(),
            lose_reply: true,
            raft: state.clone(),
        })));

        let first = AegisReplicaCoordinator::recover_application(
            voters[0],
            &config,
            state.clone(),
            Limits::default(),
            8,
            2,
            backend.clone(),
        )
        .await;
        assert!(matches!(
            first,
            Err(ApplicationError::Backend("publication reply lost"))
        ));
        assert_eq!(backend.0.borrow().progress.applied.index, 2);
        assert_eq!(backend.0.borrow().values, [42]);

        let mut recovered = AegisReplicaCoordinator::recover_application(
            voters[0],
            &config,
            state,
            Limits::default(),
            8,
            2,
            backend.clone(),
        )
        .await
        .unwrap();
        assert_eq!(backend.0.borrow().batches, [(1, 2), (3, 4)]);
        assert_eq!(backend.0.borrow().values, [42, 84, 126]);
        assert_eq!(
            recovered.progress().unwrap().applied,
            AppliedPosition { index: 4, term: 1 }
        );
        assert_eq!(recovered.progress().unwrap().visible_index, 2);
        assert!(recovered.apply_next().await.unwrap().is_none());

        // The returned owner can rejoin the group immediately. Only properly
        // attributed traffic may advance its term or publish a vote.
        let vote = Envelope {
            domain: config.domain(),
            configuration: config.identity(),
            from: voters[1],
            to: voters[0],
            message: Message::RequestVote {
                term: 2,
                last_index: 5,
                last_term: 1,
            },
        };
        assert!(matches!(
            recovered.receive(voters[2], vote.clone()).await,
            Err(MemberError::Replica(ReplicaError::Raft(
                OrderError::InvalidMessage
            )))
        ));
        assert_eq!(backend.0.borrow().raft.term(), 1);
        let output = recovered.receive(voters[1], vote).await.unwrap();
        assert_eq!(backend.0.borrow().raft.voted_for(), Some(voters[1]));
        assert!(matches!(output.consensus.messages.as_slice(), [reply]
            if matches!(reply.message, Message::Vote { term: 2, granted: true })));
    });
}
