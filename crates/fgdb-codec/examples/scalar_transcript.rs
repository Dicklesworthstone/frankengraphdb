use std::{error::Error, fmt::Write as _};

use fgdb_codec::{
    bitpack,
    block::{self, CodecProfile, OutputLimit},
    delta_varint::{self, EntryLimit as DeltaVarintEntryLimit},
    elias_fano::{EliasFano, EntryLimit},
    neighbor::EncodedNeighbors,
    roaring::{EntryLimit as RoaringEntryLimit, RoaringBitmap},
    varint,
};

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unexpected_success(message: &'static str) -> Box<dyn Error> {
    std::io::Error::other(message).into()
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== fgdb-codec scalar transcript v1 ==");

    let max_varint = varint::encode_u64(u64::MAX);
    println!("uleb128 max: {}", hex(max_varint.as_bytes()));
    let nonminimal = match varint::decode_u64(&[0x80, 0x00]) {
        Err(error) => error,
        Ok(_) => return Err(unexpected_success("nonminimal varint was accepted")),
    };
    println!("uleb128 reject nonminimal: {nonminimal}");

    let delta_values = [127, 127, 255, 16_384];
    let delta_encoded = delta_varint::encode(&delta_values)?;
    println!(
        "delta_varint count=4: bytes={} decoded={:?}",
        hex(&delta_encoded),
        delta_varint::decode(
            &delta_encoded,
            delta_values.len(),
            DeltaVarintEntryLimit::new(delta_values.len()),
        )?
    );
    let decreasing_delta = match delta_varint::encode(&[8, 5]) {
        Err(error) => error,
        Ok(_) => {
            return Err(unexpected_success(
                "decreasing delta-varint input was accepted",
            ));
        }
    };
    println!("delta_varint reject decreasing: {decreasing_delta}");

    let block_profile = CodecProfile::try_new(4096, 256, 4096)?;
    let block_input = b"abcdabcdabcd";
    let block_encoded = block::compress(block_input, block_profile)?;
    println!(
        "block input={} encoded={} bytes={} decoded={}",
        block_input.len(),
        block_encoded.len(),
        hex(&block_encoded),
        String::from_utf8(block::decompress(
            &block_encoded,
            block_input.len(),
            OutputLimit::new(block_input.len()),
        )?)?
    );

    let packed_values = [0, 1, 2, 3, 4, 5, 30, 31];
    let packed = bitpack::encode(&packed_values, 5)?;
    println!(
        "bitpack width=5 count=8: bytes={} decoded={:?}",
        hex(&packed),
        bitpack::decode(&packed, packed_values.len(), 5)?
    );
    let nonzero_padding = match bitpack::decode(&[0x20], 1, 5) {
        Err(error) => error,
        Ok(_) => return Err(unexpected_success("nonzero bitpack padding was accepted")),
    };
    println!("bitpack reject nonzero padding: {nonzero_padding}");

    let frame_values = [100, 101, 105, 109, 115];
    let frame = bitpack::encode_for(&frame_values, 100, 4)?;
    println!(
        "for base=100 width=4 count=5: bytes={} decoded={:?}",
        hex(&frame),
        bitpack::decode_for(&frame, frame_values.len(), 100, 4)?
    );

    let monotone = [0, 1, 1, 3, 5, 8, 13, 21, 34, 55];
    let ef = EliasFano::try_new(&monotone, EntryLimit::new(monotone.len()))?;
    println!(
        "elias_fano count={} low_bits={} high_bits={} logical_storage_words={}",
        ef.len(),
        ef.low_bits(),
        ef.high_bit_len(),
        ef.logical_storage_words()
    );
    let selected = ef
        .select(7)
        .ok_or_else(|| unexpected_success("Elias-Fano select lost an in-range value"))?;
    println!(
        "elias_fano rank_le(13)={} select(7)={selected}",
        ef.rank_le(13)
    );
    println!(
        "elias_fano predecessor(20)={:?} successor(20)={:?}",
        ef.predecessor(20),
        ef.successor(20)
    );
    let decreasing = match EliasFano::try_new(&[1, 4, 3], EntryLimit::new(3)) {
        Err(error) => error,
        Ok(_) => {
            return Err(unexpected_success(
                "decreasing Elias-Fano input was accepted",
            ));
        }
    };
    println!("elias_fano reject decreasing: {decreasing}");

    let bitmap_values = [1, 2, 3, 10, 65_536, u32::MAX];
    let bitmap = RoaringBitmap::try_from_sorted(
        &bitmap_values,
        RoaringEntryLimit::new(bitmap_values.len()),
    )?;
    let bitmap_other = RoaringBitmap::try_from_sorted(&[2, 10, 65_536], RoaringEntryLimit::new(3))?;
    println!(
        "roaring count={} chunks={} rank_le(10)={} select(4)={:?} intersection={:?}",
        bitmap.len(),
        bitmap.chunk_count(),
        bitmap.rank_le(10),
        bitmap.select(4),
        bitmap
            .intersection(&bitmap_other, RoaringEntryLimit::new(3))?
            .iter()
            .collect::<Vec<_>>()
    );

    let neighbors = [1, 2, 3, 10, 127, 128, 1_000];
    let stream = EncodedNeighbors::try_stream_vbyte(&neighbors, EntryLimit::new(neighbors.len()))?;
    let dense =
        EncodedNeighbors::try_dense_intervals(&[2, 3, 4, 10, 11, 1_000], EntryLimit::new(6))?;
    println!(
        "neighbor codec={:?} count={} rank_le(128)={} select(5)={:?} intersection={:?}",
        stream.codec(),
        stream.len(),
        stream.rank_le(128),
        stream.select(5),
        stream.intersection(&dense, EntryLimit::new(4))?
    );

    Ok(())
}
