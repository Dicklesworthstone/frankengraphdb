//! Laws of the partition manifest (`fgdb-63w2`, the mechanism half of
//! `fgdb-ge6a`): the durable object that makes a database directory reach its
//! partitions.
//!
//! Same disciplines as the sibling format suites — DISTINCT values per field
//! (two decoder offset defects shipped in this subsystem precisely because
//! equal values made a wrong-offset read look right), an every-strict-prefix
//! truncation sweep with an append control, and refusals proven in BOTH
//! directions. The store half proves the two ge6a laws with teeth: a
//! closed-and-reopened store resolves every named partition with nothing
//! carried in memory, and an absent root is a typed refusal NAMING the
//! missing identity — never a silently smaller graph.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::RelationId;
use fgdb_strata::manifest::{
    MANIFEST_FORMAT_V2, ManifestError, ManifestRecord, ManifestVersion, decode_manifest,
    encode_manifest, manifest_id, read_manifest,
};
use fgdb_strata::root::{BlockRef, PartitionRoot};
use fgdb_strata::store::{BlockStore, StoreError};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion, encode_block};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::future::Future;
use std::path::PathBuf;

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-manifest-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

/// Records whose every field is distinct, so a wrong-offset decode cannot
/// reproduce the input.
fn distinct_records() -> Vec<ManifestRecord> {
    vec![
        ManifestRecord {
            graph: GraphId(3),
            branch: BranchId(5),
            partition: 7,
            root: PartitionRootVersion(ObjectId([0x11; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa1; 32]),
        },
        ManifestRecord {
            graph: GraphId(3),
            branch: BranchId(5),
            partition: 9,
            root: PartitionRootVersion(ObjectId([0x22; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa2; 32]),
        },
        ManifestRecord {
            graph: GraphId(4),
            branch: BranchId(2),
            partition: 1,
            root: PartitionRootVersion(ObjectId([0x33; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa3; 32]),
        },
    ]
}

#[test]
fn a_manifest_round_trips_with_distinct_values() {
    let records = distinct_records();
    let bytes = encode_manifest(&records).expect("canonical records encode");
    assert_eq!(decode_manifest(&bytes).expect("decodes"), records);
    let empty = encode_manifest(&[]).expect("an empty database has an empty manifest");
    assert_eq!(decode_manifest(&empty).expect("decodes"), vec![]);
}

#[test]
fn every_strict_prefix_is_a_typed_refusal_with_an_append_control() {
    let bytes = encode_manifest(&distinct_records()).expect("encodes");
    for cut in 0..bytes.len() {
        let result = decode_manifest(&bytes[..cut]);
        assert!(
            matches!(
                result,
                Err(ManifestError::Truncated { .. }) | Err(ManifestError::NotAManifest)
            ),
            "prefix of {cut} bytes must refuse, got {result:?}"
        );
    }
    let mut appended = bytes;
    appended.push(0);
    assert_eq!(
        decode_manifest(&appended),
        Err(ManifestError::TrailingBytes { extra: 1 })
    );
}

#[test]
fn foreign_bytes_future_formats_and_identity_are_distinct_refusals() {
    let records = distinct_records();
    let bytes = encode_manifest(&records).expect("encodes");

    let mut wrong_magic = bytes.clone();
    wrong_magic[..4].copy_from_slice(b"FGSB");
    assert_eq!(
        decode_manifest(&wrong_magic),
        Err(ManifestError::NotAManifest)
    );

    let mut future = bytes.clone();
    future[4..6].copy_from_slice(&(MANIFEST_FORMAT_V2 + 1).to_be_bytes());
    assert_eq!(
        decode_manifest(&future),
        Err(ManifestError::UnsupportedFormat {
            format: MANIFEST_FORMAT_V2 + 1
        })
    );

    let id = ManifestVersion(manifest_id(&K_OID, NAMESPACE, &bytes));
    assert_eq!(
        read_manifest(&K_OID, NAMESPACE, &bytes, id).expect("identity matches"),
        records
    );
    let mut flipped = bytes;
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;
    assert!(matches!(
        read_manifest(&K_OID, NAMESPACE, &flipped, id),
        Err(ManifestError::IdentityMismatch { .. })
    ));
}

#[test]
fn order_duplicates_and_the_zero_root_are_refused_in_both_directions() {
    let records = distinct_records();

    let mut unsorted = records.clone();
    unsorted.swap(0, 2);
    assert_eq!(
        encode_manifest(&unsorted),
        Err(ManifestError::NonCanonicalOrder { at: 1 })
    );
    let duplicate = vec![records[0], records[0]];
    assert_eq!(
        encode_manifest(&duplicate),
        Err(ManifestError::NonCanonicalOrder { at: 1 }),
        "one coordinate, one root — a duplicate makes the answer ambiguous"
    );
    let zero = vec![ManifestRecord {
        root: PartitionRootVersion(ObjectId([0u8; 32])),
        ..records[0]
    }];
    assert_eq!(
        encode_manifest(&zero),
        Err(ManifestError::ZeroRoot { at: 0 })
    );

    // Decoder independence: splice the second record's coordinate into a copy
    // of the first — bytes the encoder cannot emit.
    let bytes = encode_manifest(&records).expect("encodes");
    let mut spliced = bytes.clone();
    let (first, second) = (10, 10 + 104);
    let head: Vec<u8> = spliced[first..first + 40].to_vec();
    spliced[second..second + 40].copy_from_slice(&head);
    assert_eq!(
        decode_manifest(&spliced),
        Err(ManifestError::NonCanonicalOrder { at: 1 })
    );
    let mut zeroed = bytes;
    for byte in &mut zeroed[first + 40..first + 72] {
        *byte = 0;
    }
    assert_eq!(
        decode_manifest(&zeroed),
        Err(ManifestError::ZeroRoot { at: 0 })
    );
}

// ---------------------------------------------------------------------------
// The store half: the two ge6a laws with teeth
// ---------------------------------------------------------------------------

const REL: RelationId = RelationId(1);

fn entry(src: u128, dst: u128, eid: u128, created: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: None,
    }
}

/// Publish one single-block partition and return its root identity.
async fn publish_partition(
    cx: &CommitCx,
    store: &BlockStore,
    partition: u64,
    src: u128,
) -> PartitionRootVersion {
    let bytes =
        encode_block(partition, None, &[entry(src, src + 1, src * 10, 1)]).expect("encodes");
    let block_id = store.put(cx, &bytes).await.expect("stores block");
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(1),
        partition,
        published_at: CommitSeq(2),
        blocks: vec![BlockRef {
            block_id: block_id.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        }],
        vertex_patches: vec![],
    };
    store.put_root(cx, &root).await.expect("root admits")
}

/// **THE RESOLUTION LAW: a manifest written, closed, and re-read resolves
/// every partition it names, with the store holding nothing in memory across
/// the close.** Until this object existed, every apparent reopen held a root
/// identity in a variable — which is not a reopen.
#[test]
fn a_reopened_store_resolves_every_partition_the_manifest_names() {
    let dir = scratch_dir("resolution");
    let manifest_id = under_lab(61, {
        let dir = dir.clone();
        move |cx| async move {
            let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
                .await
                .expect("opens");
            let first = publish_partition(&cx, &store, 0, 1).await;
            let second = publish_partition(&cx, &store, 1, 5).await;
            let records = vec![
                ManifestRecord {
                    graph: GraphId(1),
                    branch: BranchId(1),
                    partition: 0,
                    root: first,
                    published_chain_hash: fgdb_crypto::Digest([0xb1; 32]),
                },
                ManifestRecord {
                    graph: GraphId(1),
                    branch: BranchId(1),
                    partition: 1,
                    root: second,
                    published_chain_hash: fgdb_crypto::Digest([0xb2; 32]),
                },
            ];
            store
                .put_manifest(&cx, &records)
                .await
                .expect("manifest stores")
        }
    });

    // NOTHING crosses this line except the directory, the keys, and the
    // manifest identity a root slot would carry.
    under_lab(62, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("reopens");
        let resolved = store
            .resolve_manifest(&cx, manifest_id)
            .await
            .expect("every named partition resolves");
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved
                .iter()
                .map(|(record, root)| (record.partition, root.partition))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1)],
            "each record resolves to the root of ITS partition"
        );
        for (record, root) in &resolved {
            assert!(
                !root.blocks.is_empty(),
                "partition {} resolved to an empty root — the fixture published one block",
                record.partition
            );
        }
    });
}

/// **THE FAIL-CLOSED LAW: a manifest naming an absent root refuses with the
/// missing identity in the error.** A silently smaller graph is the failure
/// mode that looks exactly like correct operation, so the refusal must be
/// loud and must name what is missing.
#[test]
fn a_manifest_naming_an_absent_root_fails_closed_with_the_identity() {
    let dir = scratch_dir("absent-root");
    under_lab(63, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let real = publish_partition(&cx, &store, 0, 1).await;
        let ghost = PartitionRootVersion(ObjectId([0xee; 32]));
        let records = vec![
            ManifestRecord {
                graph: GraphId(1),
                branch: BranchId(1),
                partition: 0,
                root: real,
                published_chain_hash: fgdb_crypto::Digest([0xc1; 32]),
            },
            ManifestRecord {
                graph: GraphId(1),
                branch: BranchId(1),
                partition: 1,
                root: ghost,
                published_chain_hash: fgdb_crypto::Digest([0xc2; 32]),
            },
        ];
        let id = store
            .put_manifest(&cx, &records)
            .await
            .expect("manifest stores");
        let result = store.resolve_manifest(&cx, id).await;
        let refusal = result.err().and_then(|error| match error {
            StoreError::ManifestRootLoad { at, root, .. } => Some((at, root)),
            _ => None,
        });
        assert_eq!(
            refusal,
            Some((1, ghost.0)),
            "an absent root must refuse resolution, naming the gap"
        );
    });
}
