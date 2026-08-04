//! Real-consumer proof for task-local collection storage.
//!
//! These are deliberately narrower than the five-oracle `collections_e2e`
//! matrix owned by `fgdb-5mqr`: each test drives one optimized collection
//! through actual growth, verifies its answers, drops every typed owner, and
//! then requires a balanced cancellation audit from the underlying region.

use asupersync::Cx;
use asupersync::cx::cap;
#[cfg(not(miri))]
use asupersync::lab::run_async_under_lab;
use fgdb_collections::art::{AdaptiveRadixTree, ArtError};
use fgdb_collections::hash_table::{DeterministicHashTable, HashTableError};
use fgdb_collections::succinct::{AllocationTarget, BitVectorError, SuccinctBitVectorBuilder};
use fgdb_types::{PurposeContexts, QueryCx};
use fgdb_unsafe_arena::{RegionOutcome, RegionScope, RegionVecError};

#[cfg(miri)]
const ART_EDGE_COUNT: usize = 17;
#[cfg(not(miri))]
const ART_EDGE_COUNT: usize = 256;
#[cfg(miri)]
const HASH_ENTRY_COUNT: u64 = 128;
#[cfg(not(miri))]
const HASH_ENTRY_COUNT: u64 = 2_048;
#[cfg(miri)]
const SUCCINCT_BIT_COUNT: usize = 513;
#[cfg(not(miri))]
const SUCCINCT_BIT_COUNT: usize = 4_097;

#[cfg(not(miri))]
fn query_root() -> (Cx<cap::All>, QueryCx) {
    let (pair, report) = run_async_under_lab(0xc011_ec71, |root| async move {
        let query = PurposeContexts::narrow_runtime_root(&root).query();
        (root, query)
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    pair
}

#[cfg(miri)]
fn query_root() -> (Cx<cap::All>, QueryCx) {
    let root = Cx::<cap::All>::for_testing();
    let query = PurposeContexts::narrow_runtime_root(&root).query();
    (root, query)
}

fn query_cx() -> QueryCx {
    query_root().1
}

fn scope() -> RegionScope {
    RegionScope::with_capacity(1 << 20, 1 << 29, 1 << 30)
}

fn assert_cancelled_and_balanced(scope: RegionScope) {
    assert_eq!(scope.owners(), 0, "all real consumers must be dropped");
    assert!(
        scope.bytes_allocated() > 0,
        "the consumer must exercise an actual regional allocation"
    );
    let audit = scope.cancel().expect("owner-free cancellation audits");
    assert_eq!(audit.outcome, RegionOutcome::Cancelled);
    assert!(
        audit.balanced(),
        "cancellation leaked region bytes: {audit:?}"
    );
    assert_eq!(audit.bytes_reclaimed, audit.bytes_allocated);
}

#[test]
fn art_growth_is_region_backed_and_cancel_reclaimed() {
    let scope = scope();
    let cx = query_cx();
    {
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        for ordinal in 0..ART_EDGE_COUNT {
            let edge = ordinal as u8;
            tree.insert(&cx, [edge], u64::from(edge))
                .expect("one-byte key inserts");
        }
        #[cfg(not(miri))]
        assert_eq!(
            tree.node_kind_histogram().node256,
            1,
            "real growth must reach the widest ART representation"
        );
        #[cfg(miri)]
        assert_eq!(tree.len(), ART_EDGE_COUNT);
        for ordinal in 0..ART_EDGE_COUNT {
            let edge = ordinal as u8;
            assert_eq!(tree.get([edge]), Some(&u64::from(edge)));
        }
        assert!(scope.owners() > 0);
    }
    assert_cancelled_and_balanced(scope);
}

#[test]
fn hash_growth_is_region_backed_and_cancel_reclaimed() {
    let scope = scope();
    let cx = query_cx();
    {
        let mut table = DeterministicHashTable::new_in(&scope, 0xdec1_5105).expect("table opens");
        for key in 0_u64..HASH_ENTRY_COUNT {
            table
                .insert(&cx, key, key.rotate_left(17))
                .expect("hash entry inserts");
        }
        assert!(table.bucket_count() > HASH_ENTRY_COUNT as usize);
        for key in 0_u64..HASH_ENTRY_COUNT {
            assert_eq!(table.get(&key), Some(&key.rotate_left(17)));
        }
        assert!(scope.owners() > 0);
    }
    assert_cancelled_and_balanced(scope);
}

#[test]
fn succinct_growth_is_region_backed_and_cancel_reclaimed() {
    let scope = scope();
    let cx = query_cx();
    {
        let bit_len = SUCCINCT_BIT_COUNT;
        let mut builder = SuccinctBitVectorBuilder::new_in(&scope, bit_len).expect("builder opens");
        for index in 0..bit_len {
            builder
                .push(&cx, index.is_multiple_of(7) || index.is_multiple_of(127))
                .expect("bit appends");
        }
        let vector = builder.finish(&cx).expect("rank directories build");
        let expected_ones = (0..bit_len)
            .filter(|index| index.is_multiple_of(7) || index.is_multiple_of(127))
            .count();
        assert_eq!(vector.count_ones(), expected_ones);
        assert_eq!(vector.rank1(bit_len), Some(expected_ones));
        assert!(scope.owners() > 0);
    }
    assert_cancelled_and_balanced(scope);
}

#[test]
fn cancelled_query_refuses_consumer_mutation_before_state_changes() {
    let scope = scope();
    let (root, cx) = query_root();
    let mut tree = AdaptiveRadixTree::new_in(&scope);
    tree.insert(&cx, b"stable", 7).expect("ART seed inserts");
    let mut table = DeterministicHashTable::new_in(&scope, 17).expect("table opens");
    table.insert(&cx, 1_u64, 11_u64).expect("hash seed inserts");
    let mut builder = SuccinctBitVectorBuilder::new_in(&scope, 64).expect("builder opens");
    builder.push(&cx, true).expect("succinct seed appends");

    root.set_cancel_requested(true);

    assert!(matches!(
        tree.insert(&cx, b"stable", 9),
        Err(ArtError::Region {
            source: RegionVecError::CheckpointRefused,
            ..
        })
    ));
    assert!(matches!(
        tree.remove(&cx, b"stable"),
        Err(ArtError::Region {
            source: RegionVecError::CheckpointRefused,
            ..
        })
    ));
    assert_eq!(tree.get(b"stable"), Some(&7));

    assert_eq!(
        table.insert(&cx, 1_u64, 13_u64),
        Err(HashTableError::Region(RegionVecError::CheckpointRefused))
    );
    assert_eq!(table.get(&1), Some(&11));

    assert_eq!(
        builder.push(&cx, false),
        Err(BitVectorError::Region {
            target: AllocationTarget::Words,
            source: RegionVecError::CheckpointRefused,
        })
    );
    assert_eq!(builder.len(), 1);
    assert_eq!(builder.as_words(), &[1]);

    drop(builder);
    drop(table);
    drop(tree);
    assert_cancelled_and_balanced(scope);
}
