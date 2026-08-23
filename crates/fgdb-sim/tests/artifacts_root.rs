//! Guard for the durable test-artifact root
//! (fgdb-crashpack-artifacts-durable-home-vd35).
//!
//! `.cargo/config.toml` pins `ASUPERSYNC_TEST_ARTIFACTS_DIR` to
//! `<workspace>/artifacts/test` so asupersync's lab runtime writes crashpacks
//! and replay bundles somewhere `cargo clean` cannot reach. The regression
//! class this bead closed: the runtime default resolves to a CWD-relative
//! `target/test-artifacts`, which cargo places inside each crate's cleanable
//! `target/` directory — every saved reproducer silently died on the next
//! clean. These tests are the promotion witnesses: one proves the pin is
//! present, durable, and writable; one fires on the regressed shape so the
//! predicate has a distinct failing witness in the house style.

use std::path::{Component, Path, PathBuf};

/// Walk up from this crate's manifest to the directory holding the workspace
/// marker. Mirrors how cargo resolves `[env] relative = true`, without taking
/// a dependency on cargo internals.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("rust-toolchain.toml").is_file() {
            return dir;
        }
        dir = dir
            .parent()
            .map(Path::to_path_buf)
            .expect("rust-toolchain.toml marker above crates/fgdb-sim");
    }
}

fn contains_target_component(path: &Path) -> bool {
    path.components()
        .any(|c| c == Component::Normal("target".as_ref()))
}

#[test]
fn artifact_root_pin_is_present_durable_and_writable() {
    let raw = std::env::var("ASUPERSYNC_TEST_ARTIFACTS_DIR")
        .unwrap_or_default();
    assert!(
        !raw.is_empty(),
        "ASUPERSYNC_TEST_ARTIFACTS_DIR is unset: the .cargo/config.toml [env] \
         pin is missing or was renamed; crashpacks would regress into a \
         cleanable target/ directory"
    );

    // Cargo may deliver the pinned value resolved (absolute) or raw; both must
    // land on the same durable location.
    let root = workspace_root();
    let resolved = if Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        root.join(&raw)
    };

    assert!(
        !contains_target_component(&resolved),
        "artifact root {resolved:?} regressed under a target/ component; \
         replays would not survive cargo clean"
    );
    assert_eq!(
        resolved,
        root.join("artifacts").join("test"),
        "artifact root drifted from the .cargo/config.toml pin"
    );

    // Writability is part of durability: prove the pinned location accepts a
    // deterministic probe file (gitignored; contents are not contractual).
    let probe_dir = resolved.join("artifacts_root_guard");
    std::fs::create_dir_all(&probe_dir).expect("create durable artifact probe dir");
    std::fs::write(probe_dir.join("probe.txt"), b"fgdb-crashpack-vd35\n")
        .expect("write durable artifact probe file");
}

#[test]
fn legacy_target_relative_default_fires_the_guard_predicate() {
    // Distinct negative witness for the predicate the positive test relies on:
    // the pre-bead default shape MUST be classified as regressed.
    let legacy = Path::new("target/test-artifacts");
    assert!(contains_target_component(legacy));
}
