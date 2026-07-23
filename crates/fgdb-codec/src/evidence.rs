//! Deterministic diagnostic evidence for scalar codec runs.
//!
//! A [`CodecRunRow`] describes encoded-output mechanics. Its codec and corpus
//! IDs are symbolic diagnostics, not registered durable identifiers. Its
//! checksum covers the encoded bytes only; it is neither a logical-content
//! digest nor an object identity and must never be used as either.

#![forbid(unsafe_code)]

use core::fmt;

use crate::kernel::{DispatchPath, KernelOutput};

const NDJSON_CODEC_PREFIX: &str = "{\"codec_id\":\"";
const NDJSON_CORPUS_SEPARATOR: &str = "\",\"corpus_id\":\"";
const NDJSON_ENTRY_SEPARATOR: &str = "\",\"entry_count\":";
const NDJSON_ENCODED_SEPARATOR: &str = ",\"encoded_bytes\":";
const NDJSON_RATIO_SEPARATOR: &str = ",\"bytes_per_entry\":";
const NDJSON_RATIO_PREFIX: &str = "{\"numerator\":";
const NDJSON_RATIO_MIDDLE: &str = ",\"denominator\":";
const NDJSON_RATIO_SUFFIX: &str = "}";
const NDJSON_NULL: &str = "null";
const NDJSON_DISPATCH_SEPARATOR: &str = ",\"dispatch_path\":\"";
const NDJSON_CHECKSUM_PREFIX: &str = "\",\"output_checksum\":{\"algorithm\":\"";
const NDJSON_CHECKSUM_MIDDLE: &str = "\",\"hex\":\"";
const NDJSON_SUFFIX: &str = "\"}}\n";

/// Maximum UTF-8 bytes in one symbolic evidence identifier.
pub const MAX_EVIDENCE_ID_BYTES: usize = 256;
/// Maximum bytes in one emitted NDJSON row.
pub const MAX_NDJSON_ROW_BYTES: usize = 4_096;

/// Exact normalized byte count per logical entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExactRatio {
    numerator: usize,
    denominator: usize,
}

impl ExactRatio {
    fn new(numerator: usize, denominator: usize) -> Self {
        debug_assert_ne!(denominator, 0);
        let divisor = greatest_common_divisor(numerator, denominator);
        Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        }
    }

    /// Returns the normalized numerator.
    #[must_use]
    pub const fn numerator(self) -> usize {
        self.numerator
    }

    /// Returns the positive normalized denominator.
    #[must_use]
    pub const fn denominator(self) -> usize {
        self.denominator
    }
}

/// Exact byte-per-entry accounting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BytesPerEntry {
    /// A run with no logical entries has no defined bytes-per-entry ratio.
    UndefinedForEmpty,
    /// A normalized exact ratio for a nonempty run.
    Exact(ExactRatio),
}

/// Version-tagged checksum over encoded output bytes.
///
/// This value is diagnostic evidence only. It does not authenticate logical
/// content and is not a durable object identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EncodedOutputChecksum(u64);

impl EncodedOutputChecksum {
    /// Explicit algorithm tag emitted alongside every checksum.
    pub const ALGORITHM: &'static str = "fnv1a64-output-evidence-v1";

    /// Computes the stable evidence checksum for `encoded_output`.
    #[must_use]
    pub fn from_bytes(encoded_output: &[u8]) -> Self {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut checksum = OFFSET_BASIS;
        for &byte in encoded_output {
            checksum ^= u64::from(byte);
            // Wrapping multiplication is the defined FNV-1a recurrence.
            checksum = checksum.wrapping_mul(PRIME);
        }
        Self(checksum)
    }

    /// Returns the raw diagnostic checksum value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Allocation involved in constructing or encoding one evidence row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceAllocation {
    /// Owned symbolic codec identifier.
    CodecId,
    /// Owned symbolic corpus identifier.
    CorpusId,
    /// Complete one-line NDJSON output.
    NdjsonLine,
}

/// Checked evidence-row construction or encoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceError {
    /// A symbolic diagnostic identifier exceeds the fixed scalar bound.
    IdentifierTooLong {
        /// Identifier being copied.
        target: EvidenceAllocation,
        /// UTF-8 bytes supplied.
        actual: usize,
        /// Fixed UTF-8 byte ceiling.
        limit: usize,
    },
    /// A complete output length was not representable as `usize`.
    NdjsonLengthOverflow,
    /// A complete row exceeds the fixed scalar evidence ceiling.
    NdjsonRowLimitExceeded {
        /// Exact row bytes required.
        required: usize,
        /// Fixed row ceiling.
        limit: usize,
    },
    /// Reserving owned diagnostic storage failed.
    AllocationFailed {
        /// Storage being reserved.
        target: EvidenceAllocation,
        /// Exact bytes requested.
        requested: usize,
    },
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IdentifierTooLong {
                target,
                actual,
                limit,
            } => write!(
                formatter,
                "codec evidence {target:?} has {actual} UTF-8 bytes, limit is {limit}"
            ),
            Self::NdjsonLengthOverflow => {
                formatter.write_str("codec evidence NDJSON length overflows usize")
            }
            Self::NdjsonRowLimitExceeded { required, limit } => write!(
                formatter,
                "codec evidence NDJSON needs {required} bytes, limit is {limit}"
            ),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} bytes for codec evidence {target:?}"
            ),
        }
    }
}

impl std::error::Error for EvidenceError {}

/// One deterministic encoded-output evidence row.
///
/// The string IDs are explicitly symbolic evidence identifiers: registration,
/// durable framing, and logical digests belong to other layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecRunRow {
    codec_id: String,
    corpus_id: String,
    entry_count: usize,
    encoded_bytes: usize,
    bytes_per_entry: BytesPerEntry,
    dispatch_path: DispatchPath,
    output_checksum: EncodedOutputChecksum,
}

impl CodecRunRow {
    /// Constructs a row from symbolic IDs and the exact encoded output.
    fn try_new(
        codec_id: &str,
        corpus_id: &str,
        entry_count: usize,
        encoded_output: &[u8],
        dispatch_path: DispatchPath,
    ) -> Result<Self, EvidenceError> {
        let codec_id = try_copy_id(codec_id, EvidenceAllocation::CodecId)?;
        let corpus_id = try_copy_id(corpus_id, EvidenceAllocation::CorpusId)?;
        let encoded_bytes = encoded_output.len();
        let bytes_per_entry = if entry_count == 0 {
            BytesPerEntry::UndefinedForEmpty
        } else {
            BytesPerEntry::Exact(ExactRatio::new(encoded_bytes, entry_count))
        };

        Ok(Self {
            codec_id,
            corpus_id,
            entry_count,
            encoded_bytes,
            bytes_per_entry,
            dispatch_path,
            output_checksum: EncodedOutputChecksum::from_bytes(encoded_output),
        })
    }

    /// Constructs a row from bytes provenance-bound to their selected kernel.
    ///
    /// The caller supplies only symbolic, non-durable IDs and the logical entry
    /// count. Both the exact bytes and their dispatch path come from the sealed
    /// [`KernelOutput`], so an evidence caller cannot relabel arbitrary bytes.
    pub fn try_from_kernel_output(
        codec_id: &str,
        corpus_id: &str,
        entry_count: usize,
        encoded_output: &KernelOutput,
    ) -> Result<Self, EvidenceError> {
        Self::try_new(
            codec_id,
            corpus_id,
            entry_count,
            encoded_output.as_bytes(),
            encoded_output.dispatch_path(),
        )
    }

    /// Returns the symbolic, explicitly non-durable codec ID.
    #[must_use]
    pub fn codec_id(&self) -> &str {
        &self.codec_id
    }

    /// Returns the symbolic corpus ID.
    #[must_use]
    pub fn corpus_id(&self) -> &str {
        &self.corpus_id
    }

    /// Returns the logical entry count supplied by the caller.
    #[must_use]
    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// Returns the exact encoded-output byte count.
    #[must_use]
    pub const fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }

    /// Returns exact byte-per-entry accounting.
    #[must_use]
    pub const fn bytes_per_entry(&self) -> BytesPerEntry {
        self.bytes_per_entry
    }

    /// Returns the implementation path that produced the output.
    #[must_use]
    pub const fn dispatch_path(&self) -> DispatchPath {
        self.dispatch_path
    }

    /// Returns the encoded-output evidence checksum.
    #[must_use]
    pub const fn output_checksum(&self) -> EncodedOutputChecksum {
        self.output_checksum
    }

    /// Encodes one deterministic NDJSON line with a fixed key order.
    ///
    /// `bytes_per_entry` is `null` when `entry_count == 0`; otherwise it is a
    /// normalized `{numerator, denominator}` object. The returned string ends
    /// in exactly one newline.
    pub fn to_ndjson(&self) -> Result<String, EvidenceError> {
        let capacity = self.ndjson_len()?;
        if capacity > MAX_NDJSON_ROW_BYTES {
            return Err(EvidenceError::NdjsonRowLimitExceeded {
                required: capacity,
                limit: MAX_NDJSON_ROW_BYTES,
            });
        }
        let mut output = String::new();
        output
            .try_reserve_exact(capacity)
            .map_err(|_| EvidenceError::AllocationFailed {
                target: EvidenceAllocation::NdjsonLine,
                requested: capacity,
            })?;

        output.push_str(NDJSON_CODEC_PREFIX);
        push_json_string_content(&mut output, &self.codec_id);
        output.push_str(NDJSON_CORPUS_SEPARATOR);
        push_json_string_content(&mut output, &self.corpus_id);
        output.push_str(NDJSON_ENTRY_SEPARATOR);
        push_usize_decimal(&mut output, self.entry_count);
        output.push_str(NDJSON_ENCODED_SEPARATOR);
        push_usize_decimal(&mut output, self.encoded_bytes);
        output.push_str(NDJSON_RATIO_SEPARATOR);
        match self.bytes_per_entry {
            BytesPerEntry::UndefinedForEmpty => output.push_str(NDJSON_NULL),
            BytesPerEntry::Exact(ratio) => {
                output.push_str(NDJSON_RATIO_PREFIX);
                push_usize_decimal(&mut output, ratio.numerator());
                output.push_str(NDJSON_RATIO_MIDDLE);
                push_usize_decimal(&mut output, ratio.denominator());
                output.push_str(NDJSON_RATIO_SUFFIX);
            }
        }
        output.push_str(NDJSON_DISPATCH_SEPARATOR);
        output.push_str(self.dispatch_path.evidence_label());
        output.push_str(NDJSON_CHECKSUM_PREFIX);
        output.push_str(EncodedOutputChecksum::ALGORITHM);
        output.push_str(NDJSON_CHECKSUM_MIDDLE);
        push_u64_hex(&mut output, self.output_checksum.value());
        output.push_str(NDJSON_SUFFIX);

        debug_assert_eq!(output.len(), capacity);
        Ok(output)
    }

    fn ndjson_len(&self) -> Result<usize, EvidenceError> {
        let mut length = 0_usize;
        add_len(&mut length, NDJSON_CODEC_PREFIX.len())?;
        add_len(&mut length, escaped_json_len(&self.codec_id)?)?;
        add_len(&mut length, NDJSON_CORPUS_SEPARATOR.len())?;
        add_len(&mut length, escaped_json_len(&self.corpus_id)?)?;
        add_len(&mut length, NDJSON_ENTRY_SEPARATOR.len())?;
        add_len(&mut length, usize_decimal_len(self.entry_count))?;
        add_len(&mut length, NDJSON_ENCODED_SEPARATOR.len())?;
        add_len(&mut length, usize_decimal_len(self.encoded_bytes))?;
        add_len(&mut length, NDJSON_RATIO_SEPARATOR.len())?;
        match self.bytes_per_entry {
            BytesPerEntry::UndefinedForEmpty => add_len(&mut length, NDJSON_NULL.len())?,
            BytesPerEntry::Exact(ratio) => {
                add_len(&mut length, NDJSON_RATIO_PREFIX.len())?;
                add_len(&mut length, usize_decimal_len(ratio.numerator()))?;
                add_len(&mut length, NDJSON_RATIO_MIDDLE.len())?;
                add_len(&mut length, usize_decimal_len(ratio.denominator()))?;
                add_len(&mut length, NDJSON_RATIO_SUFFIX.len())?;
            }
        }
        add_len(&mut length, NDJSON_DISPATCH_SEPARATOR.len())?;
        add_len(&mut length, self.dispatch_path.evidence_label().len())?;
        add_len(&mut length, NDJSON_CHECKSUM_PREFIX.len())?;
        add_len(&mut length, EncodedOutputChecksum::ALGORITHM.len())?;
        add_len(&mut length, NDJSON_CHECKSUM_MIDDLE.len())?;
        add_len(&mut length, 16)?;
        add_len(&mut length, NDJSON_SUFFIX.len())?;
        Ok(length)
    }
}

fn try_copy_id(source: &str, target: EvidenceAllocation) -> Result<String, EvidenceError> {
    if source.len() > MAX_EVIDENCE_ID_BYTES {
        return Err(EvidenceError::IdentifierTooLong {
            target,
            actual: source.len(),
            limit: MAX_EVIDENCE_ID_BYTES,
        });
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(source.len())
        .map_err(|_| EvidenceError::AllocationFailed {
            target,
            requested: source.len(),
        })?;
    owned.push_str(source);
    Ok(owned)
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    debug_assert_ne!(right, 0);
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn add_len(total: &mut usize, additional: usize) -> Result<(), EvidenceError> {
    *total = total
        .checked_add(additional)
        .ok_or(EvidenceError::NdjsonLengthOverflow)?;
    Ok(())
}

fn escaped_json_len(value: &str) -> Result<usize, EvidenceError> {
    let mut length = 0_usize;
    for character in value.chars() {
        let additional = match character {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        add_len(&mut length, additional)?;
    }
    Ok(length)
}

fn push_json_string_content(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{0008}' => output.push_str("\\b"),
            '\u{000c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{0000}'..='\u{001f}' => {
                output.push_str("\\u00");
                let byte = character as u8;
                output.push(hex_digit(byte >> 4));
                output.push(hex_digit(byte & 0x0f));
            }
            _ => output.push(character),
        }
    }
}

fn usize_decimal_len(mut value: usize) -> usize {
    let mut digits = 1_usize;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn push_usize_decimal(output: &mut String, mut value: usize) {
    const BUFFER_LEN: usize = usize::BITS as usize;
    let mut reversed = [0_u8; BUFFER_LEN];
    let mut cursor = BUFFER_LEN;

    loop {
        cursor -= 1;
        reversed[cursor] = (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for &digit in &reversed[cursor..] {
        output.push(char::from(b'0' + digit));
    }
}

fn push_u64_hex(output: &mut String, value: u64) {
    for nibble_index in (0..16).rev() {
        let shift = nibble_index * 4;
        let nibble = ((value >> shift) & 0x0f) as u8;
        output.push(hex_digit(nibble));
    }
}

fn hex_digit(nibble: u8) -> char {
    debug_assert!(nibble < 16);
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + (nibble - 10)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block::CodecProfile,
        kernel::{BlockKernel, ScalarKernels},
    };

    #[test]
    fn rational_accounting_is_exact_normalized_and_empty_is_undefined() {
        let row = CodecRunRow::try_new(
            "bitpack",
            "six-byte-corpus",
            4,
            &[0; 6],
            DispatchPath::Scalar,
        )
        .expect("small labels fit");
        assert_eq!(
            row.bytes_per_entry(),
            BytesPerEntry::Exact(ExactRatio::new(3, 2))
        );
        assert_eq!(row.entry_count(), 4);
        assert_eq!(row.encoded_bytes(), 6);

        let empty = CodecRunRow::try_new("bitpack", "empty", 0, &[0xa5], DispatchPath::Scalar)
            .expect("small labels fit");
        assert_eq!(empty.bytes_per_entry(), BytesPerEntry::UndefinedForEmpty);
    }

    #[test]
    fn ndjson_freezes_order_escaping_controls_unicode_and_ratio() {
        let row = CodecRunRow::try_new(
            "bit\"pack\\λ\n",
            "snow雪\u{0000}\t",
            2,
            &[0, 1, 2],
            DispatchPath::Scalar,
        )
        .expect("small labels fit");

        assert_eq!(
            row.to_ndjson().expect("small output fits"),
            "{\"codec_id\":\"bit\\\"pack\\\\λ\\n\",\"corpus_id\":\"snow雪\\u0000\\t\",\"entry_count\":2,\"encoded_bytes\":3,\"bytes_per_entry\":{\"numerator\":3,\"denominator\":2},\"dispatch_path\":\"scalar\",\"output_checksum\":{\"algorithm\":\"fnv1a64-output-evidence-v1\",\"hex\":\"d949aa186c0c4928\"}}\n"
        );
    }

    #[test]
    fn empty_row_has_frozen_null_ratio_and_empty_checksum() {
        let row = CodecRunRow::try_new("block", "empty", 0, &[], DispatchPath::Scalar)
            .expect("small labels fit");
        assert_eq!(
            row.to_ndjson().expect("small output fits"),
            "{\"codec_id\":\"block\",\"corpus_id\":\"empty\",\"entry_count\":0,\"encoded_bytes\":0,\"bytes_per_entry\":null,\"dispatch_path\":\"scalar\",\"output_checksum\":{\"algorithm\":\"fnv1a64-output-evidence-v1\",\"hex\":\"cbf29ce484222325\"}}\n"
        );
    }

    #[test]
    fn evidence_is_deterministic_and_checksum_changes_with_output_bytes() {
        let first =
            CodecRunRow::try_new("neighbor", "fixture", 3, &[1, 2, 3], DispatchPath::Scalar)
                .expect("small labels fit");
        let same = CodecRunRow::try_new("neighbor", "fixture", 3, &[1, 2, 3], DispatchPath::Scalar)
            .expect("small labels fit");
        let changed =
            CodecRunRow::try_new("neighbor", "fixture", 3, &[1, 2, 4], DispatchPath::Scalar)
                .expect("small labels fit");

        assert_eq!(first, same);
        assert_eq!(
            first.to_ndjson().expect("small output fits"),
            same.to_ndjson().expect("small output fits")
        );
        assert_ne!(first.output_checksum(), changed.output_checksum());
    }

    #[test]
    fn row_records_the_trait_selected_evidence_path() {
        let kernels = ScalarKernels;
        let profile = CodecProfile::try_new(1_024, 256, 1_024).expect("valid scalar profile");
        let encoded = BlockKernel::compress_output(&kernels, b"trait-fixture", profile)
            .expect("valid scalar block fixture");
        let row =
            CodecRunRow::try_from_kernel_output("block-symbolic", "trait-fixture", 3, &encoded)
                .expect("small labels fit");

        assert_eq!(row.codec_id(), "block-symbolic");
        assert_eq!(row.corpus_id(), "trait-fixture");
        assert_eq!(row.dispatch_path(), DispatchPath::Scalar);
        assert_eq!(row.encoded_bytes(), encoded.len());
    }

    #[test]
    fn identifier_bounds_fail_before_copying_or_row_encoding() {
        let oversized = "x".repeat(MAX_EVIDENCE_ID_BYTES + 1);
        assert_eq!(
            CodecRunRow::try_new(&oversized, "corpus", 1, &[0], DispatchPath::Scalar),
            Err(EvidenceError::IdentifierTooLong {
                target: EvidenceAllocation::CodecId,
                actual: MAX_EVIDENCE_ID_BYTES + 1,
                limit: MAX_EVIDENCE_ID_BYTES,
            })
        );

        let controls = "\u{0001}".repeat(MAX_EVIDENCE_ID_BYTES);
        let row = CodecRunRow::try_new(&controls, &controls, usize::MAX, &[], DispatchPath::Scalar)
            .expect("bounded identifiers fit owned storage");
        assert!(
            row.to_ndjson()
                .expect("maximum escaped identifiers fit the row ceiling")
                .len()
                <= MAX_NDJSON_ROW_BYTES
        );
    }
}
