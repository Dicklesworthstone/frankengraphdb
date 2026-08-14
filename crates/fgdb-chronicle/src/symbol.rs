//! The `SymbolRecord` durable wire format and its authentication transcript.
//!
//! A symbol is one RaptorQ-coded piece of a protected object. Its record is
//! the smallest durable unit Chronicle writes, and it carries a MAC under the
//! per-encoding `K_symbol` so bit rot and substitution are both detectable.
//!
//! TWO LAWS SHAPE THIS FILE, and both are the kind that fail silently if you
//! get them subtly wrong:
//!
//! 1. **The MAC transcript is total.** It covers the domain/version plus
//!    *every* serialized header field except the MAC bytes themselves, then
//!    the payload. There is no shorter competing transcript — a field outside
//!    the transcript is a field an attacker may edit freely, so
//!    [`SymbolRecord::mac_transcript`] is derived from the same serializer
//!    that writes the record rather than from a hand-copied field list.
//!
//! 2. **A bare record is never self-authorizing.** The decoder must obtain the
//!    complete descriptor, OTI, and auth profile from an *authenticated*
//!    encoding descriptor before accepting any symbol. [`SymbolRecord::verify`]
//!    therefore takes an [`EncodedObject`] and checks the record against it;
//!    there is no entry point that authenticates a record against itself.

use crate::identity::{
    CryptoVerificationSink, EncodedObject, VerificationFailureClass, VerificationOperation,
    VerificationOutcome, verification_event,
};
use fgdb_crypto::Digest;
use fgdb_types::ids::ObjectId;

/// Wire magic. Present so a stray file is identifiable and a misaligned read
/// fails immediately rather than decoding garbage.
pub const SYMBOL_MAGIC: [u8; 4] = *b"FGEC";

/// The wire format version this build writes.
pub const SYMBOL_FORMAT_VERSION: u16 = 1;

/// The symbol-MAC transcript domain string. Domain separation is what stops a
/// MAC computed over one record class from validating another.
pub const SYMBOL_MAC_DOMAIN: &[u8] = b"fgdb:symbol-record:v1";

/// Serialized header length for `format_version = 1`: the fixed prefix through
/// `symbol_mac_len`, excluding payload and MAC.
pub const HEADER_LEN_V1: u16 = 4 + 2 + 2 + 4 + 32 + 32 + 32 + 2 + 4 + 4 + 4 + 8 + 8 + 4 + 4 + 2 + 2;

/// The MAC length this build writes (BLAKE3 keyed, truncated to 128 bits —
/// the profile is recorded in the header so a future profile is additive).
pub const SYMBOL_MAC_LEN_V1: u16 = 16;

/// The symbol-MAC profile id for keyed BLAKE3 truncated to 128 bits.
pub const SYMBOL_MAC_PROFILE_BLAKE3_128: u16 = 1;

/// Why a record was rejected. Every variant is a fail-closed outcome: no
/// caller ever receives payload bytes from a record that did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolError {
    /// The buffer is shorter than the fixed header, or shorter than its own
    /// declared `record_len`.
    Truncated,
    /// Magic, version, header length, or MAC profile is not one this build
    /// writes. Unknown means rejected, never ignored.
    UnsupportedFraming,
    /// A declared length does not agree with the buffer or with another
    /// declared length.
    InconsistentLengths,
    /// The record does not belong to the encoding it was checked against —
    /// a different `EncodingId`, `CiphertextId`, object, or kind.
    ForeignEncoding,
    /// The MAC did not verify over the complete transcript.
    AuthenticationFailed,
}

impl core::fmt::Display for SymbolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "symbol record is truncated",
            Self::UnsupportedFraming => "symbol record framing is not supported",
            Self::InconsistentLengths => "symbol record lengths disagree",
            Self::ForeignEncoding => "symbol record belongs to another encoding",
            Self::AuthenticationFailed => "symbol record authentication failed",
        })
    }
}

impl core::error::Error for SymbolError {}

/// One durable, authenticated RaptorQ symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    pub format_version: u16,
    pub logical_oid: ObjectId,
    pub ciphertext_id: Digest,
    pub encoding_id: Digest,
    pub object_kind: u16,
    pub source_block: u32,
    pub esi: u32,
    pub transfer_length: u64,
    pub oti_common: u64,
    pub oti_scheme: u32,
    pub flags: u32,
    pub symbol_mac_profile: u16,
    pub payload: Vec<u8>,
}

impl SymbolRecord {
    /// Total serialized length: header + payload + MAC.
    pub fn record_len(&self) -> u32 {
        u32::from(HEADER_LEN_V1)
            + u32::try_from(self.payload.len()).expect("symbol payload fits u32")
            + u32::from(SYMBOL_MAC_LEN_V1)
    }

    /// The serialized header — the single source of the field order. Both
    /// [`SymbolRecord::serialize`] and [`SymbolRecord::mac_transcript`] build
    /// on this, so a new field cannot land in the record while being omitted
    /// from the transcript.
    fn serialize_header(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::from(HEADER_LEN_V1));
        out.extend_from_slice(&SYMBOL_MAGIC);
        out.extend_from_slice(&self.format_version.to_le_bytes());
        out.extend_from_slice(&HEADER_LEN_V1.to_le_bytes());
        out.extend_from_slice(&self.record_len().to_le_bytes());
        out.extend_from_slice(&self.logical_oid.0);
        out.extend_from_slice(&self.ciphertext_id.0);
        out.extend_from_slice(&self.encoding_id.0);
        out.extend_from_slice(&self.object_kind.to_le_bytes());
        out.extend_from_slice(&self.source_block.to_le_bytes());
        out.extend_from_slice(&self.esi.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.payload.len())
                .expect("symbol payload fits u32")
                .to_le_bytes(),
        );
        out.extend_from_slice(&self.transfer_length.to_le_bytes());
        out.extend_from_slice(&self.oti_common.to_le_bytes());
        out.extend_from_slice(&self.oti_scheme.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.symbol_mac_profile.to_le_bytes());
        out.extend_from_slice(&SYMBOL_MAC_LEN_V1.to_le_bytes());
        debug_assert_eq!(out.len(), usize::from(HEADER_LEN_V1));
        out
    }

    /// The exact canonical MAC transcript: domain/version, then every
    /// serialized header field (the MAC bytes are not among them because they
    /// are not part of the header), then the payload.
    pub fn mac_transcript(&self) -> Vec<u8> {
        let header = self.serialize_header();
        let mut transcript =
            Vec::with_capacity(SYMBOL_MAC_DOMAIN.len() + 2 + header.len() + self.payload.len());
        transcript.extend_from_slice(SYMBOL_MAC_DOMAIN);
        transcript.extend_from_slice(&self.format_version.to_le_bytes());
        transcript.extend_from_slice(&header);
        transcript.extend_from_slice(&self.payload);
        transcript
    }

    /// Compute the symbol MAC under the per-encoding `K_symbol`.
    pub fn compute_mac(&self, k_symbol: &[u8; 32]) -> [u8; 16] {
        let digest = fgdb_crypto::keyed_hash(k_symbol, &self.mac_transcript());
        let mut mac = [0u8; 16];
        mac.copy_from_slice(&digest.0[..usize::from(SYMBOL_MAC_LEN_V1)]);
        mac
    }

    /// Serialize to durable bytes: header, payload, MAC.
    pub fn serialize(&self, k_symbol: &[u8; 32]) -> Vec<u8> {
        let mut out = self.serialize_header();
        out.extend_from_slice(&self.payload);
        out.extend_from_slice(&self.compute_mac(k_symbol));
        out
    }

    /// Parse framing WITHOUT authenticating. Deliberately private: the only
    /// public path to a record is [`SymbolRecord::verify`], which requires an
    /// authenticated encoding, so a bare record can never be self-authorizing.
    fn parse_framing(bytes: &[u8]) -> Result<(Self, [u8; 16]), SymbolError> {
        if bytes.len() < usize::from(HEADER_LEN_V1) {
            return Err(SymbolError::Truncated);
        }
        if bytes[..4] != SYMBOL_MAGIC {
            return Err(SymbolError::UnsupportedFraming);
        }
        let mut cursor = 4usize;
        let take_u16 = |cursor: &mut usize| {
            let value = u16::from_le_bytes([bytes[*cursor], bytes[*cursor + 1]]);
            *cursor += 2;
            value
        };
        let format_version = take_u16(&mut cursor);
        let header_len = take_u16(&mut cursor);
        if format_version != SYMBOL_FORMAT_VERSION || header_len != HEADER_LEN_V1 {
            return Err(SymbolError::UnsupportedFraming);
        }

        let record_len = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]);
        cursor += 4;

        let take_32 = |cursor: &mut usize| {
            let mut value = [0u8; 32];
            value.copy_from_slice(&bytes[*cursor..*cursor + 32]);
            *cursor += 32;
            value
        };
        let logical_oid = ObjectId(take_32(&mut cursor));
        let ciphertext_id = Digest(take_32(&mut cursor));
        let encoding_id = Digest(take_32(&mut cursor));

        let object_kind = u16::from_le_bytes([bytes[cursor], bytes[cursor + 1]]);
        cursor += 2;
        let take_u32 = |cursor: &mut usize| {
            let value = u32::from_le_bytes([
                bytes[*cursor],
                bytes[*cursor + 1],
                bytes[*cursor + 2],
                bytes[*cursor + 3],
            ]);
            *cursor += 4;
            value
        };
        let source_block = take_u32(&mut cursor);
        let esi = take_u32(&mut cursor);
        let symbol_len = take_u32(&mut cursor);
        let take_u64 = |cursor: &mut usize| {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[*cursor..*cursor + 8]);
            *cursor += 8;
            u64::from_le_bytes(value)
        };
        let transfer_length = take_u64(&mut cursor);
        let oti_common = take_u64(&mut cursor);
        let oti_scheme = take_u32(&mut cursor);
        let flags = take_u32(&mut cursor);
        let symbol_mac_profile = u16::from_le_bytes([bytes[cursor], bytes[cursor + 1]]);
        cursor += 2;
        let symbol_mac_len = u16::from_le_bytes([bytes[cursor], bytes[cursor + 1]]);
        cursor += 2;

        // ubs:ignore — public wire-profile discriminator, not secret material.
        if symbol_mac_profile != SYMBOL_MAC_PROFILE_BLAKE3_128
            // ubs:ignore — public framing length, not secret material.
            || symbol_mac_len != SYMBOL_MAC_LEN_V1
        {
            return Err(SymbolError::UnsupportedFraming);
        }

        // Every declared length must agree with every other and with the
        // buffer; a length that only agrees with itself is how a decoder gets
        // walked off the end of a record.
        let expected_record_len = u32::from(HEADER_LEN_V1)
            .checked_add(symbol_len)
            .and_then(|len| len.checked_add(u32::from(SYMBOL_MAC_LEN_V1)))
            .ok_or(SymbolError::InconsistentLengths)?;
        if record_len != expected_record_len {
            return Err(SymbolError::InconsistentLengths);
        }
        let record_len_usize = usize::try_from(record_len).expect("record_len fits usize");
        if bytes.len() < record_len_usize {
            return Err(SymbolError::Truncated);
        }
        if bytes.len() != record_len_usize {
            return Err(SymbolError::InconsistentLengths);
        }

        let payload_end = cursor + usize::try_from(symbol_len).expect("symbol_len fits usize");
        let payload = bytes[cursor..payload_end].to_vec();
        let mut mac = [0u8; 16];
        mac.copy_from_slice(&bytes[payload_end..payload_end + usize::from(SYMBOL_MAC_LEN_V1)]);

        Ok((
            Self {
                format_version,
                logical_oid,
                ciphertext_id,
                encoding_id,
                object_kind,
                source_block,
                esi,
                transfer_length,
                oti_common,
                oti_scheme,
                flags,
                symbol_mac_profile,
                payload,
            },
            mac,
        ))
    }

    /// THE ONLY WAY TO OBTAIN A RECORD FROM BYTES. Requires the authenticated
    /// [`EncodedObject`] whose descriptor supplies the OTI and auth profile,
    /// binds the record to that exact encoding, and verifies the MAC over the
    /// complete transcript before returning anything.
    pub fn verify(
        bytes: &[u8],
        encoding: &EncodedObject,
        dek: &[u8; 32],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Self, SymbolError> {
        let result = (|| {
            let (record, claimed_mac) = Self::parse_framing(bytes)?;

            // Bind to the authenticated encoding BEFORE checking the MAC: a
            // record from a foreign encoding must be rejected as foreign, not as
            // an authentication failure, and symbols from different EncodingIds
            // must never mix in a decode.
            // ubs:ignore — public encoding identity, not authentication material.
            if record.encoding_id != encoding.encoding_id()
                // ubs:ignore — public ciphertext identity, not authentication material.
                || record.ciphertext_id != encoding.ciphertext_id()
                || record.logical_oid != encoding.object_id()
                || record.object_kind != encoding.cipher_descriptor().object_kind
            {
                return Err(SymbolError::ForeignEncoding);
            }
            let descriptor = encoding.descriptor();
            if record.transfer_length != descriptor.transfer_length
                || record.oti_common != descriptor.oti_common
                || record.oti_scheme != descriptor.oti_scheme
                // ubs:ignore — public descriptor profile, not a MAC or authentication tag.
                || record.symbol_mac_profile != descriptor.symbol_auth_profile
            {
                return Err(SymbolError::ForeignEncoding);
            }
            if record.source_block >= u32::from(descriptor.source_block_count) {
                return Err(SymbolError::InconsistentLengths);
            }
            // The encoder pads every source symbol to exactly `symbol_size`
            // (symbolize.rs source_symbols zero-fills), and asupersync's
            // validate_input requires the same equality wholesale: anything else
            // is damage this check exists to name, not a shape to admit. A
            // short-but-MAC-valid payload would otherwise enter here and fail the
            // whole decode as one erasure too many, contradicting the per-symbol
            // MAC's reason for existing.
            if record.payload.len() != usize::from(descriptor.symbol_size) {
                return Err(SymbolError::InconsistentLengths);
            }

            let k_symbol = encoding.symbol_auth_key(dek);
            let expected = record.compute_mac(&k_symbol);
            let mut diff = 0u8;
            for (a, b) in expected.iter().zip(claimed_mac.iter()) {
                diff |= a ^ b;
            }
            if diff != 0 {
                return Err(SymbolError::AuthenticationFailed);
            }
            Ok(record)
        })();

        let outcome = match result.as_ref() {
            Ok(_) => VerificationOutcome::Accepted,
            Err(SymbolError::Truncated) => {
                VerificationOutcome::Rejected(VerificationFailureClass::SymbolTruncated)
            }
            Err(SymbolError::UnsupportedFraming) => VerificationOutcome::Rejected(
                VerificationFailureClass::SymbolUnsupportedFraming,
            ),
            Err(SymbolError::InconsistentLengths) => VerificationOutcome::Rejected(
                VerificationFailureClass::SymbolInconsistentLengths,
            ),
            Err(SymbolError::ForeignEncoding) => {
                VerificationOutcome::Rejected(VerificationFailureClass::ForeignEncoding)
            }
            Err(SymbolError::AuthenticationFailed) => {
                VerificationOutcome::Rejected(VerificationFailureClass::Authentication)
            }
        };
        verification.record(verification_event(
            encoding.cipher_descriptor(),
            Some(encoding.encoding_id()),
            Some(bytes.len()),
            VerificationOperation::SymbolRecord,
            outcome,
        ));
        result
    }

    /// Build a record for one symbol of an encoding. All identity fields come
    /// from the encoding, so a caller cannot mislabel a symbol.
    pub fn for_encoding(
        encoding: &EncodedObject,
        source_block: u32,
        esi: u32,
        flags: u32,
        payload: Vec<u8>,
    ) -> Self {
        let descriptor = encoding.descriptor();
        Self {
            format_version: SYMBOL_FORMAT_VERSION,
            logical_oid: encoding.object_id(),
            ciphertext_id: encoding.ciphertext_id(),
            encoding_id: encoding.encoding_id(),
            object_kind: encoding.cipher_descriptor().object_kind,
            source_block,
            esi,
            transfer_length: descriptor.transfer_length,
            oti_common: descriptor.oti_common,
            oti_scheme: descriptor.oti_scheme,
            flags,
            symbol_mac_profile: descriptor.symbol_auth_profile,
            payload,
        }
    }
}
