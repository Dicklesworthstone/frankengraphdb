//! Black-box proof that unusable first JSONL input precedes credential access.
//! The credential path does not exist, so reaching connection setup would be
//! an open failure rather than the required input usage refusal.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn input(bytes: &[u8]) -> (PathBuf, PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "fgdb-prepared-jsonl-input-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rows.jsonl");
    std::fs::write(&path, bytes).unwrap();
    (path, directory.join("missing-token.hex"))
}

#[test]
fn prepared_jsonl_empty_invalid_and_oversized_first_records_never_read_credentials() {
    let oversized = vec![b' '; 1_048_577];
    for bytes in [
        b"".as_slice(),
        b"\n".as_slice(),
        b"not JSON\n".as_slice(),
        b"{\"name\":\"invalid:secret\"}\n".as_slice(),
        oversized.as_slice(),
    ] {
        let (path, token) = input(bytes);
        let output = Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args(["--robot", "remote", "--addr", "127.0.0.1:9", "--token-file"])
            .arg(&token)
            .args(["--database", "social", "--prepared-jsonl"])
            .arg(&path)
            .args(["write", "CREATE (:Person {name:$name})"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(r#""event":"error""#));
        assert!(stdout.contains(r#""class":"usage""#));
        assert!(!stdout.contains(r#""event":"result""#));
        assert!(!stdout.contains("secret"));
        assert!(!stdout.contains("missing-token"));
    }
}
