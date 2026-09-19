//! Maintain a recursive topology view from actual durable database commits.
//!
//! Run: `cargo run -p fgdb --example incremental_reachability`
//!
//! This example uses the existing production runtime and database commit/reopen
//! path. It demonstrates whole-batch catch-up, a downstream refusal/retry,
//! cycles, partial parallel-edge retraction, unrelated relations and replay.
//! The maintained view is in-process and caller-driven, NOT durably registered
//! or automatically scheduled. The database's delta history remains authority.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::zset::reachability::committed::CommittedReachability;
use fgdb_delta_types::{LimbLimit, RelationId, ZSet, ZSetEvent, ZWeight};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, QueryCx, VId};

const KNOWS: RelationId = RelationId(1);
const LIMBS: LimbLimit = LimbLimit::new(16);
type Error = Box<dyn core::error::Error + Send + Sync>;

fn new_view() -> CommittedReachability {
    // This embedded database slice serves graph 1, branch 1.
    CommittedReachability::new(GraphId(1), BranchId(1), KNOWS)
}

fn catch_up(
    db: &Database,
    cx: &QueryCx,
    view: &mut CommittedReachability,
    sink: &mut ZSet<(VId, VId)>,
) -> Result<usize, Error> {
    cx.with_restriction(|| {
        let index = db.delta_index()?;
        let mut events = 0_u64;
        let mut control = |_: ZSetEvent| -> Result<(), std::io::Error> {
            cx.checkpoint().map_err(|error| std::io::Error::other(error.to_string()))?;
            events += 1;
            if events > 1_000_000 {
                return Err(std::io::Error::other("example catch-up event budget exhausted"));
            }
            Ok(())
        };
        let mut ticks = 0;
        while let Some(pending) = view.prepare_next(index, LIMBS, &mut control)? {
            // Nothing publishes until the sink has prepared too. A refusal
            // drops pending and leaves the same input batch available to retry.
            let output = sink.prepare_update(pending.delta(), LIMBS, &mut control)?;
            output.commit();
            let _delta = pending.commit();
            ticks += 1;
        }
        assert_eq!(view.frontier(), db.frontier()?);
        assert_eq!(view.pairs().count(), sink.len());
        for pair in view.pairs() {
            assert_eq!(sink.weight(&pair), Some(&ZWeight::ONE));
        }
        Ok(ticks)
    })
}

fn run() -> Result<(), Error> {
    // Retained for inspection; the example never deletes a database directory.
    let path = std::env::temp_dir().join(format!("fgdb-reachability-{}", std::process::id()));
    let keys = DatabaseKeys::new(
        [0x5a; 32], DatabaseSecurityNamespaceId([0x77; 32]), [0x3c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let commit_cx = &PurposeContexts::narrow_runtime_root(&root).commit();
    let query_cx = &PurposeContexts::narrow_runtime_root(&root).query();
    runtime.block_on(async move {
        let mut db = Database::create(commit_cx, &path, keys.clone()).await?;
        let mut view = new_view();
        let mut sink = ZSet::new();
        let mut batch = WriteBatch::new(KNOWS);
        for vertex in 1..=4 {
            batch.create_vertex(VId(vertex), vec![], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(2), vec![]);
        batch.add_edge(EId(12), VId(2), VId(3), vec![]);
        db.write(commit_cx, batch).await?;
        assert_eq!(catch_up(&db, query_cx, &mut view, &mut sink)?, 1);
        assert!(view.contains(VId(1), VId(3)));
        assert!(!view.contains(VId(1), VId(1)));
        assert_eq!(sink.len(), 3);

        let mut unrelated = WriteBatch::new(RelationId(2));
        unrelated.add_edge(EId(20), VId(4), VId(1), vec![]);
        db.write(commit_cx, unrelated).await?;
        assert_eq!(catch_up(&db, query_cx, &mut view, &mut sink)?, 1);
        assert!(!view.contains(VId(4), VId(1)));

        let old_frontier = view.frontier();
        let mut close_cycle = WriteBatch::new(KNOWS);
        close_cycle.add_edge(EId(13), VId(3), VId(1), vec![]);
        db.write(commit_cx, close_cycle).await?;
        {
            let mut allow = |_: ZSetEvent| Ok::<_, std::io::Error>(());
            let pending = view.prepare_next(db.delta_index()?, LIMBS, &mut allow)?
                .expect("the durable cycle commit has not been consumed");
            let rejected = sink.prepare_update(pending.delta(), LIMBS, &mut |_| {
                Err(std::io::Error::other("injected downstream refusal"))
            });
            assert!(rejected.is_err());
            // Neither guard commits. This does NOT roll back the database:
            // only the derived input/view stay at their previous frontier.
        }
        assert_eq!(view.frontier(), old_frontier);
        assert_eq!(sink.len(), 3);
        assert!(!view.contains(VId(1), VId(1)));
        catch_up(&db, query_cx, &mut view, &mut sink)?;
        assert_eq!(sink.len(), 9);

        let mut partial = WriteBatch::new(KNOWS);
        partial.delete_edge(EId(10));
        db.write(commit_cx, partial).await?;
        catch_up(&db, query_cx, &mut view, &mut sink)?;
        assert_eq!(sink.len(), 9, "one parallel edge still supports the cycle");
        let mut last = WriteBatch::new(KNOWS);
        last.delete_edge(EId(11));
        db.write(commit_cx, last).await?;
        catch_up(&db, query_cx, &mut view, &mut sink)?;
        assert_eq!(sink.len(), 3);
        assert!(!view.contains(VId(1), VId(1)), "a removed cycle cannot self-support");

        // Freshly recovered Chronicle history recreates the same input
        // identities, exact frontier and recursive view without saved state.
        drop(db);
        let mut db = Database::open(commit_cx, &path, keys).await?;
        let mut rebuilt = new_view();
        let mut rebuilt_sink = ZSet::new();
        assert_eq!(catch_up(&db, query_cx, &mut rebuilt, &mut rebuilt_sink)?, 5);
        assert_eq!(view, rebuilt);
        assert_eq!(sink, rebuilt_sink);
        assert_eq!(catch_up(&db, query_cx, &mut view, &mut sink)?, 0);

        let mut suffix = WriteBatch::new(KNOWS);
        suffix.delete_edge(EId(13));
        db.write(commit_cx, suffix).await?;
        catch_up(&db, query_cx, &mut view, &mut sink)?;
        catch_up(&db, query_cx, &mut rebuilt, &mut rebuilt_sink)?;
        assert_eq!(view, rebuilt);
        assert_eq!(sink, rebuilt_sink);
        assert_eq!(view.pairs().collect::<Vec<_>>(), vec![(VId(2), VId(3))]);
        println!("OK: recursive view at {:?}, durable database {}", view.frontier(), path.display());
        Ok(())
    })
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}
