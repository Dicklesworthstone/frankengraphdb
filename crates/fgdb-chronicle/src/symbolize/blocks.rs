//! RFC 6330 §4.4.1.2 object partitioning around the foundation block codec.
//!
//! Multi-block encodings must carry canonical RFC common/scheme OTI and agree
//! with the duplicated descriptor fields. Existing one-block encodings retain
//! their established contiguous-symbol interpretation; their opaque OTI bytes
//! are not reinterpreted as a new interleaving profile.

use super::{MAX_SOURCE_SYMBOLS_PER_BLOCK, SymbolizeError, code_seed};
use crate::identity::{CryptoVerificationSink, EncodedObject};
use crate::symbol::{HEADER_LEN_V1, SYMBOL_MAC_LEN_V1, SymbolError, SymbolRecord};
use asupersync::raptorq::decoder::{DecodeError, InactivationDecoder, ReceivedSymbol};
use asupersync::raptorq::systematic::SystematicEncoder;
use std::collections::BTreeMap;

pub(crate) const MAX_SOURCE_BLOCKS: usize = 255;
const MAX_ESI: u32 = 0x00ff_ffff;

/// Checked arithmetic/partition view, not a new durable descriptor or codec.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    bytes: usize,
    symbol_size: usize,
    blocks: usize,
    total_symbols: usize,
    sub_blocks: usize,
    alignment: usize,
}

impl Layout {
    pub(crate) fn new(encoding: &EncodedObject, bytes: usize) -> Result<Self, SymbolizeError> {
        let descriptor = encoding.descriptor();
        let symbol_size = usize::from(descriptor.symbol_size);
        let blocks = usize::from(descriptor.source_block_count);
        if bytes == 0 || symbol_size == 0 || blocks == 0 || blocks > MAX_SOURCE_BLOCKS
            || u64::try_from(bytes).ok() != Some(descriptor.transfer_length)
            || encoding.cipher_descriptor().compressed_len.checked_add(
                u64::from(encoding.cipher_descriptor().object_tag_len),
            ) != Some(descriptor.transfer_length)
        {
            return Err(SymbolizeError::InvalidParameters);
        }
        let total_symbols = bytes.div_ceil(symbol_size);
        if blocks > total_symbols
            || total_symbols.div_ceil(blocks) > MAX_SOURCE_SYMBOLS_PER_BLOCK
            || total_symbols.checked_mul(symbol_size).is_none()
        {
            return Err(SymbolizeError::InvalidParameters);
        }
        let (sub_blocks, alignment) = if blocks == 1 {
            (1, 1)
        } else {
            // Common OTI is F:40 | reserved:8 | T:16; scheme OTI is Z:8 | N:16 | Al:8.
            let common = descriptor.oti_common;
            let scheme = descriptor.oti_scheme;
            let n = ((scheme >> 8) & 0xffff) as usize;
            let al = (scheme & 0xff) as usize;
            if common >> 24 != descriptor.transfer_length
                || (common >> 16) & 0xff != 0
                || common & 0xffff != u64::from(descriptor.symbol_size)
                || (scheme >> 24) as usize != blocks
                || al == 0 || symbol_size % al != 0
                || n == 0 || n > symbol_size / al
            {
                return Err(SymbolizeError::InvalidParameters);
            }
            (n, al)
        };
        Ok(Self { bytes, symbol_size, blocks, total_symbols, sub_blocks, alignment })
    }

    pub(crate) fn blocks(self) -> usize { self.blocks }

    pub(crate) fn source_symbols(self, block: u32) -> Option<usize> {
        let block = usize::try_from(block).ok()?;
        (block < self.blocks).then(|| {
            self.total_symbols / self.blocks + usize::from(block < self.total_symbols % self.blocks)
        })
    }

    /// Copy one systematic symbol without materializing its siblings. The
    /// batch encoder and request-driven donor share this byte mapping.
    pub(crate) fn copy_source_symbol(
        self,
        protected: &[u8],
        block: u32,
        esi: u32,
        output: &mut [u8],
    ) -> Result<(), SymbolizeError> {
        self.block(block)?.copy_source_symbol(protected, esi as usize, output)
    }

    fn block(self, number: u32) -> Result<Block, SymbolizeError> {
        let symbols = self.source_symbols(number).ok_or(SymbolizeError::InvalidParameters)?;
        let number = number as usize;
        let first = number * (self.total_symbols / self.blocks)
            + number.min(self.total_symbols % self.blocks);
        let start = first * self.symbol_size;
        let end = (start + symbols * self.symbol_size).min(self.bytes);
        Ok(Block { layout: self, symbols, start, end })
    }
}

#[derive(Clone, Copy)]
struct Block {
    layout: Layout,
    symbols: usize,
    start: usize,
    end: usize,
}

impl Block {
    /// Offset/length of sub-symbol n within an encoding symbol. Multiplying
    /// its offset by K locates the contiguous sub-block in the source bytes.
    fn sub_symbol(self, n: usize) -> (usize, usize) {
        let units = self.layout.symbol_size / self.layout.alignment;
        let small = units / self.layout.sub_blocks;
        let large = units % self.layout.sub_blocks;
        (
            (n * small + n.min(large)) * self.layout.alignment,
            (small + usize::from(n < large)) * self.layout.alignment,
        )
    }

    fn copy_source_symbol(
        self,
        protected: &[u8],
        esi: usize,
        symbol: &mut [u8],
    ) -> Result<(), SymbolizeError> {
        if esi >= self.symbols || symbol.len() != self.layout.symbol_size {
            return Err(SymbolizeError::InvalidParameters);
        }
        let source_bytes = protected.get(self.start..self.end)
            .ok_or(SymbolizeError::InvalidParameters)?;
        symbol.fill(0);
        for n in 0..self.layout.sub_blocks {
            let (offset, width) = self.sub_symbol(n);
            let begin = offset * self.symbols + esi * width;
            if begin < source_bytes.len() {
                let length = width.min(source_bytes.len() - begin);
                symbol[offset..offset + length].copy_from_slice(&source_bytes[begin..begin + length]);
            }
        }
        Ok(())
    }

    fn materialize(self, protected: &[u8]) -> Result<Vec<Vec<u8>>, SymbolizeError> {
        let mut source = Vec::new();
        source.try_reserve_exact(self.symbols).map_err(|_| SymbolizeError::AllocationFailed)?;
        for esi in 0..self.symbols {
            let mut symbol = Vec::new();
            symbol.try_reserve_exact(self.layout.symbol_size)
                .map_err(|_| SymbolizeError::AllocationFailed)?;
            symbol.resize(self.layout.symbol_size, 0);
            self.copy_source_symbol(protected, esi, &mut symbol)?;
            source.push(symbol);
        }
        Ok(source)
    }

    fn restore(self, source: &[Vec<u8>], protected: &mut [u8]) -> Result<(), SymbolizeError> {
        if source.len() < self.symbols
            || source.iter().take(self.symbols).any(|symbol| symbol.len() != self.layout.symbol_size)
        {
            return Err(SymbolizeError::DecodeFailed);
        }
        for (esi, symbol) in source.iter().take(self.symbols).enumerate() {
            self.restore_symbol(esi, symbol, protected)?;
        }
        Ok(())
    }

    /// Restore either a verified original symbol or one recovered by the native
    /// decoder. Both paths enforce the same sub-block ordering and zero padding.
    fn restore_symbol(
        self,
        esi: usize,
        symbol: &[u8],
        protected: &mut [u8],
    ) -> Result<(), SymbolizeError> {
        if esi >= self.symbols || symbol.len() != self.layout.symbol_size {
            return Err(SymbolizeError::DecodeFailed);
        }
        let target = protected.get_mut(self.start..self.end).ok_or(SymbolizeError::DecodeFailed)?;
        for n in 0..self.layout.sub_blocks {
            let (offset, width) = self.sub_symbol(n);
            let begin = offset * self.symbols + esi * width;
            let length = width.min(target.len().saturating_sub(begin));
            if length != 0 {
                target[begin..begin + length].copy_from_slice(&symbol[offset..offset + length]);
            }
            // Padding is part of the encoded source block, not arbitrary data
            // to discard merely because the object AEAD does not cover it.
            if symbol[offset + length..offset + width].iter().any(|byte| *byte != 0) {
                return Err(SymbolizeError::DecodeFailed);
            }
        }
        Ok(())
    }
}

/// Keep request-driven and batch repair generation on the same foundation
/// seed/parameter path. The donor admits solver resources before calling this.
pub(crate) fn repair_encoder(
    encoding: &EncodedObject,
    source: &[Vec<u8>],
) -> Result<SystematicEncoder, SymbolizeError> {
    let size = usize::from(encoding.descriptor().symbol_size);
    if source.is_empty() || source.len() > MAX_SOURCE_SYMBOLS_PER_BLOCK
        || size == 0 || source.iter().any(|symbol| symbol.len() != size)
    {
        return Err(SymbolizeError::InvalidParameters);
    }
    SystematicEncoder::new(source, size, code_seed(encoding))
        .ok_or(SymbolizeError::EncoderUnavailable)
}

pub(super) fn encode_block(
    encoding: &EncodedObject,
    protected: &[u8],
    number: u32,
    repair_symbols: u32,
    dek: &[u8; 32],
) -> Result<Vec<Vec<u8>>, SymbolizeError> {
    let block = Layout::new(encoding, protected.len())?.block(number)?;
    let k = block.symbols as u32;
    let count = k.checked_add(repair_symbols).filter(|count| *count <= MAX_ESI + 1)
        .ok_or(SymbolizeError::InvalidParameters)?;
    // Validate ESI and partition limits before materializing symbols or invoking
    // the foundation encoder, whose systematic table has a finite K ceiling.
    let source = block.materialize(protected)?;
    let encoder = if repair_symbols == 0 { None } else {
        Some(repair_encoder(encoding, &source)?)
    };
    let mut records = Vec::new();
    records.try_reserve_exact(count as usize).map_err(|_| SymbolizeError::AllocationFailed)?;
    let key = encoding.symbol_auth_key(dek);
    for (esi, symbol) in source.iter().enumerate() {
        records.push(SymbolRecord::for_encoding(encoding, number, esi as u32, 0, symbol.clone()).serialize(&key));
    }
    if let Some(encoder) = encoder {
        for esi in k..count {
            let symbol = encoder.try_repair_symbol(esi).map_err(|_| SymbolizeError::InvalidParameters)?;
            records.push(SymbolRecord::for_encoding(encoding, number, esi, 0, symbol).serialize(&key));
        }
    }
    Ok(records)
}

/// Reconstruct protected bytes only. The caller must still perform the whole
/// object AEAD, CiphertextId and namespace-bound keyed ObjectId checks.
pub(super) fn decode_protected(
    encoding: &EncodedObject,
    serialized: &[Vec<u8>],
    bytes: usize,
    dek: &[u8; 32],
    verification: &mut dyn CryptoVerificationSink,
) -> Result<Vec<u8>, SymbolizeError> {
    decode_protected_observed(encoding, serialized, bytes, dek, verification, |_| {})
}

// The test observer is called at the actual native-decoder construction site,
// not on a predicted path. Production monomorphizes the no-op callback above.
fn decode_protected_observed(
    encoding: &EncodedObject,
    serialized: &[Vec<u8>],
    bytes: usize,
    dek: &[u8; 32],
    verification: &mut dyn CryptoVerificationSink,
    mut before_erasure_decode: impl FnMut(u32),
) -> Result<Vec<u8>, SymbolizeError> {
    let layout = Layout::new(encoding, bytes)?;
    let mut groups = Vec::new();
    groups.try_reserve_exact(layout.blocks()).map_err(|_| SymbolizeError::AllocationFailed)?;
    for _ in 0..layout.blocks() {
        groups.push(BTreeMap::<u32, (usize, Vec<u8>)>::new());
    }
    let record_len = usize::from(HEADER_LEN_V1) + layout.symbol_size + usize::from(SYMBOL_MAC_LEN_V1);
    // Authenticate ALL supplied records before decoding ANY block, even when
    // another block is missing. Duplicate equations do not increase rank.
    for (index, raw) in serialized.iter().enumerate() {
        if raw.len() != record_len {
            return Err(SymbolizeError::Symbol(SymbolError::InconsistentLengths));
        }
        let record = SymbolRecord::verify(raw, encoding, dek, verification)?;
        if record.esi > MAX_ESI {
            return Err(SymbolizeError::InvalidParameters);
        }
        let group = groups.get_mut(record.source_block as usize)
            .ok_or(SymbolizeError::InvalidParameters)?;
        if let Some((previous, _)) = group.get(&record.esi) {
            if serialized[*previous] != *raw {
                return Err(SymbolizeError::DecodeFailed);
            }
        } else {
            group.insert(record.esi, (index, record.payload));
        }
    }
    for (number, group) in groups.iter().enumerate() {
        if group.len() < layout.block(number as u32)?.symbols {
            return Err(SymbolizeError::InsufficientSymbols);
        }
    }
    let mut protected = Vec::new();
    protected.try_reserve_exact(bytes).map_err(|_| SymbolizeError::AllocationFailed)?;
    protected.resize(bytes, 0);
    for (number, group) in groups.into_iter().enumerate() {
        let block = layout.block(number as u32)?;
        restore_group(encoding, block, group, &mut protected, || {
            before_erasure_decode(number as u32);
        })?;
    }
    Ok(protected)
}

/// Decode exactly one block selected by BondedPull's immutable, authenticated
/// coordinate index. The index is private to the pull; it is not donor metadata.
/// Each selected record is reauthenticated and byte-bound to its indexed key.
/// No partial protected bytes are exposed outside Chronicle.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn recover_indexed_block(
    encoding: &EncodedObject,
    number: u32,
    serialized: &[Vec<u8>],
    coordinates: &BTreeMap<(u32, u32), usize>,
    expected_count: usize,
    protected: &mut [u8],
    dek: &[u8; 32],
    verification: &mut dyn CryptoVerificationSink,
) -> Result<(), SymbolizeError> {
    let layout = Layout::new(encoding, protected.len())?;
    let block = layout.block(number)?;
    let record_len = usize::from(HEADER_LEN_V1) + layout.symbol_size + usize::from(SYMBOL_MAC_LEN_V1);
    let mut group = BTreeMap::new();
    for (&(source_block, esi), &index) in coordinates.range((number, 0)..=(number, MAX_ESI)) {
        let raw = serialized.get(index).ok_or(SymbolizeError::InvalidParameters)?;
        if raw.len() != record_len {
            return Err(SymbolizeError::Symbol(SymbolError::InconsistentLengths));
        }
        let record = SymbolRecord::verify(raw, encoding, dek, verification)?;
        if record.source_block != source_block || record.esi != esi {
            return Err(SymbolizeError::InvalidParameters);
        }
        group.insert(esi, (index, record.payload));
    }
    if group.len() != expected_count || group.len() < block.symbols {
        return Err(SymbolizeError::InvalidParameters);
    }
    restore_group(encoding, block, group, protected, || {})
}

// The batch and resumable paths share the actual source/erasure decoder, not
// just its parameter calculations. Keep extra repair equations on this path.
fn restore_group(
    encoding: &EncodedObject,
    block: Block,
    group: BTreeMap<u32, (usize, Vec<u8>)>,
    protected: &mut [u8],
    before_erasure_decode: impl FnOnce(),
) -> Result<(), SymbolizeError> {
    if complete_systematic(&group, block.symbols) {
        // A systematic code transmits original symbols unchanged. All MACs,
        // duplicate conflicts and block identities were checked above. Move
        // each original directly into the protected object, without building
        // constraint equations, a decoding matrix or a second source vector.
        // Extra repair equations retain the existing native validation path.
        for (esi, (_, payload)) in group {
            block.restore_symbol(esi as usize, &payload, protected)?;
        }
        return Ok(());
    }
    before_erasure_decode();
    let decoder = InactivationDecoder::try_new(block.symbols, block.layout.symbol_size, code_seed(encoding))
        .map_err(|_| SymbolizeError::InvalidParameters)?;
    let mut received = decoder.constraint_symbols();
    received.try_reserve_exact(group.len()).map_err(|_| SymbolizeError::AllocationFailed)?;
    for (esi, (_, payload)) in group {
        if (esi as usize) < block.symbols {
            received.push(ReceivedSymbol::source(esi, payload));
        } else {
            let (columns, coefficients) = decoder.repair_equation(esi)
                .map_err(|_| SymbolizeError::InvalidParameters)?;
            received.push(ReceivedSymbol::repair(esi, columns, coefficients, payload));
        }
    }
    // ubs:ignore -- foundation erasure decoder, not JWT/signature decoding.
    let decoded = decoder.decode(&received).map_err(|error| match error {
        DecodeError::InsufficientSymbols { .. } | DecodeError::SingularMatrix { .. } =>
            SymbolizeError::InsufficientSymbols,
        _ => SymbolizeError::DecodeFailed,
    })?;
    block.restore(&decoded.source, protected)
}

fn complete_systematic(group: &BTreeMap<u32, (usize, Vec<u8>)>, sources: usize) -> bool {
    group.len() == sources
        && group.keys().enumerate().all(|(index, esi)| *esi as usize == index)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod source_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_and_subblock_transpose_match_independent_byte_walk() {
        // This tests partitioning independently of the encoder/decoder pair:
        // enumerate source bytes in RFC sub-block / ESI / sub-symbol order.
        for size in [4, 8, 12, 16] {
            for alignment in [1, 2, 4] {
                for sub_blocks in 1..=size / alignment {
                    for symbols in 2..24 {
                        for blocks in 2..=symbols.min(5) {
                            let bytes = symbols * size - 3;
                            let layout = Layout { bytes, symbol_size: size, blocks,
                                total_symbols: symbols, sub_blocks, alignment };
                            let input: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
                            let mut restored = vec![0; bytes];
                            let mut consumed = 0;
                            for number in 0..blocks {
                                let block = layout.block(number as u32).unwrap();
                                assert_eq!(block.start, consumed);
                                let source = block.materialize(&input).unwrap();
                                let units = size / alignment;
                                let mut cursor = block.start;
                                let mut symbol_offset = 0;
                                for sub in 0..sub_blocks {
                                    let width = (units / sub_blocks + usize::from(sub < units % sub_blocks)) * alignment;
                                    for symbol in &source {
                                        for value in &symbol[symbol_offset..symbol_offset + width] {
                                            assert_eq!(*value, input.get(cursor).copied().unwrap_or(0));
                                            cursor += 1;
                                        }
                                    }
                                    symbol_offset += width;
                                }
                                block.restore(&source, &mut restored).unwrap();
                                consumed = block.end;
                            }
                            assert_eq!(consumed, bytes);
                            assert_eq!(restored, input);
                        }
                    }
                }
            }
        }
    }
}
