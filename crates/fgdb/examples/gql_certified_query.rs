//! **A real `main()` that certifies a GQL result**.
//!
//! The bounded GQL executor can return a replayable certificate alongside the
//! result rows. The certificate binds the rows to this handle's published
//! frontier, the exact statement bytes, and the canonical bind encoding. This
//! example creates a small graph, runs one certified query, commits one more
//! fact, and shows that the certificate changes when the snapshot advances —
//! while the same (database state, statement, bind) triple reproduces byte-for-
//! byte.
//!
//! ```text
//! cargo run --example gql_certified_query
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!("fgdb-cert-example-{}", std::process::id()));
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );

    println!("fgdb GQL certified-query witness");
    println!("  database directory: {}", path.display());

    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut db = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        let first_seq = db.write(cx, batch).await?;
        println!("  committed first batch at seq {first_seq:?}");

        let bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let query = "MATCH (a)-[:KNOWS]->(b) RETURN b";
        let (rows_1, cert_1) = db.execute_gql_certified(query, &bind)?;
        println!("  certified rows at snapshot {snapshot:?}: {rows:?}",
                 snapshot = cert_1.snapshot_seq, rows = rows_1);
        println!("    statement digest: {statement_digest}",
                 statement_digest = cert_1.statement_digest);
        println!("    bind digest: {bind_digest}",
                 bind_digest = cert_1.bind_digest);

        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(4), vec![], vec![]);
        second.add_edge(EId(12), VId(1), VId(4), vec![]);
        let second_seq = db.write(cx, second).await?;
        println!("  committed second batch at seq {second_seq:?}");

        let (rows_2, cert_2) = db.execute_gql_certified(query, &bind)?;
        println!("  certified rows at snapshot {snapshot:?}: {rows:?}",
                 snapshot = cert_2.snapshot_seq, rows = rows_2);
        assert!(
            cert_1.snapshot_seq != cert_2.snapshot_seq,
            "a later commit must advance the certificate snapshot"
        );
        assert_eq!(
            cert_1.statement_digest, cert_2.statement_digest,
            "the same statement text must produce the same statement digest"
        );
        assert_eq!(
            cert_1.bind_digest, cert_2.bind_digest,
            "the same bind must produce the same bind digest"
        );
        assert_eq!(rows_1, vec![VId(2), VId(3)], "initial rows must match");
        assert_eq!(
            rows_2,
            vec![VId(2), VId(3), VId(4)],
            "rows must include the newly inserted destination"
        );

        println!("OK: certified rows advance with the snapshot and stay byte-identical for the same inputs.");
        Ok(())
    })
}
