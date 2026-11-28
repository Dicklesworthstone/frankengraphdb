#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn invoke(root: &Path, cmd: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fgdb"))
        .current_dir(root)
        .args(["--robot", cmd, "--db", "graph", "--key-file", "keys"])
        .args(args)
        .output()
        .unwrap()
}

fn success(root: &Path, cmd: &str, args: &[&str]) -> Vec<u8> {
    let result = invoke(root, cmd, args);
    assert!(
        result.status.success(),
        "{cmd} failed: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}

#[test]
fn scrub_imports_a_committed_backup_capsule_across_process_opens() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fgdb-capsule-restart-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    let key_path = root.join("keys");
    fs::write(
        &key_path,
        format!(
            "{}\n{}\n{}\n",
            "01".repeat(32),
            "02".repeat(32),
            "03".repeat(32)
        ),
    )
    .unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    success(
        &root,
        "create",
        &[
            "--persist-bindings",
            "--label",
            "Person=1",
            "--property",
            "id=1",
        ],
    );
    let capsules = root.join("graph/capsules");
    let all_capsules = || -> std::collections::BTreeSet<_> {
        fs::read_dir(&capsules)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "capsule"))
            .collect()
    };
    let write_capsule = |statement| {
        let before = all_capsules();
        success(&root, "write", &[statement]);
        let after = all_capsules();
        let added: Vec<_> = after.difference(&before).collect();
        assert_eq!(added.len(), 1);
        added[0].clone()
    };
    let live = write_capsule("CREATE (:Person {id:1})");
    let second = write_capsule("CREATE (:Person {id:2})");
    let query = ["MATCH (n:Person) RETURN n.id ORDER BY n.id"];
    let expected_rows = success(&root, "query", &query);
    assert_eq!(
        String::from_utf8_lossy(&expected_rows)
            .matches(r#""event":"row""#)
            .count(),
        2
    );
    let oid = live.file_stem().unwrap().to_str().unwrap();
    let oid2 = second.file_stem().unwrap().to_str().unwrap();
    let history_path = root.join("graph/commits.log");
    let history = fs::read(&history_path).unwrap();
    let verified =
        String::from_utf8(success(&root, "verify-history", &["--through", "3"])).unwrap();
    let anchor = verified
        .split(r#""chain_hash":""#)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    fs::rename(&live, root.join("backup.capsule")).unwrap();
    fs::rename(&second, root.join("second.capsule")).unwrap();
    fs::write(&second, b"damaged").unwrap();
    assert!(!invoke(&root, "scrub", &[]).status.success());

    // The keys and capsule data are unnecessary for a marker inventory. An
    // independently retained commitment anchors the list before repair.
    let inventory = |cut, hash| {
        Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .current_dir(&root)
            .args([
                "--robot",
                "list-capsules",
                "--db",
                "graph",
                "--through",
                cut,
                "--expect-chain",
                hash,
            ])
            .output()
            .unwrap()
    };
    let listed = inventory("3", anchor);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let output = String::from_utf8(listed.stdout).unwrap();
    assert_eq!(output.matches(r#""event":"capsule_ref""#).count(), 3);
    for (seq, id) in [(2, oid), (3, oid2)] {
        assert!(output.contains(&format!(
            r#""event":"capsule_ref","seq":{seq},"object":"{id}""#
        )));
    }
    assert!(output.contains(r#""kind":"capsule_inventory""#));
    let invalid_anchor = "00".repeat(32);
    for refused in [inventory("3", &invalid_anchor), inventory("4", anchor)] {
        assert!(!refused.status.success());
        assert!(
            !String::from_utf8_lossy(&refused.stdout).contains(r#""kind":"capsule_inventory""#)
        );
    }

    // The first source is valid but the second file contains the wrong object.
    // Neither missing nor corrupt local data may be replaced by this batch.
    let invalid = [
        "--restore-capsule",
        oid,
        "backup.capsule",
        "--restore-capsule",
        oid2,
        "backup.capsule",
    ];
    let rejected = invoke(&root, "scrub", &invalid);
    assert_eq!(rejected.status.code(), Some(4));
    assert!(!live.exists());
    assert_eq!(fs::read(&second).unwrap(), b"damaged");
    assert_eq!(fs::read(&history_path).unwrap(), history);

    let restore = [
        "--restore-capsule",
        oid,
        "backup.capsule",
        "--restore-capsule",
        oid2,
        "second.capsule",
    ];
    for _ in 0..2 {
        let report = String::from_utf8(success(&root, "scrub", &restore)).unwrap();
        for id in [oid, oid2] {
            assert!(report.contains(&format!(
                r#""object":"{id}","kind":"capsule","state":"backup_verified""#
            )));
        }
        assert!(report.contains(r#""kind":"scrubbed""#));
        assert_eq!(success(&root, "query", &query), expected_rows);
        assert_eq!(fs::read(&history_path).unwrap(), history);
    }
}
