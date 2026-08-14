//! Portable scalar BLAKE3: hash, keyed hash, key derivation, and XOF output.
//!
//! Implemented from the BLAKE3 specification (chunked Merkle tree over the
//! BLAKE2s-derived compression function; 1024-byte chunks of 64-byte blocks;
//! 7 rounds with the fixed message permutation; lazy chaining-value stack for
//! streaming tree hashing). Verified against golden vectors generated from the
//! official `blake3` crate by a dev-time oracle outside the workspace
//! (`tests/blake3_vectors.rs` records the provenance) — memory is not a hash
//! oracle, and neither is this comment.
//!
//! Scalar only, by doctrine: the workspace forbids `unsafe`, so the SIMD
//! variant belongs to the `fgdb-unsafe-simd` boundary crate and must be
//! bit-identical to this fallback before it may exist at all.
//!
//! Keyed and derive-key modes keep persistent key words, chaining values, and
//! buffered material in scrub-on-drop storage. The fixed chaining-value stack
//! avoids leaving moved values in inaccessible spare `Vec` capacity. As with
//! [`crate::zeroize`], this covers original safe-Rust storage, not compiler
//! register/spill copies or the caller-owned key/input/output buffers.

use crate::zeroize::{scrub_slice, scrub_words32};

/// The BLAKE3 initialization vector (the SHA-256 IV words).
const IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Per-round source indices into the 16-word message. Round r uses
/// `MSG_SCHEDULE[r]`, precomputed from r applications of the BLAKE3
/// permutation `[2,6,3,10,7,0,4,13,1,11,12,5,9,14,15,8]`.
const MSG_SCHEDULE: [[usize; 16]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

const BLOCK_LEN: usize = 64;
const CHUNK_LEN: usize = 1024;
const MAX_CV_STACK_DEPTH: usize = 64;

const CHUNK_START: u32 = 1 << 0;
const CHUNK_END: u32 = 1 << 1;
const PARENT: u32 = 1 << 2;
const ROOT: u32 = 1 << 3;
const KEYED_HASH: u32 = 1 << 4;
const DERIVE_KEY_CONTEXT: u32 = 1 << 5;
const DERIVE_KEY_MATERIAL: u32 = 1 << 6;

#[inline]
fn owns_secret_state(flags: u32) -> bool {
    flags & (KEYED_HASH | DERIVE_KEY_MATERIAL) != 0
}

#[inline(always)]
#[allow(clippy::many_single_char_names)]
fn g(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, mx: u32, my: u32) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(mx);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(my);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

#[inline(always)]
fn round(state: &mut [u32; 16], m: &[u32; 16], schedule: &[usize; 16]) {
    // Column step.
    g(state, 0, 4, 8, 12, m[schedule[0]], m[schedule[1]]);
    g(state, 1, 5, 9, 13, m[schedule[2]], m[schedule[3]]);
    g(state, 2, 6, 10, 14, m[schedule[4]], m[schedule[5]]);
    g(state, 3, 7, 11, 15, m[schedule[6]], m[schedule[7]]);
    // Diagonal step.
    g(state, 0, 5, 10, 15, m[schedule[8]], m[schedule[9]]);
    g(state, 1, 6, 11, 12, m[schedule[10]], m[schedule[11]]);
    g(state, 2, 7, 8, 13, m[schedule[12]], m[schedule[13]]);
    g(state, 3, 4, 9, 14, m[schedule[14]], m[schedule[15]]);
}

/// The compression function. Returns the full 16-word extended state; callers
/// take words 0..8 as the chaining value, and root/XOF output additionally
/// uses words 8..16.
fn compress(
    chaining_value: &[u32; 8],
    block_words: &[u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
) -> [u32; 16] {
    let mut state = [
        chaining_value[0],
        chaining_value[1],
        chaining_value[2],
        chaining_value[3],
        chaining_value[4],
        chaining_value[5],
        chaining_value[6],
        chaining_value[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block_len,
        flags,
    ];
    for schedule in &MSG_SCHEDULE {
        round(&mut state, block_words, schedule);
    }
    for i in 0..8 {
        state[i] ^= state[i + 8];
        state[i + 8] ^= chaining_value[i];
    }
    state
}

#[inline(always)]
fn words_from_block(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    let mut words = [0u32; 16];
    for (i, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    words
}

#[inline(always)]
fn first_8_words(compression_output: &[u32; 16]) -> [u32; 8] {
    let mut cv = [0u32; 8];
    cv.copy_from_slice(&compression_output[..8]);
    cv
}

fn words_from_key(key: &[u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    for (i, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    words
}

/// One chunk's streaming state.
struct ChunkState {
    chaining_value: [u32; 8],
    chunk_counter: u64,
    block: [u8; BLOCK_LEN],
    block_len: u8,
    blocks_compressed: u8,
    flags: u32,
}

impl ChunkState {
    fn new(key_words: &[u32; 8], chunk_counter: u64, flags: u32) -> Self {
        Self {
            chaining_value: *key_words,
            chunk_counter,
            block: [0; BLOCK_LEN],
            block_len: 0,
            blocks_compressed: 0,
            flags,
        }
    }

    fn len(&self) -> usize {
        BLOCK_LEN * usize::from(self.blocks_compressed) + usize::from(self.block_len)
    }

    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 {
            CHUNK_START
        } else {
            0
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            // A full buffered block is compressed only once MORE input
            // arrives: the final block of the chunk must keep CHUNK_END.
            if usize::from(self.block_len) == BLOCK_LEN {
                let mut block_words = words_from_block(&self.block);
                let mut compression_output = compress(
                    &self.chaining_value,
                    &block_words,
                    self.chunk_counter,
                    BLOCK_LEN as u32,
                    self.flags | self.start_flag(),
                );
                self.chaining_value = first_8_words(&compression_output);
                if owns_secret_state(self.flags) {
                    scrub_words32(&mut compression_output);
                    scrub_words32(&mut block_words);
                }
                self.blocks_compressed += 1;
                self.block = [0; BLOCK_LEN];
                self.block_len = 0;
            }
            let want = BLOCK_LEN - usize::from(self.block_len);
            let take = want.min(input.len());
            self.block[usize::from(self.block_len)..usize::from(self.block_len) + take]
                .copy_from_slice(&input[..take]);
            self.block_len += take as u8;
            input = &input[take..];
        }
    }

    fn output(&self) -> Output {
        Output {
            input_chaining_value: self.chaining_value,
            block_words: words_from_block(&self.block),
            counter: self.chunk_counter,
            block_len: u32::from(self.block_len),
            flags: self.flags | self.start_flag() | CHUNK_END,
        }
    }
}

impl Drop for ChunkState {
    fn drop(&mut self) {
        if owns_secret_state(self.flags) {
            scrub_words32(&mut self.chaining_value);
            scrub_slice(&mut self.block);
        }
    }
}

/// A finalization-pending node: everything `compress` needs except the ROOT
/// flag and the output counter, so one node can produce unbounded XOF bytes.
struct Output {
    input_chaining_value: [u32; 8],
    block_words: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Output {
    fn chaining_value(&self) -> [u32; 8] {
        let mut compression_output = compress(
            &self.input_chaining_value,
            &self.block_words,
            self.counter,
            self.block_len,
            self.flags,
        );
        let chaining_value = first_8_words(&compression_output);
        if owns_secret_state(self.flags) {
            scrub_words32(&mut compression_output);
        }
        chaining_value
    }

    fn root_output_bytes(&self, out: &mut [u8]) {
        for (block_counter, out_block) in out.chunks_mut(2 * BLOCK_LEN / 2).enumerate() {
            let mut words = compress(
                &self.input_chaining_value,
                &self.block_words,
                block_counter as u64,
                self.block_len,
                self.flags | ROOT,
            );
            for (word, out_word) in words.iter().zip(out_block.chunks_mut(4)) {
                out_word.copy_from_slice(&word.to_le_bytes()[..out_word.len()]);
            }
            if owns_secret_state(self.flags) {
                scrub_words32(&mut words);
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if owns_secret_state(self.flags) {
            scrub_words32(&mut self.input_chaining_value);
            scrub_words32(&mut self.block_words);
        }
    }
}

fn parent_output(
    left_child: &[u32; 8],
    right_child: &[u32; 8],
    key_words: &[u32; 8],
    flags: u32,
) -> Output {
    let mut output = Output {
        input_chaining_value: *key_words,
        block_words: [0; 16],
        counter: 0,
        block_len: BLOCK_LEN as u32,
        flags: PARENT | flags,
    };
    output.block_words[..8].copy_from_slice(left_child);
    output.block_words[8..].copy_from_slice(right_child);
    output
}

fn parent_cv(
    left_child: &[u32; 8],
    right_child: &[u32; 8],
    key_words: &[u32; 8],
    flags: u32,
) -> [u32; 8] {
    parent_output(left_child, right_child, key_words, flags).chaining_value()
}

/// Streaming BLAKE3 hasher (plain, keyed, or derive-key mode).
///
/// The same public type can own a keyed-hash or derive-key state, so it is
/// deliberately non-`Clone` even when a particular value is in plain mode:
///
/// ```compile_fail
/// use fgdb_crypto::blake3::Hasher;
///
/// let state = Hasher::new_keyed(&[0_u8; 32]);
/// let _copy = state.clone();
/// ```
pub struct Hasher {
    chunk_state: ChunkState,
    key_words: [u32; 8],
    /// Chaining values of completed subtrees, lowest level last-pushed.
    cv_stack: [[u32; 8]; MAX_CV_STACK_DEPTH],
    cv_stack_len: usize,
    flags: u32,
}

impl Hasher {
    fn new_internal(key_words: &[u32; 8], flags: u32) -> Self {
        Self {
            chunk_state: ChunkState::new(key_words, 0, flags),
            key_words: *key_words,
            cv_stack: [[0; 8]; MAX_CV_STACK_DEPTH],
            cv_stack_len: 0,
            flags,
        }
    }

    /// Plain hash mode.
    pub fn new() -> Self {
        Self::new_internal(&IV, 0)
    }

    /// Keyed hash mode with a 256-bit key.
    pub fn new_keyed(key: &[u8; 32]) -> Self {
        let mut key_words = words_from_key(key);
        let hasher = Self::new_internal(&key_words, KEYED_HASH);
        scrub_words32(&mut key_words);
        hasher
    }

    /// Key-derivation mode: `context` should be a hardcoded, globally unique,
    /// application-specific string (the fgdb domain-separation strings).
    pub fn new_derive_key(context: &str) -> Self {
        let mut context_hasher = Self::new_internal(&IV, DERIVE_KEY_CONTEXT);
        context_hasher.update(context.as_bytes());
        let mut context_key = context_hasher.finalize();
        let mut key_words = words_from_key(&context_key.0);
        let hasher = Self::new_internal(&key_words, DERIVE_KEY_MATERIAL);
        scrub_words32(&mut key_words);
        scrub_slice(&mut context_key.0);
        hasher
    }

    /// After popping `total_chunks.trailing_zeros()` levels, completed
    /// subtrees merge; the rule keeps the stack equal to the binary
    /// representation of the completed-chunk count.
    fn add_chunk_chaining_value(&mut self, mut new_cv: [u32; 8], mut total_chunks: u64) {
        while total_chunks & 1 == 0 {
            self.cv_stack_len = self
                .cv_stack_len
                .checked_sub(1)
                .expect("cv stack tracks the binary representation of the chunk count");
            let mut left = self.cv_stack[self.cv_stack_len];
            if owns_secret_state(self.flags) {
                scrub_words32(&mut self.cv_stack[self.cv_stack_len]);
            }
            let mut right = new_cv;
            new_cv = parent_cv(&left, &right, &self.key_words, self.flags);
            if owns_secret_state(self.flags) {
                scrub_words32(&mut left);
                scrub_words32(&mut right);
            }
            total_chunks >>= 1;
        }
        assert!(
            self.cv_stack_len < MAX_CV_STACK_DEPTH,
            "a usize-sized input cannot exceed the fixed BLAKE3 tree depth"
        );
        self.cv_stack[self.cv_stack_len] = new_cv;
        self.cv_stack_len += 1;
        if owns_secret_state(self.flags) {
            scrub_words32(&mut new_cv);
        }
    }

    /// Absorb input bytes. Chainable with further `update` calls.
    pub fn update(&mut self, mut input: &[u8]) -> &mut Self {
        while !input.is_empty() {
            // A full chunk is finalized only once more input arrives: the
            // final chunk of the whole message must become the root.
            if self.chunk_state.len() == CHUNK_LEN {
                let mut chunk_cv = self.chunk_state.output().chaining_value();
                let total_chunks = self.chunk_state.chunk_counter + 1;
                self.add_chunk_chaining_value(chunk_cv, total_chunks);
                if owns_secret_state(self.flags) {
                    scrub_words32(&mut chunk_cv);
                }
                self.chunk_state = ChunkState::new(&self.key_words, total_chunks, self.flags);
            }
            let want = CHUNK_LEN - self.chunk_state.len();
            let take = want.min(input.len());
            self.chunk_state.update(&input[..take]);
            input = &input[take..];
        }
        self
    }

    fn final_output(&self) -> Output {
        let mut output = self.chunk_state.output();
        for left in self.cv_stack[..self.cv_stack_len].iter().rev() {
            let mut right = output.chaining_value();
            output = parent_output(left, &right, &self.key_words, self.flags);
            if owns_secret_state(self.flags) {
                scrub_words32(&mut right);
            }
        }
        output
    }

    /// The standard 256-bit digest.
    pub fn finalize(&self) -> Digest {
        let mut bytes = [0u8; 32];
        self.final_output().root_output_bytes(&mut bytes);
        Digest(bytes)
    }

    /// Extended output: fill `out` with XOF bytes (prefix-consistent with
    /// `finalize`, unbounded length).
    pub fn finalize_xof(&self, out: &mut [u8]) {
        self.final_output().root_output_bytes(out);
    }
}

impl Drop for Hasher {
    fn drop(&mut self) {
        if owns_secret_state(self.flags) {
            scrub_words32(&mut self.key_words);
            for chaining_value in &mut self.cv_stack {
                scrub_words32(chaining_value);
            }
        }
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// A 256-bit BLAKE3 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble < 16"));
            s.push(char::from_digit(u32::from(b & 0xf), 16).expect("nibble < 16"));
        }
        s
    }
}

impl core::fmt::Debug for Digest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

/// One-shot plain hash.
pub fn hash(input: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(input);
    hasher.finalize()
}

/// One-shot keyed hash (the primitive under `ObjectId = BLAKE3_keyed(K_oid, …)`).
pub fn keyed_hash(key: &[u8; 32], input: &[u8]) -> Digest {
    let mut hasher = Hasher::new_keyed(key);
    hasher.update(input);
    hasher.finalize()
}

/// One-shot key derivation: 32 bytes of key material from a context string
/// and input key material.
pub fn derive_key(context: &str, key_material: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new_derive_key(context);
    hasher.update(key_material);
    hasher.finalize().0
}
