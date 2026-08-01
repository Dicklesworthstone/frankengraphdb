//! Durability laws for the published root — run under the lab runtime.
//!
//! This is the first Chronicle test that touches a real filesystem, so it is
//! the first place the durability claims can be *false* rather than merely
//! unproven. The laws worth having are the ones about crashes:
//!
//!   * a published root survives the process that wrote it;
//!   * publishing ALTERNATES slots, because rewriting one slot forever is the
//!     failure mode that silently removes all crash safety while every test
//!     that only checks "the newest generation is readable" still passes;
//!   * a crash mid-publish leaves the previous generation whole;
//!   * recovery never moves backwards, even when the newest slot is damaged.
//!
//! Writer paths take `&CommitCx`, while persisted reads take the sealed storage-
//! read capability implemented by `CommitCx`, so these run under asupersync's
//! lab runtime (plan §15: simulation-first, and the lab exists before the first
//! fsync).

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::root::{NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootSlot};
use fgdb_chronicle::store::{RootCreateCrashPoint, RootStore, StoreError};
use fgdb_types::context::{CommitCx, PurposeContexts};
use std::path::PathBuf;

/// A per-test directory that includes the process id, because a neighbouring
/// pane running the same suite must not be able to delete or observe ours.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fgdb-root-store-{}-{}-{name}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("t")
            .replace("::", "-")
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn bootstrap(seed: u8) -> RootBootstrap {
    RootBootstrap {
        root_encoding_id: [seed; 32],
        root_placement_id: [seed.wrapping_add(1); 32],
        root_placement_epoch: 1,
        failure_domain_policy_id: 2,
        root_failure_domain_id: 7,
        segment_id: 11,
        offset: 0,
        encoded_len: 4096,
        root_symbol_inventory_digest: [seed.wrapping_add(2); 32],
        object_kind: 0x0001,
        canonical_plaintext_len: 1024,
        codec_profile: 1,
        compressed_len: 1024,
        data_crypto_profile: 1,
        dek_id: [seed.wrapping_add(3); 16],
        nonce_len: NONCE_CAPACITY as u16,
        nonce_or_siv: [seed.wrapping_add(4); NONCE_CAPACITY],
        object_tag_len: 16,
        fec_profile: 1,
        transfer_length: 4096,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: 256,
        source_block_count: 1,
        symbol_auth_profile: 1,
        ciphertext_id: [seed.wrapping_add(5); 32],
        ciphertext_digest: [seed.wrapping_add(6); 32],
        opener_kind: 1,
        oid_key_id: [seed.wrapping_add(7); 16],
        opener_payload_len: 32,
        opener_payload: [seed.wrapping_add(8); OPENER_PAYLOAD_LEN],
        opener_digest: [seed.wrapping_add(9); 32],
    }
}

fn slot(generation: u64) -> RootSlot {
    RootSlot {
        format_major: 1,
        format_minor: 0,
        slot_generation: generation,
        local_writer_fence_epoch: 3,
        database_id: [0xaa; 16],
        database_security_namespace_id: [0x5a; 32],
        cluster_incarnation: 4,
        incarnation_continuity_profile_id: 1,
        cluster_incarnation_continuity_digest: [0xc3; 32],
        continuity_cas_version: 12,
        service_visibility_epoch: 5,
        root_manifest_oid: [0x77; 32],
        // A distinct bootstrap per generation, so a test that believes it read
        // generation N cannot be satisfied by generation M's bytes.
        bootstrap: bootstrap(generation as u8),
    }
}

/// Run a closure with a real `CommitCx` under the lab runtime.
fn under_lab<T: Send + 'static>(
    seed: u64,
    test: impl FnOnce(&CommitCx) -> T + Send + 'static,
) -> T {
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(&contexts.commit())
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

/// THE MILESTONE: a published root outlives the code that wrote it. The store
/// is dropped and rebuilt from the path alone, which is what a process restart
/// looks like from the file's point of view.
#[test]
fn a_published_root_survives_a_restart() {
    let dir = scratch_dir("restart");
    under_lab(1, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis publish");
        drop(store);

        // A completely fresh binding, as a restarted process would make.
        let reopened = RootStore::new(&dir);
        let recovered = reopened.current(cx).expect("the root must survive");
        assert_eq!(recovered.slot_generation, 1);
        assert_eq!(
            recovered,
            slot(1),
            "every field must survive the round trip"
        );
    });
}

/// Syncing the two root slots is not publication until the parent directory
/// also makes the new `manifest.root` name durable. The working view proves
/// the inode bytes were written; the empty crash image models the legal loss
/// of an unsynced creation dirent without deleting the working fixture.
#[test]
fn genesis_is_not_acknowledged_between_inode_and_directory_sync() {
    let working_dir = scratch_dir("genesis-dirent-working");
    let crash_image = scratch_dir("genesis-dirent-image");
    under_lab(11, move |cx| {
        let store = RootStore::new(&working_dir);
        let crashed = store.create_with_crash(
            cx,
            &slot(1),
            Some(RootCreateCrashPoint::AfterFileSyncBeforeDirectorySync),
        );
        assert!(
            crashed.is_err(),
            "the creation barrier must not return green"
        );
        assert_eq!(
            store
                .current(cx)
                .expect("inode survives in the working view"),
            slot(1)
        );

        let lost_dirent = RootStore::new(&crash_image);
        assert!(
            matches!(
                lost_dirent.current(cx),
                Err(StoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound
            ),
            "a crash image without the unsynced dirent has no published root"
        );
    });
}

/// PUBLISHING MUST ALTERNATE. Rewriting one slot forever passes every test
/// that only asks "is the newest generation readable" while silently removing
/// all crash safety — so the alternation is asserted directly.
#[test]
fn publishing_alternates_slots() {
    let dir = scratch_dir("alternate");
    under_lab(2, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");

        let mut seen = Vec::new();
        for generation in 2..=6u64 {
            store.publish(cx, &slot(generation)).expect("publish");
            let current = store.current(cx).expect("current");
            assert_eq!(current.slot_generation, generation);
            seen.push(store.selected_slot_index(cx).expect("index"));
        }
        // Consecutive publishes must land in different physical slots.
        for pair in seen.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "consecutive publishes must alternate slots, saw {seen:?}"
            );
        }
    });
}

/// A CRASH MID-PUBLISH LEAVES THE PREVIOUS GENERATION WHOLE. The slot a
/// publish targets is by construction not the one recovery would choose, so
/// damaging it models the crash exactly.
#[test]
fn a_crash_mid_publish_preserves_the_previous_generation() {
    let dir = scratch_dir("crash");
    under_lab(3, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");
        store.publish(cx, &slot(2)).expect("second generation");

        let live_index = store.selected_slot_index(cx).expect("index");
        let target = 1 - live_index; // the slot the NEXT publish would write

        // Crash partway through writing generation 3: the target slot is left
        // damaged, the live slot is untouched.
        store
            .damage_slot_for_test(target, 1234)
            .expect("simulate a torn write");

        let recovered = store.current(cx).expect("the previous generation survives");
        assert_eq!(
            recovered.slot_generation, 2,
            "a crash on the inactive slot must not disturb the live root"
        );
        assert_eq!(recovered, slot(2));

        // And the store is still writable: the next publish simply reuses the
        // damaged slot, which is exactly what it was for.
        store.publish(cx, &slot(3)).expect("publish after a crash");
        assert_eq!(store.current(cx).expect("current").slot_generation, 3);
    });
}

/// Damaging the LIVE slot must fall back to the older credible one — and that
/// is a rollback the rule permits ONLY because the newer slot is no longer
/// credible, not because it is older. Recovery still never prefers an older
/// slot while a newer one is intact.
#[test]
fn damaging_the_live_slot_falls_back_to_the_credible_one() {
    let dir = scratch_dir("fallback");
    under_lab(4, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");
        store.publish(cx, &slot(2)).expect("second");

        let live = store.selected_slot_index(cx).expect("index");
        store.damage_slot_for_test(live, 77).expect("damage live");

        let recovered = store.current(cx).expect("the older credible slot recovers");
        assert_eq!(recovered.slot_generation, 1);
    });
}

/// A publish that does not increase the generation is refused: recovery
/// selects by highest generation, so such a write could never be chosen while
/// leaving the writer believing it had published.
#[test]
fn a_non_monotonic_publish_is_refused() {
    let dir = scratch_dir("monotonic");
    under_lab(5, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(5)).expect("genesis at 5");

        for regressive in [1u64, 4, 5] {
            let outcome = store.publish(cx, &slot(regressive));
            assert!(
                matches!(
                    outcome,
                    Err(StoreError::NonMonotonicGeneration {
                        current: 5,
                        proposed
                    }) if proposed == regressive
                ),
                "generation {regressive} must be refused, got {outcome:?}"
            );
        }
        // The refusals changed nothing.
        assert_eq!(store.current(cx).expect("current").slot_generation, 5);
    });
}

/// Genesis writes both slots identically, so the very first recovery meets the
/// identical-pair rule rather than one credible slot beside 4096 zero bytes.
#[test]
fn genesis_publishes_an_identical_pair() {
    let dir = scratch_dir("genesis");
    under_lab(6, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");
        assert!(
            matches!(
                store.recover(cx).expect("recover"),
                fgdb_chronicle::root::RootSelection::IdenticalPair { .. }
            ),
            "genesis must leave an identical pair"
        );
    });
}

/// A truncated root file is not an empty one. Treating a short file as fresh
/// would discard a database, so it is refused by length before any slot is
/// parsed.
#[test]
fn a_truncated_root_file_is_refused_not_treated_as_fresh() {
    let dir = scratch_dir("truncated");
    under_lab(7, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(store.path())
            .expect("open");
        file.set_len(4096).expect("truncate to one slot");
        file.sync_all().expect("sync");

        assert!(
            matches!(
                store.current(cx),
                Err(StoreError::MalformedFile { len: 4096 })
            ),
            "a short root file must be refused, never read as fresh"
        );
    });
}

/// Many publishes in sequence keep converging: the store stays readable and
/// monotone across a long run, which is the property a long-lived database
/// actually depends on.
#[test]
fn repeated_publishes_stay_monotone_and_readable() {
    let dir = scratch_dir("repeat");
    under_lab(8, move |cx| {
        let store = RootStore::new(&dir);
        store.create(cx, &slot(1)).expect("genesis");
        for generation in 2..=40u64 {
            store.publish(cx, &slot(generation)).expect("publish");
            let current = store.current(cx).expect("current");
            assert_eq!(current.slot_generation, generation);
            assert_eq!(
                current,
                slot(generation),
                "the read root must be the written one"
            );
        }
    });
}
