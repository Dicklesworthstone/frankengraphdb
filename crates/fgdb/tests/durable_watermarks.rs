//! E2E verification and fault-injection negative controls for durable
//! Chronicle-watermarked Beacon indexes and Ripple standing-query Z-set
//! arrangements (`fgdb-rc1111-durable-watermarks-c1g98`, `FG-INV-18`).

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedCheckpointOutcome, DerivedRebuildReason, MemVfs, WriteBatch,
};
use fgdb_beacon::read::{ReadOptions, ReadPolicy, Search};
use fgdb_beacon::{DistanceMetric, HnswConfig, TextMatch, VectorSearch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const DOC: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const LINKS: RelationId = RelationId(1);

type BeaconOptions = ReadOptions<PropertyKeyId, LabelId>;

const HYBRID_GQL: &str = "CALL hybrid.search(text => 'graph quantum', text_property => 'body', \
    vector => [2.0], vector_properties => ['x'], label => 'Doc', \
    candidates => 12, k => 6) \
    YIELD node, score, vector_rank, text_rank, vector_distance, text_score \
    RETURN node, score, vector_rank, text_rank, vector_distance, text_score";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn beacon_options() -> BeaconOptions {
    let mut options = BeaconOptions::text(BODY);
    options.vertex_label = Some(DOC);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(DOC)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Relation, "LINKS") => Some(GraphSymbol::Relation(LINKS)),
        _ => None,
    }
}

fn prepare(statement: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn rebuild_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(50_000, 10_000, 100_000_000, 200_000)
}

fn zero_scan_policy() -> GqlQueryPolicy {
    // Zero snapshot records allowed: any full-graph vertex or Strata block scan
    // fails closed with SnapshotBudget!
    GqlQueryPolicy::new(0, 10_000, 100_000_000, 200_000)
}

async fn seed_base_graph(cx: &CommitCx, db: &mut Database<MemVfs>) -> CommitSeq {
    let mut batch = WriteBatch::new(LINKS);
    for id in 1..=64_u128 {
        let body = if id % 5 == 0 {
            "graph quantum topology"
        } else {
            "storage retrieval database"
        };
        batch.create_vertex(
            VId(id),
            vec![DOC],
            vec![(BODY, text(body)), (X, CanonicalScalar::Int(id as i64))],
        );
    }
    // Chain 1 -> 2 -> 3 -> 4 -> 5 plus parallel edge on 2 -> 3.
    batch.add_edge(EId(101), VId(1), VId(2), vec![]);
    batch.add_edge(EId(102), VId(2), VId(3), vec![]);
    batch.add_edge(EId(103), VId(2), VId(3), vec![]);
    batch.add_edge(EId(104), VId(3), VId(4), vec![]);
    batch.add_edge(EId(105), VId(4), VId(5), vec![]);
    db.write(cx, batch).await.unwrap()
}

#[test]
fn durable_watermarks_catch_up_in_o_delta_with_zero_full_scans_and_heal_raptorq_bit_rot() {
    let ((), report) = run_async_under_lab(0xc198_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();

        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let s1 = seed_base_graph(&commit, &mut db).await;
        assert_eq!(s1, CommitSeq(1));

        // Bare Database::create + write must NOT create <db_dir>/derived/ lazily.
        assert!(
            vfs.metadata(&vfs.database_dir().join("derived"))
                .await
                .is_err()
        );

        let beacon_h = db
            .register_beacon_index(&query, &beacon_options(), ReadPolicy::default())
            .unwrap();
        let reach_h = db
            .register_standing_reachability(&query, LINKS, rebuild_policy())
            .unwrap();

        assert_eq!(
            db.save_beacon_checkpoint(&commit, &query, &beacon_h, "doc_hybrid")
                .await
                .unwrap(),
            s1,
        );
        assert_eq!(
            db.save_standing_reachability_checkpoint(&commit, &query, &reach_h, "links_reach")
                .await
                .unwrap(),
            s1,
        );

        // Append two delta commits S1+1..=S2 touching only 3 vertices and mutating topology.
        let mut delta1 = WriteBatch::new(LINKS);
        delta1.create_vertex(
            VId(65),
            vec![DOC],
            vec![
                (BODY, text("graph quantum needle")),
                (X, CanonicalScalar::Int(2)),
            ],
        );
        delta1.set_vertex_property(VId(2), BODY, Some(text("graph quantum promoted")));
        delta1.set_vertex_property(VId(2), X, Some(CanonicalScalar::Int(2)));
        delta1.delete_edge(EId(104)); // breaks 3 -> 4
        assert_eq!(db.write(&commit, delta1).await.unwrap(), CommitSeq(2));

        let mut delta2 = WriteBatch::new(LINKS);
        delta2.delete_vertex(VId(10));
        delta2.add_edge(EId(106), VId(2), VId(65), vec![]);
        delta2.add_edge(EId(107), VId(65), VId(5), vec![]); // restores 1 -> 2 -> 65 -> 5
        let s2 = db.write(&commit, delta2).await.unwrap();
        assert_eq!(s2, CommitSeq(3));

        // Corrupt 16 bytes inside each FGEC extent payload (within RaptorQ R=4 repair budget).
        let beacon_rel = db.beacon_checkpoint_relative_path("doc_hybrid");
        let reach_rel = db.standing_reachability_checkpoint_relative_path("links_reach");
        drop(db);

        // Build fresh reference expectations from a clean reopen at S2 BEFORE injecting bit-rot!
        let hybrid_plan = prepare(HYBRID_GQL);
        let text_req = Search::Text {
            query: "graph quantum",
            k: 6,
            mode: TextMatch::Any,
        };
        let vec_req = Search::Vector {
            query: &[2.0],
            k: 6,
            mode: VectorSearch::Exact,
        };
        let (expected_gql, expected_text_rows, expected_vec_rows, expected_pairs) = {
            let mut fresh_ref =
                Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                    .await
                    .unwrap();
            let fresh_reach = fresh_ref
                .register_standing_reachability(&query, LINKS, rebuild_policy())
                .unwrap();
            let expected_gql = fresh_ref
                .execute_graph_set_governed(&query, &hybrid_plan, rebuild_policy())
                .unwrap();
            let expected_text_rows = fresh_ref
                .beacon_search(&query, &beacon_options(), text_req)
                .unwrap();
            let expected_vec_rows = fresh_ref
                .beacon_search(&query, &beacon_options(), vec_req)
                .unwrap();
            let expected_pairs: Vec<_> = fresh_ref
                .standing_reachability(&query, &fresh_reach)
                .unwrap()
                .rows()
                .iter()
                .map(|(&pair, w)| (pair, w.to_i128().unwrap()))
                .collect();
            (
                expected_gql,
                expected_text_rows,
                expected_vec_rows,
                expected_pairs,
            )
        };

        for rel in [&beacon_rel, &reach_rel] {
            let full_path = vfs.database_dir().join(rel);
            let mut raw = vfs.read(&full_path).await.unwrap();
            assert!(raw.len() > 300);
            let flip_offset = raw.len() - 180;
            for byte in &mut raw[flip_offset..flip_offset + 16] {
                *byte ^= 0xA5;
            }
            vfs.write(&full_path, &raw).await.unwrap();
        }

        // Reopen cold from disk!
        let mut reopened =
            Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        assert_eq!(reopened.frontier().unwrap(), s2);

        // Catch up Beacon with max_staging_rows = 4 (vs 64-vertex corpus -> proves 0 full-graph scan!).
        let delta_only_maintenance = ReadPolicy {
            max_work_units: 10_000_000,
            max_source_scratch: 32,
            max_staging_rows: 4,
            max_result_rows: 64,
        };
        let (restored_beacon, beacon_outcome) = reopened
            .load_or_rebuild_beacon_checkpoint(
                &commit,
                &query,
                "doc_hybrid",
                &beacon_options(),
                delta_only_maintenance,
            )
            .await
            .unwrap();
        assert_eq!(
            beacon_outcome,
            DerivedCheckpointOutcome::CatchUp {
                watermark_seq: s1,
                head_seq: s2,
                commits_replayed: 2,
                touched_vertices: 3,
                healed_by_raptorq: true,
            },
        );

        // Catch up Ripple reachability with max_snapshot_records = 0 (proves 0 Strata block scans!).
        let (restored_reach, reach_outcome) = reopened
            .load_or_rebuild_standing_reachability_checkpoint(
                &commit,
                &query,
                "links_reach",
                LINKS,
                zero_scan_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            reach_outcome,
            DerivedCheckpointOutcome::CatchUp {
                watermark_seq: s1,
                head_seq: s2,
                commits_replayed: 2,
                touched_vertices: 0,
                healed_by_raptorq: true,
            },
        );

        // Execute CALL hybrid.search on `reopened` with max_snapshot_records = 0:
        // proves CALL hybrid.search reuses the restored MaintainedBeaconIndex with 0 vertex scans!
        let actual_gql = reopened
            .execute_graph_set_governed(&query, &hybrid_plan, zero_scan_policy())
            .unwrap();
        assert_eq!(actual_gql.value, expected_gql.value);
        assert_eq!(actual_gql.rows.snapshot_records, 0);
        assert_eq!(expected_gql.rows.snapshot_records, 64);

        for (req, expected_rows) in [
            (text_req, &expected_text_rows),
            (vec_req, &expected_vec_rows),
        ] {
            let mut zero_stage_opts = beacon_options();
            zero_stage_opts.policy.max_staging_rows = 0;
            zero_stage_opts.policy.max_source_scratch = 0;
            assert_eq!(
                &reopened
                    .beacon_search(&query, &zero_stage_opts, req)
                    .unwrap(),
                expected_rows,
            );
            assert_eq!(
                &reopened
                    .search_beacon_index(&query, &restored_beacon, req, ReadPolicy::default())
                    .unwrap(),
                expected_rows,
            );
        }

        let actual_pairs: Vec<_> = reopened
            .standing_reachability(&query, &restored_reach)
            .unwrap()
            .rows()
            .iter()
            .map(|(&pair, w)| (pair, w.to_i128().unwrap()))
            .collect();
        assert_eq!(actual_pairs, expected_pairs);
        assert!(actual_pairs.contains(&((VId(1), VId(5)), 1)));
        assert!(!actual_pairs.iter().any(|&((_, dst), _)| dst == VId(4)));

        // Verify live commits after reopen continue maintaining both restored views seamlessly.
        let mut delta3 = WriteBatch::new(LINKS);
        delta3.add_edge(EId(108), VId(5), VId(4), vec![]);
        reopened.write(&commit, delta3).await.unwrap();
        let post_write_pairs: Vec<_> = reopened
            .standing_reachability(&query, &restored_reach)
            .unwrap()
            .rows()
            .iter()
            .map(|(&pair, _)| pair)
            .collect();
        assert!(post_write_pairs.contains(&(VId(1), VId(4))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fg_inv_18_discards_corrupt_rolled_back_and_forked_checkpoints_and_rebuilds_cleanly() {
    let ((), report) = run_async_under_lab(0xc198_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();

        // Create a clean S1 database directory in `s1_vfs` (Chronicle head = S1).
        let s1_vfs = MemVfs::new().unwrap();
        let mut s1_db =
            Database::create_with_vfs(&commit, s1_vfs.clone(), s1_vfs.database_dir(), keys())
                .await
                .unwrap();
        let s1_only = seed_base_graph(&commit, &mut s1_db).await;
        assert_eq!(s1_only, CommitSeq(1));
        drop(s1_db);

        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let s1 = seed_base_graph(&commit, &mut db).await;
        assert_eq!(s1, CommitSeq(1));

        let beacon_h = db
            .register_beacon_index(&query, &beacon_options(), ReadPolicy::default())
            .unwrap();
        let reach_h = db
            .register_standing_reachability(&query, LINKS, rebuild_policy())
            .unwrap();

        // Advance to S2 and persist derived checkpoints at watermark_seq = S2.
        let mut s2_batch = WriteBatch::new(LINKS);
        s2_batch.create_vertex(
            VId(999),
            vec![DOC],
            vec![
                (BODY, text("graph quantum rolled_back_ghost")),
                (X, CanonicalScalar::Int(2)),
            ],
        );
        s2_batch.add_edge(EId(999), VId(5), VId(999), vec![]);
        let s2 = db.write(&commit, s2_batch).await.unwrap();
        assert_eq!(s2, CommitSeq(2));

        db.save_beacon_checkpoint(&commit, &query, &beacon_h, "doc_hybrid")
            .await
            .unwrap();
        db.save_standing_reachability_checkpoint(&commit, &query, &reach_h, "links_reach")
            .await
            .unwrap();
        let beacon_rel = db.beacon_checkpoint_relative_path("doc_hybrid");
        let reach_rel = db.standing_reachability_checkpoint_relative_path("links_reach");
        let beacon_s2_bytes = vfs
            .read(&vfs.database_dir().join(&beacon_rel))
            .await
            .unwrap();
        let reach_s2_bytes = vfs
            .read(&vfs.database_dir().join(&reach_rel))
            .await
            .unwrap();
        drop(db);

        // Negative Control 1: Chronicle at S1 (`s1_vfs`) while derived/ holds authentic S2 checkpoints!
        let s1_beacon_path = s1_vfs.database_dir().join(&beacon_rel);
        let s1_reach_path = s1_vfs.database_dir().join(&reach_rel);
        s1_vfs
            .create_dir_all(s1_beacon_path.parent().unwrap())
            .await
            .unwrap();
        s1_vfs
            .create_dir_all(s1_reach_path.parent().unwrap())
            .await
            .unwrap();
        s1_vfs
            .write(&s1_beacon_path, &beacon_s2_bytes)
            .await
            .unwrap();
        s1_vfs.write(&s1_reach_path, &reach_s2_bytes).await.unwrap();

        let mut rolled_back =
            Database::open_with_vfs(&commit, s1_vfs.clone(), s1_vfs.database_dir(), keys())
                .await
                .unwrap();
        assert_eq!(rolled_back.frontier().unwrap(), s1);

        let (rb_beacon, rb_beacon_outcome) = rolled_back
            .load_or_rebuild_beacon_checkpoint(
                &commit,
                &query,
                "doc_hybrid",
                &beacon_options(),
                ReadPolicy::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            rb_beacon_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::AheadOfChronicleHead {
                    watermark_seq: s2,
                    head_seq: s1,
                },
            },
        );
        let ghost_hits = rolled_back
            .search_beacon_index(
                &query,
                &rb_beacon,
                Search::Text {
                    query: "rolled_back_ghost",
                    k: 10,
                    mode: TextMatch::Any,
                },
                ReadPolicy::default(),
            )
            .unwrap();
        assert_eq!(ghost_hits.len(), 0, "rolled-back vertex must never leak!");

        let (rb_reach, rb_reach_outcome) = rolled_back
            .load_or_rebuild_standing_reachability_checkpoint(
                &commit,
                &query,
                "links_reach",
                LINKS,
                rebuild_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            rb_reach_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::AheadOfChronicleHead {
                    watermark_seq: s2,
                    head_seq: s1,
                },
            },
        );
        assert!(
            rolled_back
                .standing_reachability(&query, &rb_reach)
                .unwrap()
                .rows()
                .weight(&(VId(1), VId(999)))
                .is_none(),
            "rolled-back edge must never leak!",
        );
        drop(rolled_back);

        // Negative Control 2: Truncate/corrupt derived checkpoint files completely -> clean discard + rebuild!
        vfs.write(
            &vfs.database_dir().join(&beacon_rel),
            b"FGDCtruncated-corrupt",
        )
        .await
        .unwrap();
        vfs.write(&vfs.database_dir().join(&reach_rel), &[0xFF; 200])
            .await
            .unwrap();

        let mut corrupt_open =
            Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let (_, corrupt_beacon_outcome) = corrupt_open
            .load_or_rebuild_beacon_checkpoint(
                &commit,
                &query,
                "doc_hybrid",
                &beacon_options(),
                ReadPolicy::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            corrupt_beacon_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::CorruptOrIncompatible,
            },
        );
        let (_, corrupt_reach_outcome) = corrupt_open
            .load_or_rebuild_standing_reachability_checkpoint(
                &commit,
                &query,
                "links_reach",
                LINKS,
                rebuild_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            corrupt_reach_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::CorruptOrIncompatible,
            },
        );
        assert!(
            corrupt_open
                .standing_reachability(&query, &rb_reach)
                .is_err(),
            "foreign handle from previous open remains rejected",
        );
        drop(corrupt_open);

        // Negative Control 3: Transplant valid S2 checkpoints into a forked history at CommitSeq(2) -> ChainMismatch!
        let forked_vfs = MemVfs::new().unwrap();
        let mut forked = Database::create_with_vfs(
            &commit,
            forked_vfs.clone(),
            forked_vfs.database_dir(),
            keys(),
        )
        .await
        .unwrap();
        seed_base_graph(&commit, &mut forked).await;
        let mut alt_s2 = WriteBatch::new(LINKS);
        alt_s2.create_vertex(
            VId(777),
            vec![DOC],
            vec![
                (BODY, text("alternate branch commit")),
                (X, CanonicalScalar::Int(7)),
            ],
        );
        assert_eq!(forked.write(&commit, alt_s2).await.unwrap(), CommitSeq(2));
        drop(forked);

        let forked_beacon_path = forked_vfs.database_dir().join(&beacon_rel);
        let forked_reach_path = forked_vfs.database_dir().join(&reach_rel);
        forked_vfs
            .create_dir_all(forked_beacon_path.parent().unwrap())
            .await
            .unwrap();
        forked_vfs
            .create_dir_all(forked_reach_path.parent().unwrap())
            .await
            .unwrap();
        forked_vfs
            .write(&forked_beacon_path, &beacon_s2_bytes)
            .await
            .unwrap();
        forked_vfs
            .write(&forked_reach_path, &reach_s2_bytes)
            .await
            .unwrap();

        let mut forked_reopen = Database::open_with_vfs(
            &commit,
            forked_vfs.clone(),
            forked_vfs.database_dir(),
            keys(),
        )
        .await
        .unwrap();
        let (_, forked_beacon_outcome) = forked_reopen
            .load_or_rebuild_beacon_checkpoint(
                &commit,
                &query,
                "doc_hybrid",
                &beacon_options(),
                ReadPolicy::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            forked_beacon_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::ChainMismatch { watermark_seq: s2 },
            },
        );
        let (_, forked_reach_outcome) = forked_reopen
            .load_or_rebuild_standing_reachability_checkpoint(
                &commit,
                &query,
                "links_reach",
                LINKS,
                rebuild_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            forked_reach_outcome,
            DerivedCheckpointOutcome::RebuiltFromChronicle {
                reason: DerivedRebuildReason::ChainMismatch { watermark_seq: s2 },
            },
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
