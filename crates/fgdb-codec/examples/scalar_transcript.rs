use std::{error::Error, fmt::Write as _};

use fgdb_codec::{
    bitpack,
    elias_fano::{EliasFano, EntryLimit},
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
        "elias_fano count={} low_bits={} high_bits={} storage_words={}",
        ef.len(),
        ef.low_bits(),
        ef.high_bit_len(),
        ef.storage_words()
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

    Ok(())
}
