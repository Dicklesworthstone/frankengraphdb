//! Automatically maintain recursive topology through actual database commits.
//!
//! Run: `cargo run -p fgdb --example standing_reachability`
//!
//! No explicit catch-up loop: registration joins the existing post-commit hook.
//! This uses production runtime contexts and durable create/write/reopen, not
//! LAB constructors. Views are session-local; reopening requires registration.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, StandingQueryError, StandingQueryFailure, WriteBatch};
use fgdb_delta_types::{RelationId, ZWeight};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

type Error = Box<dyn core::error::Error + Send + Sync>;
const R: RelationId = RelationId(1);
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}

fn run() -> Result<(), Error> {
    // Retained for inspection. Never delete or silently reuse a database path.
    let path = std::env::temp_dir().join(format!("fgdb-standing-reachability-{}", std::process::id()));
    let keys = DatabaseKeys::new([0x5a; 32], DatabaseSecurityNamespaceId([0x77; 32]), [0x3c; 32]);
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    runtime.block_on(async move {
        let mut db = Database::create(&commit, &path, keys.clone()).await?;
        let handle = db.register_standing_reachability(&query, R, policy(3))?;
        let mut first = WriteBatch::new(R);
        for id in 1..=3 { first.create_vertex(VId(id), vec![], vec![]); }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(1), VId(2), vec![]);
        first.add_edge(EId(12), VId(2), VId(3), vec![]);
        let basis = db.write(&commit, first).await?;
        let current = db.standing_reachability(&query, &handle)?;
        assert_eq!(current.frontier(), basis);
        assert_eq!(current.rows().len(), 3);
        assert_eq!(current.rows().weight(&(VId(1), VId(3))), Some(&ZWeight::ONE));

        let mut cycle = WriteBatch::new(R);
        cycle.add_edge(EId(13), VId(3), VId(1), vec![]);
        let at = db.write(&commit, cycle).await?;
        // The write is already durable. Only the derived view refuses its
        // nine-pair result, remaining explicitly unavailable at the old basis.
        assert!(matches!(db.standing_reachability(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert_eq!(db.frontier()?, at);
        db.rebuild_standing_query(&query, &handle, policy(9))?;
        assert_eq!(db.standing_reachability(&query, &handle)?.rows().len(), 9);

        let mut partial = WriteBatch::new(R);
        partial.delete_edge(EId(10));
        db.write(&commit, partial).await?;
        assert_eq!(db.standing_reachability(&query, &handle)?.rows().len(), 9);
        let mut last = WriteBatch::new(R);
        last.delete_edge(EId(11));
        db.write(&commit, last).await?;
        assert_eq!(db.standing_reachability(&query, &handle)?.rows().len(), 3);
        // Rebuild admits the CURRENT result, despite the larger past cycle.
        db.rebuild_standing_query(&query, &handle, policy(3))?;
        let expected: Vec<_> = db.standing_reachability(&query, &handle)?.rows()
            .iter().map(|(pair, _)| *pair).collect();
        let frontier = db.frontier()?;
        drop(db);

        let mut db = Database::open(&commit, &path, keys).await?;
        assert!(matches!(db.standing_reachability(&query, &handle), Err(StandingQueryError::ForeignHandle)));
        let reopened = db.register_standing_reachability(&query, R, policy(3))?;
        let view = db.standing_reachability(&query, &reopened)?;
        assert_eq!(view.frontier(), frontier);
        assert_eq!(view.rows().iter().map(|(pair, _)| *pair).collect::<Vec<_>>(), expected);
        let mut suffix = WriteBatch::new(R);
        suffix.delete_edge(EId(13));
        let at = db.write(&commit, suffix).await?;
        let view = db.standing_reachability(&query, &reopened)?;
        assert_eq!(view.frontier(), at);
        assert_eq!(view.rows().len(), 1);
        assert_eq!(view.rows().weight(&(VId(2), VId(3))), Some(&ZWeight::ONE));
        println!("OK: automatic recursive view at {at:?}; database {}", path.display());
        Ok(())
    })
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}
