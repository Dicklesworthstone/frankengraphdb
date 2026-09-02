//! Bounded embedded write transactions, decomposed by responsibility.
//!
//! Every included file is compiled in this module, so private transaction state
//! remains private while the source ownership map stays legible to agents:
//! lifecycle and staging, vertex reads, edge/adjacency reads, overlay GQL,
//! commit/conflict handling, and diagnostics/tests.

include!("write_txn_parts/preamble.rs");
include!("write_txn_parts/lifecycle.rs");
include!("write_txn_parts/vertex_reads.rs");
include!("write_txn_parts/edge_reads.rs");
include!("write_txn_parts/gql_types.rs");
include!("write_txn_parts/gql_entry.rs");
include!("write_txn_parts/gql_node.rs");
include!("write_txn_parts/gql_overlay_graph.rs");
include!("write_txn_parts/gql_edge_match.rs");
include!("write_txn_parts/gql_api.rs");
include!("write_txn_parts/owned_prepared.rs");
include!("write_txn_parts/overlay_evidence.rs");
include!("write_txn_parts/portable_evidence.rs");
include!("write_txn_parts/evidence_limits.rs");
include!("write_txn_parts/evidence_page.rs");
include!("write_txn_parts/evidence_cursor.rs");
include!("write_txn_parts/finish.rs");
include!("write_txn_parts/traits_and_tests.rs");
