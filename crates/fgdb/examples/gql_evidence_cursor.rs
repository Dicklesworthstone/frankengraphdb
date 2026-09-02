//! **Audit once, checkpoint, and resume an owned exact-result cursor.**
//!
//! ```text
//! cargo run -p fgdb --example gql_evidence_cursor
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlEvidenceCursorError, GqlEvidenceCursorState};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:KNOWS]->(b) RETURN b";

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!(
        "fgdb-evidence-cursor-example-{}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x4a; 32],
        DatabaseSecurityNamespaceId([0x5b; 32]),
        [0x6c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut database = Database::create(cx, &path, keys).await?;
        let mut initial = WriteBatch::new(KNOWS);
        initial.create_vertex(VId(1), vec![], vec![]);
        for (vid, eid) in [(VId(2), EId(10)), (VId(3), EId(11)), (VId(4), EId(12))] {
            initial.create_vertex(vid, vec![], vec![]);
            initial.add_edge(eid, VId(1), vid, vec![]);
        }
        let snapshot = database.write(cx, initial).await?;

        let query = database
            .prepare_gql_query(QUERY, &RelationBind::new().with_relation("KNOWS", KNOWS))?;
        let artifact = database.execute_prepared_query_artifact_at(&query, snapshot)?;
        let bytes = artifact.to_bytes();
        let mut cursor = database.open_untrusted_prepared_query_artifact_cursor(&query, &bytes)?;

        let first = cursor.next_page(2)?;
        assert_eq!(first.rows(), &[VId(2), VId(3)]);
        let checkpoint = cursor
            .checkpoint_token()
            .expect("one historical row remains")
            .to_bytes();
        assert!(cursor.close());

        let mut later = WriteBatch::new(KNOWS);
        later.create_vertex(VId(5), vec![], vec![]);
        later.add_edge(EId(13), VId(1), VId(5), vec![]);
        database.write(cx, later).await?;

        let mut resumed = database.resume_untrusted_prepared_query_artifact_cursor(
            &query,
            &bytes,
            &checkpoint,
        )?;
        assert_eq!(resumed.position(), 2);
        let terminal = resumed.next_page(8)?;
        assert_eq!(terminal.rows(), &[VId(4)]);
        assert_eq!(resumed.state(), GqlEvidenceCursorState::Exhausted);
        assert!(matches!(
            resumed.next_page(1),
            Err(GqlEvidenceCursorError::Exhausted)
        ));

        println!("audited snapshot: {snapshot:?}");
        println!("first page: {:?}", first.rows());
        println!("checkpoint bytes: {}", checkpoint.len());
        println!(
            "resumed terminal page after live advance: {:?}",
            terminal.rows()
        );
        println!("final state: {:?}", resumed.state());
        println!("OK: one audit per open, portable checkpoint, explicit exhaustion");
        Ok(())
    })
}
