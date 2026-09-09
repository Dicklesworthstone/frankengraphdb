//! cargo run -p fgdb --example atomic_relation_writes

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let txn_cx = contexts.txn();
    runtime.block_on(async move {
        // Fixed demonstration keys for a private transient database only.
        let keys = DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await?;
        let knows = RelationId(1);
        let works_at = RelationId(2);
        let basis = db.frontier()?;
        let pinned = db.read_session()?;
        let names = RelationBind::new()
            .with_relation("KNOWS", knows)
            .with_relation("WORKS_AT", works_at);
        let query = db.prepare_gql_query(
            "MATCH (person)-[:KNOWS]->(friend)-[:WORKS_AT]->(company) RETURN company",
            &names,
        )?;

        // The vertex-initialization prefix is explicit and first. Its source
        // relation need not sort before the edges: preparation emits the
        // canonical endpoint creations once in the earliest coordinate.
        let mut entities = WriteBatch::new(RelationId(9));
        for id in 1..=3 {
            entities.ensure_vertex(VId(id), vec![], vec![]);
        }
        let mut social = WriteBatch::new(knows);
        social.ensure_edge_by_triple(EId(10), VId(1), VId(2), vec![]);
        let mut employment = WriteBatch::new(works_at);
        employment.ensure_edge_by_triple(EId(20), VId(2), VId(3), vec![]);
        let groups = vec![entities, employment, social];

        let mut txn = db.begin(&txn_cx)?;
        txn.write_atomic(&mut db, groups.clone())?;
        assert_eq!(txn.execute_prepared_query(&db, &query)?, vec![VId(3)]);
        assert!(db.vertices()?.is_empty());
        assert!(db.edges()?.is_empty());
        let sequence = txn.commit(&mut db, &commit).await?;
        assert_eq!(basis, CommitSeq(0));
        assert_eq!(sequence, CommitSeq(1));
        assert_eq!(db.delta_since(basis)?.count(), 1);
        assert_eq!(db.vertices()?.len(), 3);
        assert_eq!(db.edges()?.len(), 2);
        assert_eq!(db.execute_prepared_query(&query)?, vec![VId(3)]);
        assert!(pinned.vertices()?.is_empty());
        assert!(pinned.execute_prepared_query(&query)?.is_empty());
        assert!(db.execute_prepared_query_at(&query, basis)?.is_empty());
        let artifact = db.execute_prepared_query_artifact(&query)?;
        db.audit_prepared_query_artifact(&query, &artifact.to_bytes())?;

        // Ensures make the graph operation idempotent. A repeated submission
        // still uses the ordinary commit protocol and may advance its marker;
        // this is not exactly-once request or transaction deduplication.
        let vertices = db.vertices()?;
        let edges = db.edges()?;
        let versions = db.element_versions()?.clone();
        let repeated = db.write_atomic(&commit, groups).await?;
        assert_eq!(db.vertices()?, vertices);
        assert_eq!(db.edges()?, edges);
        assert_eq!(db.element_versions()?, &versions);
        db.audit_prepared_query_artifact(&query, &artifact.to_bytes())?;
        println!("initial commit: {sequence:?}; repeated ensure commit: {repeated:?}");
        println!("companies: {:?}", artifact.rows());
        println!("OK: new vertices and two relations publish atomically; repeated ensures preserve graph state");
        Ok(())
    })
}
