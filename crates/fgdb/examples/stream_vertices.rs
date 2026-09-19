//! Run with `cargo run -p fgdb --example stream_vertices`.
//! A production-runtime example over the ordinary in-memory VFS composition.
//! The write path is real Chronicle/Strata, not a substitute graph fixture.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
use fgdb_gql::{GqlQueryPolicy, stream::VertexScanState};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn main() {
    let runtime = RuntimeBuilder::new().build().expect("production runtime");
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    runtime.block_on(async {
        let keys = DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32]);
        let mut database = Database::open_memory(&commit, keys).await.expect("create database");
        let name = PropertyKeyId(7);
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, "Ada"), (2, "Grace"), (3, "Katherine")] {
            seed.create_vertex(VId(id), vec![], vec![(name, CanonicalScalar::ucs_basic_text(value).unwrap())]);
        }
        database.write(&commit, seed).await.expect("commit seed");
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("person").unwrap();
        let prepared = builder.prepare_values(&[
            GraphColumn::vertex("id", "person"),
            GraphColumn::property("name", "person", name),
        ], 0, None).unwrap();
        let policy = GqlQueryPolicy::new(3, 3, 1000, 100);
        let mut stream = database.stream_graph_values_governed(&query, &prepared, policy).unwrap();
        assert_eq!(stream.row_stats().snapshot_records, 0);
        let first = stream.next().expect("first row").unwrap();
        assert_eq!(first.get(0).and_then(|value| value.as_vertex()), Some(VId(1)));
        assert_eq!(stream.row_stats().snapshot_records, 1);
        let pinned = stream.snapshot_seq();

        // The cursor does not borrow the mutable database. Change a row the
        // consumer has not requested yet; the pinned stream still sees Grace.
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(2), name, Some(CanonicalScalar::ucs_basic_text("updated").unwrap()));
        database.write(&commit, edit).await.expect("write during open stream");
        assert!(database.frontier().unwrap() > pinned);
        let second = stream.next().expect("second row").unwrap();
        assert_eq!(second.get(1).and_then(|value| value.as_scalar()),
            Some(&CanonicalScalar::ucs_basic_text("Grace").unwrap()));
        assert_eq!(stream.row_stats().snapshot_records, 2);

        // Stop without examining Katherine. No detached producer is running;
        // close releases the source generation immediately, not after a drain.
        stream.close();
        assert_eq!(stream.state(), VertexScanState::Closed);
        assert!(stream.next().is_none());
        assert_eq!(stream.row_stats().snapshot_records, 2);
        println!("streamed {} complete rows from {:?}; closed before the final candidate",
            stream.row_stats().result_rows, pinned);
    });
}
