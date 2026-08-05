//! The certificate and external-CAS boundaries of dual-root publication
//! (bead `fgdb-1dgm`), in their W1 subset.
//!
//! Two pieces of machinery, two law families:
//!
//!   * **Evidence is minted from the reread, not from intent.**
//!     `publish_evidenced` re-reads the file after the durability barrier and
//!     authenticates that recovery would select exactly the published slot;
//!     the returned `RootPublicationEvidence` is the certificate boundary's
//!     W1 form. A publication the reread cannot observe mints NOTHING and
//!     fails closed, leaving the previous generation whole.
//!
//!   * **The continuity head is validated before any byte moves.**
//!     `publish_with_continuity` consults a `ContinuityAuthority` and refuses
//!     on version skew, digest fork, or outage — and the refusal laws here
//!     pin that the root file is byte-identical afterwards, which is what
//!     "immediately before its irreversible step" means as a testable claim.
//!
//! The LDFI rows `dual-root-certificate-boundary` and
//! `dual-root-external-cas-boundary` may flip to Reachable only when the sim
//! can FAULT these paths (FaultVfs lying at the reread; a lab CAS register
//! serving stale/forked heads). That is deliberately not this file's claim:
//! these are the chronicle-side laws the sim witnesses will lean on.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::root::{NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootSlot};
use fgdb_chronicle::store::{ContinuityAuthority, ContinuityHead, RootStore, StoreError};
use fgdb_types::context::{CommitCx, PurposeContexts};
use std::path::PathBuf;

/// A per-test directory that includes the process id, because a neighbouring
/// pane running the same suite must not be able to delete or observe ours.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fgdb-root-evidence-{}-{}-{name}",
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

const CONTINUITY_DIGEST: [u8; 32] = [0xc3; 32];
const CONTINUITY_CAS_VERSION: u64 = 12;

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
        cluster_incarnation_continuity_digest: CONTINUITY_DIGEST,
        continuity_cas_version: CONTINUITY_CAS_VERSION,
        service_visibility_epoch: 5,
        root_manifest_oid: [generation as u8; 32],
        // A distinct bootstrap per generation, so a test that believes it read
        // generation N cannot be satisfied by generation M's bytes.
        bootstrap: bootstrap(generation as u8),
    }
}

/// Run an async closure with a real `CommitCx` under the lab runtime.
fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

/// An authority stub whose answer the test controls exactly. `Err` models an
/// outage; the store must treat every non-`Ok` as fail-closed, so a stub is
/// the complete behaviour space.
struct FixedAuthority(Option<ContinuityHead>);

impl ContinuityAuthority for FixedAuthority {
    async fn current_head(&self, _cx: &CommitCx) -> std::io::Result<ContinuityHead> {
        self.0
            .ok_or_else(|| std::io::Error::other("authority unreachable"))
    }
}

fn matching_head() -> ContinuityHead {
    ContinuityHead {
        cas_version: CONTINUITY_CAS_VERSION,
        cluster_incarnation_continuity_digest: CONTINUITY_DIGEST,
    }
}

// ---------------------------------------------------------------------------
// The certificate boundary: evidence comes from the reread.
// ---------------------------------------------------------------------------

/// Evidence names the slot recovery would select — and successive
/// publications alternate physical slots, cross-checked against the store's
/// own selection so the evidence cannot drift from what recovery sees.
#[test]
fn evidence_is_minted_from_the_selected_slot_and_alternates() {
    let dir = scratch_dir("evidence-alternates");
    under_lab(61, move |cx| async move {
        let store = RootStore::new(&dir);
        store.create(&cx, &slot(1)).await.expect("creates");

        let second = store
            .publish_evidenced(&cx, &slot(2))
            .await
            .expect("publishes generation 2");
        assert_eq!(second.slot_generation, 2);
        assert_eq!(second.root_manifest_oid, [2u8; 32]);
        assert_eq!(
            second.written_index,
            store.selected_slot_index(&cx).await.expect("selects"),
            "evidence must name the slot recovery selects, not the writer's intent"
        );

        let third = store
            .publish_evidenced(&cx, &slot(3))
            .await
            .expect("publishes generation 3");
        assert_ne!(
            second.written_index, third.written_index,
            "publication must alternate slots; rewriting one slot forever \
             silently removes all crash safety"
        );
        assert_eq!(
            third.written_index,
            store.selected_slot_index(&cx).await.expect("selects")
        );
    });
}

/// The fail-closed arm: a slot torn AFTER the barrier reported success mints
/// no evidence, and the previous generation remains the selected root. This
/// is the law the sim's FaultVfs witness will fire through a lying sync; here
/// the interference arrives through the deterministic observation point so
/// the refusal is provable without a faulting filesystem.
#[test]
fn an_unobservable_publication_mints_no_evidence() {
    let dir = scratch_dir("unobservable");
    under_lab(62, move |cx| async move {
        let store = RootStore::new(&dir);
        store.create(&cx, &slot(1)).await.expect("creates");
        let path = store.path().to_path_buf();

        // Creation leaves an identical pair, so the next publish targets
        // slot B — tear it at the observation point between the barrier and
        // the evidence reread. 4096 is SLOT_B's offset; one flipped byte in
        // the slot interior is enough to break its authentication.
        let outcome = store
            .publish_evidenced_with_steps(&cx, &slot(2), || {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
                file.seek(SeekFrom::Start(4096 + 512))?;
                file.write_all(&[0xff])?;
                file.sync_all()?;
                Ok(())
            })
            .await;

        assert!(
            matches!(
                outcome,
                Err(StoreError::PublicationNotObservable {
                    expected_generation: 2
                })
            ),
            "a publication the reread cannot authenticate must mint nothing: {outcome:?}"
        );
        let recovered = store.current(&cx).await.expect("previous root recovers");
        assert_eq!(
            recovered.slot_generation, 1,
            "the previous generation must remain whole and selected"
        );
    });
}

// ---------------------------------------------------------------------------
// The external-CAS boundary: the head is validated before any byte moves.
// ---------------------------------------------------------------------------

/// A matching head publishes and mints the same evidence the plain path
/// would — the continuity check adds refusals, never a different protocol.
#[test]
fn a_matching_continuity_head_admits_the_publication() {
    let dir = scratch_dir("continuity-match");
    under_lab(63, move |cx| async move {
        let store = RootStore::new(&dir);
        store.create(&cx, &slot(1)).await.expect("creates");

        let evidence = store
            .publish_with_continuity(&cx, &slot(2), &FixedAuthority(Some(matching_head())))
            .await
            .expect("a matching head admits");
        assert_eq!(evidence.slot_generation, 2);
        assert_eq!(
            evidence.written_index,
            store.selected_slot_index(&cx).await.expect("selects")
        );
    });
}

/// Version skew, digest fork, and outage each refuse — and each refusal
/// leaves the root file BYTE-IDENTICAL, which is the testable meaning of
/// "revalidates the head immediately before its irreversible step".
#[test]
fn continuity_refusals_precede_the_irreversible_write() {
    let dir = scratch_dir("continuity-refusals");
    under_lab(64, move |cx| async move {
        let store = RootStore::new(&dir);
        store.create(&cx, &slot(1)).await.expect("creates");
        let pristine = std::fs::read(store.path()).expect("baseline bytes");

        let behind = FixedAuthority(Some(ContinuityHead {
            cas_version: CONTINUITY_CAS_VERSION - 1,
            ..matching_head()
        }));
        let ahead = FixedAuthority(Some(ContinuityHead {
            cas_version: CONTINUITY_CAS_VERSION + 1,
            ..matching_head()
        }));
        let forked = FixedAuthority(Some(ContinuityHead {
            cluster_incarnation_continuity_digest: [0xee; 32],
            ..matching_head()
        }));
        let unreachable = FixedAuthority(None);

        for (name, authority, want_skew, want_fork, want_outage) in [
            ("behind", &behind, true, false, false),
            ("ahead", &ahead, true, false, false),
            ("forked", &forked, false, true, false),
            ("unreachable", &unreachable, false, false, true),
        ] {
            let outcome = store
                .publish_with_continuity(&cx, &slot(2), authority)
                .await;
            let refused = match outcome {
                Err(StoreError::ContinuityVersionSkew { .. }) => want_skew,
                Err(StoreError::ContinuityForked { .. }) => want_fork,
                Err(StoreError::ContinuityUnavailable(_)) => want_outage,
                ref other => {
                    panic!("{name}: expected a continuity refusal, got {other:?}")
                }
            };
            assert!(refused, "{name}: refused with the wrong continuity error");
            assert_eq!(
                std::fs::read(store.path()).expect("bytes after refusal"),
                pristine,
                "{name}: a continuity refusal must leave the root file untouched"
            );
        }

        // The same slot publishes once the head matches: the refusals above
        // were the authority's doing, not a poisoned store.
        store
            .publish_with_continuity(&cx, &slot(2), &FixedAuthority(Some(matching_head())))
            .await
            .expect("a matching head still admits after refusals");
    });
}
