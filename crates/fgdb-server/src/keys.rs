//! Owner-only key files, read with the same rules as the `fgdb` CLI.
//!
//! A database key file holds three lines of 64 hex characters (object-id
//! key, security namespace, encryption key). An issuer key file holds one
//! such line: the Warden HMAC root key that mints and verifies capability
//! tokens for the databases it is configured with. `#` starts a comment. On
//! Unix either file must be a regular file with no group or other permission
//! bits. Key bytes are never printed.

use asupersync::Cx;
use asupersync::io::AsyncReadExt as _;
use asupersync::security::key::AuthKey;
use fgdb::DatabaseKeys;
use fgdb_types::DatabaseSecurityNamespaceId;
use std::path::Path;

const MAX_KEY_FILE_BYTES: u64 = 65_536;
const MAX_PASSPHRASE_FILE_BYTES: u64 = 4_096;

/// Redacted key-file failures; none carries key material or file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFileError(&'static str);

impl core::fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}
impl core::error::Error for KeyFileError {}

/// Parse `expected` 32-byte hex key lines from key-file text.
pub fn parse_key_lines(text: &str, expected: usize) -> Result<Vec<[u8; 32]>, KeyFileError> {
    let lines: Vec<_> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != expected {
        return Err(KeyFileError(if expected == 1 {
            "key file requires exactly one key line"
        } else {
            "key file requires three key lines"
        }));
    }
    lines
        .into_iter()
        .map(|line| {
            if line.len() != 64 || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(KeyFileError("key lines require 64 hexadecimal characters"));
            }
            let mut key = [0u8; 32];
            for (byte, pair) in key.iter_mut().zip(line.as_bytes().as_chunks::<2>().0) {
                *byte = (hex_digit(pair[0]) << 4) | hex_digit(pair[1]);
            }
            Ok(key)
        })
        .collect()
}

fn hex_digit(b: u8) -> u8 {
    if b.is_ascii_digit() {
        b - b'0'
    } else {
        b.to_ascii_lowercase() - b'a' + 10
    }
}

pub(crate) async fn read_owner_only(cx: &Cx, path: &Path) -> Result<String, KeyFileError> {
    read_owner_only_bounded(cx, path, MAX_KEY_FILE_BYTES, "key file").await
}

async fn read_owner_only_bounded(
    cx: &Cx,
    path: &Path,
    max_bytes: u64,
    kind: &'static str,
) -> Result<String, KeyFileError> {
    cx.checkpoint()
        .map_err(|_| KeyFileError("key file read cancelled"))?;
    let unreadable = |_| match kind {
        "passphrase file" => KeyFileError("cannot read passphrase file"),
        _ => KeyFileError("cannot read key file"),
    };
    let not_regular = match kind {
        "passphrase file" => KeyFileError("passphrase file must be a regular file"),
        _ => KeyFileError("key file must be a regular file"),
    };
    // Refuse a FIFO or device before opening it: opening a FIFO blocks until
    // a writer appears, so the handle check below would never run.
    let named = asupersync::fs::metadata(path).await.map_err(unreadable)?;
    if !named.is_file() {
        return Err(not_regular);
    }
    let file = asupersync::fs::File::open(path).await.map_err(unreadable)?;
    // Validate the opened handle, not a path that could be swapped between a
    // check and the read.
    let metadata = file.metadata().await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(not_regular);
    }
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(match kind {
                "passphrase file" => KeyFileError(
                    "passphrase file must not be accessible to group or others (chmod 600)",
                ),
                _ => KeyFileError("key file must not be accessible to group or others (chmod 600)"),
            });
        }
    }
    let mut text = String::new();
    file.take(max_bytes + 1)
        .read_to_string(&mut text)
        .await
        .map_err(unreadable)?;
    if text.len() as u64 > max_bytes {
        return Err(match kind {
            "passphrase file" => KeyFileError("passphrase file exceeds 4096 bytes"),
            _ => KeyFileError("key file exceeds 65536 bytes"),
        });
    }
    Ok(text)
}

/// Read an owner-only KEK passphrase file, stripping one trailing newline.
pub async fn read_passphrase_file(cx: &Cx, path: &Path) -> Result<String, KeyFileError> {
    let raw =
        read_owner_only_bounded(cx, path, MAX_PASSPHRASE_FILE_BYTES, "passphrase file").await?;
    let trimmed = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(&raw);
    if trimmed.is_empty() {
        return Err(KeyFileError("passphrase file must not be empty"));
    }
    Ok(trimmed.to_owned())
}

/// Read a three-line database key file or an `FGKE-V1` passphrase-wrapped key
/// envelope, optionally enabling Strata Encrypt-then-Code (`FGEC` v1).
pub async fn read_database_keys_with_options(
    cx: &Cx,
    path: &Path,
    passphrase_file: Option<&Path>,
    strata_ecs: bool,
) -> Result<DatabaseKeys, KeyFileError> {
    let text = read_owner_only(cx, path).await?;
    if fgdb::is_key_envelope_v1(&text) {
        let passphrase_path = passphrase_file.ok_or(KeyFileError(
            "FGKE-V1 wrapped key file requires --kek-passphrase-file",
        ))?;
        let passphrase = read_passphrase_file(cx, passphrase_path).await?;
        return DatabaseKeys::open_passphrase_envelope(passphrase.as_bytes(), &text).map_err(
            |err| match err {
                fgdb::KeyEnvelopeError::AuthenticationFailed => {
                    KeyFileError("FGKE-V1 key envelope authentication failed")
                }
                _ => KeyFileError("invalid FGKE-V1 key envelope"),
            },
        );
    }
    if passphrase_file.is_some() {
        return Err(KeyFileError(
            "--kek-passphrase-file requires an FGKE-V1 wrapped key file",
        ));
    }
    let keys = parse_key_lines(&text, 3)?;
    let out = DatabaseKeys::new(keys[0], DatabaseSecurityNamespaceId(keys[1]), keys[2]);
    Ok(if strata_ecs {
        out.with_default_strata_ecs()
    } else {
        out
    })
}

/// Read a three-line database key file.
pub async fn read_database_keys(cx: &Cx, path: &Path) -> Result<DatabaseKeys, KeyFileError> {
    read_database_keys_with_options(cx, path, None, false).await
}

/// Read a one-line Warden issuer key file.
pub async fn read_issuer_key(cx: &Cx, path: &Path) -> Result<AuthKey, KeyFileError> {
    let key = parse_key_lines(&read_owner_only(cx, path).await?, 1)?[0];
    AuthKey::from_bytes(key).map_err(|_| KeyFileError("issuer key is not a valid HMAC key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_lines_parse_exactly() {
        let line = "00".repeat(31) + "fF";
        let text = format!("# comment\n{line}  # trailing\n\n");
        assert_eq!(parse_key_lines(&text, 1).unwrap()[0][31], 0xff);
        assert!(parse_key_lines(&text, 3).is_err());
        assert!(parse_key_lines(&line[1..], 1).is_err());
        assert!(parse_key_lines(&("g".to_owned() + &line[1..]), 1).is_err());
    }
}
