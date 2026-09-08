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
        let keys = DatabaseKeys::new(
            [0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await?;
        let knows = RelationId(1);
        let works_at = RelationId(2);
        // Cross-relation groups may share existing endpoints; they may not
        // depend on vertices another relation group has just created.
        let mut entities = WriteBatch::new(knows);
        for id in 1..=3 {
            entities.create_vertex(VId(id), vec![], vec![]);
        }
        let basis = db.write(&commit, entities).await?;
        let pinned = db.read_session()?;
        let names = RelationBind::new()
            .with_relation("KNOWS", knows)
            .with_relation("WORKS_AT", works_at);
        let query = db.prepare_gql_query(
            "MATCH (person)-[:KNOWS]->(friend)-[:WORKS_AT]->(company) RETURN company",
            &names,
        )?;
        let mut social = WriteBatch::new(knows);
        social.add_edge(EId(10), VId(1), VId(2), vec![]);
        let mut employment = WriteBatch::new(works_at);
        employment.add_edge(EId(20), VId(2), VId(3), vec![]);
        let mut txn = db.begin(&txn_cx)?;
        txn.write_atomic(&mut db, vec![employment, social])?;
        assert_eq!(txn.execute_prepared_query(&db, &query)?, vec![VId(3)]);
        assert!(db.execute_prepared_query(&query)?.is_empty());
        let sequence = txn.commit(&mut db, &commit).await?;
        assert_eq!(sequence, CommitSeq(basis.0 + 1));
        assert_eq!(db.delta_since(basis)?.count(), 1);
        assert_eq!(db.execute_prepared_query(&query)?, vec![VId(3)]);
        assert!(pinned.execute_prepared_query(&query)?.is_empty());
        assert!(db.execute_prepared_query_at(&query, basis)?.is_empty());
        let artifact = db.execute_prepared_query_artifact(&query)?;
        db.audit_prepared_query_artifact(&query, &artifact.to_bytes())?;
        println!("commit: {sequence:?}");
        println!("companies: {:?}", artifact.rows());
        println!("OK: two relations, one marker, pinned history preserved");
        Ok(())
    })
}
