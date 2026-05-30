//! ZX0 decompressor (v2 format).
//!
//! A `no_std`, allocation-free port of Einar Saukas' reference `DZX0 v2.2`
//! decompressor (<https://github.com/einar-saukas/ZX0>). Only the decompression
//! algorithm is provided — no file I/O.
//!
//! ZX0 emits back-references that copy bytes from earlier in the decompressed
//! output, so the decoder needs random read access to recent output. Instead of
//! buffering the entire output, this implementation uses a caller-provided
//! sliding window that doubles as a flush buffer: as the window fills it is
//! handed to an output callback in chunks (the equivalent of the reference
//! `save_output` function), then reused. Because ZX0's maximum back-reference
//! distance is [`MAX_OFFSET`], a window of [`MIN_WINDOW_LEN`] bytes is enough to
//! keep every live back-reference reachable.
//!
//! ```ignore
//! static ZX0_WINDOW: ConstStaticCell<[u8; 0x8000]> = ConstStaticCell::new([0; 0x8000]);
//! let window = ZX0_WINDOW.take();
//! zx0_decompress::decompress(compressed, window, |chunk| flash_write(chunk))?;
//! ```
//!
//! # Features
//!
//! - `defmt` — derive [`defmt::Format`] on [`Zx0Error`]. Enabled by the firmware
//!   that links the `defmt` runtime; leave it off for host builds (e.g. tests).
//!
//! # Testing
//!
//! The crate is `no_std`, but its tests run on the host. Because this workspace
//! defaults to an embedded target, run them with an explicit host target, e.g.:
//!
//! ```text
//! cargo test -p zx0-decompress --target x86_64-unknown-linux-gnu
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::convert::Infallible;

/// Maximum back-reference distance the ZX0 v2 format can encode.
///
/// The offset is `msb * 128 - (lsb >> 1)` with `msb` in `1..=255` and the low
/// 7 bits in `0..=127`, so the largest representable offset is `255 * 128 = 32640`.
pub const MAX_OFFSET: usize = 32640;

/// Minimum length of the window slice passed to [`decompress`].
///
/// The window must be strictly larger than [`MAX_OFFSET`] so that a byte a live
/// back-reference still needs is never overwritten before it is read. `0x8000`
/// (32768) is a convenient size that satisfies this.
pub const MIN_WINDOW_LEN: usize = MAX_OFFSET + 1;

/// The ZX0 end-of-stream marker, encoded as the offset MSB value `256`.
const END_MARKER: u32 = 256;

/// An error encountered while decompressing a ZX0 stream.
///
/// `E` is the error type produced by the output callback; a callback failure is
/// reported as [`Zx0Error::CallbackError`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Zx0Error<E> {
    /// The decoder read past the end of the input (empty or truncated stream).
    TruncatedInput,
    /// A back-reference pointed further back than the data produced so far, or
    /// further than [`MAX_OFFSET`]. Also raised for an offset MSB outside the
    /// valid range, which only happens on corrupt input.
    InvalidOffset,
    /// The window slice was shorter than [`MIN_WINDOW_LEN`].
    WindowTooSmall,
    /// [`decompress_into`] was given a buffer too small to hold the whole
    /// decompressed output. (Distinct from [`Zx0Error::WindowTooSmall`], which is
    /// about the back-reference window minimum, not the total output size.)
    OutputTooLarge,
    /// The stream ended cleanly but input bytes remained afterwards.
    TrailingData,
    /// The output callback returned an error; decompression was aborted.
    CallbackError(E),
}

/// MSB-first bit reader over the compressed input.
///
/// Most of the stream is read bit-by-bit, MSB first, refilling a byte at a time.
/// The one exception is the offset LSB of a "copy from new offset" block: it is
/// a raw byte whose top 7 bits are the LSB value and whose bit 0 is fed back
/// into the bitstream as the next bit. [`BitReader::read_offset_lsb`] stashes
/// that bit in `pending_bit` and the next [`BitReader::read_bit`] pops it, so
/// the decode loop never has to reason about it.
struct BitReader<'a> {
    input: &'a [u8],
    /// Index of the next unread input byte.
    pos: usize,
    /// Mask selecting the current bit within `bit_value`; `0` means "refill".
    bit_mask: u8,
    /// The byte currently being read bit-by-bit.
    bit_value: u8,
    /// A bit split off from an offset LSB byte that the next [`BitReader::read_bit`]
    /// must return before reading from the bitstream again.
    pending_bit: Option<u8>,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            bit_mask: 0,
            bit_value: 0,
            pending_bit: None,
        }
    }

    /// Reads the next whole input byte.
    fn read_byte<E>(&mut self) -> Result<u8, Zx0Error<E>> {
        let byte = *self.input.get(self.pos).ok_or(Zx0Error::TruncatedInput)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads a single bit, MSB first.
    fn read_bit<E>(&mut self) -> Result<u8, Zx0Error<E>> {
        if let Some(bit) = self.pending_bit.take() {
            return Ok(bit);
        }
        if self.bit_mask == 0 {
            self.bit_value = self.read_byte()?;
            self.bit_mask = 0x80;
        }
        let bit = u8::from(self.bit_value & self.bit_mask != 0);
        self.bit_mask >>= 1;
        Ok(bit)
    }

    /// Reads an interlaced Elias gamma value.
    ///
    /// When `inverted` is set, every data bit is flipped (ZX0 stores the offset
    /// MSB with inverted bits in the v2 format).
    fn read_elias_gamma<E>(&mut self, inverted: bool) -> Result<u32, Zx0Error<E>> {
        let inv = u8::from(inverted);
        let mut value: u32 = 1;
        while self.read_bit()? == 0 {
            // A corrupt stream could keep this loop going indefinitely; bail out
            // before the shift would overflow rather than panic.
            if value & 0x8000_0000 != 0 {
                return Err(Zx0Error::InvalidOffset);
            }
            value = (value << 1) | u32::from(self.read_bit()? ^ inv);
        }
        Ok(value)
    }

    /// Reads the offset LSB byte of a "copy from new offset" block.
    ///
    /// The top 7 bits are returned as the LSB value (`0..=127`); bit 0 is stashed
    /// as the pending bit for the next [`BitReader::read_bit`].
    fn read_offset_lsb<E>(&mut self) -> Result<usize, Zx0Error<E>> {
        let byte = self.read_byte()?;
        self.pending_bit = Some(byte & 1);
        Ok(usize::from(byte >> 1))
    }

    /// Returns `true` if whole input bytes remain unread.
    fn has_trailing_data(&self) -> bool {
        self.pos < self.input.len()
    }
}

/// Sliding window over already-decompressed output that flushes through a callback.
///
/// Bytes are appended at `write_idx`; once the window fills it is flushed in full
/// and `write_idx` wraps to `0`, reusing the storage. Flushing does not clear the
/// bytes, so back-references keep reading them via modular indexing — valid as
/// long as every offset is strictly less than the window length, which
/// [`MIN_WINDOW_LEN`] guarantees.
struct Window<'a, E, F: FnMut(&[u8]) -> Result<(), E>> {
    buf: &'a mut [u8],
    /// Next write position, in `0..buf.len()`.
    write_idx: usize,
    /// Total number of bytes produced so far (used to validate offsets).
    produced: usize,
    output: F,
}

impl<'a, E, F: FnMut(&[u8]) -> Result<(), E>> Window<'a, E, F> {
    fn new(buf: &'a mut [u8], output: F) -> Result<Self, Zx0Error<E>> {
        if buf.len() < MIN_WINDOW_LEN {
            return Err(Zx0Error::WindowTooSmall);
        }
        Ok(Self {
            buf,
            write_idx: 0,
            produced: 0,
            output,
        })
    }

    /// Appends one byte, flushing the full window through the callback when it fills.
    fn write_byte(&mut self, byte: u8) -> Result<(), Zx0Error<E>> {
        self.buf[self.write_idx] = byte;
        self.write_idx += 1;
        self.produced += 1;
        if self.write_idx == self.buf.len() {
            (self.output)(self.buf).map_err(Zx0Error::CallbackError)?;
            self.write_idx = 0;
        }
        Ok(())
    }

    /// Copies `length` bytes from `offset` bytes back in the output.
    ///
    /// Copies byte-by-byte so overlapping matches (`offset < length`, i.e. RLE
    /// runs) reuse freshly written bytes, matching LZ semantics.
    fn copy_match(&mut self, offset: usize, length: u32) -> Result<(), Zx0Error<E>> {
        if offset == 0 || offset > MAX_OFFSET || offset > self.produced {
            return Err(Zx0Error::InvalidOffset);
        }
        let n = self.buf.len();
        for _ in 0..length {
            // `offset <= MAX_OFFSET < n`, so this stays non-negative and lands on
            // a byte that has not yet been overwritten this pass.
            let src = (self.write_idx + n - offset) % n;
            let byte = self.buf[src];
            self.write_byte(byte)?;
        }
        Ok(())
    }

    /// Flushes any bytes written since the last full-window flush.
    fn flush_remainder(&mut self) -> Result<(), Zx0Error<E>> {
        if self.write_idx > 0 {
            (self.output)(&self.buf[..self.write_idx]).map_err(Zx0Error::CallbackError)?;
            self.write_idx = 0;
        }
        Ok(())
    }
}

/// The block the decoder is about to read next.
enum State {
    /// Copy a run of literal bytes straight from the input.
    CopyLiterals,
    /// Copy a match reusing the previous offset.
    CopyFromLastOffset,
    /// Read a new offset, then copy a match from it.
    CopyFromNewOffset,
}

/// Decompresses a ZX0 v2 stream.
///
/// `input` is the compressed data, `window` is reusable scratch storage of at
/// least [`MIN_WINDOW_LEN`] bytes, and `output` receives the decompressed bytes
/// in order, in chunks. If `output` returns an error, decompression stops and
/// the error is returned as [`Zx0Error::CallbackError`].
pub fn decompress<E, F>(input: &[u8], window: &mut [u8], output: F) -> Result<(), Zx0Error<E>>
where
    F: FnMut(&[u8]) -> Result<(), E>,
{
    let mut reader = BitReader::new(input);
    let mut win = Window::new(window, output)?;

    // INITIAL_OFFSET in the reference implementation.
    let mut last_offset: usize = 1;
    let mut state = State::CopyLiterals;

    loop {
        match state {
            State::CopyLiterals => {
                let length = reader.read_elias_gamma(false)?;
                for _ in 0..length {
                    let byte = reader.read_byte()?;
                    win.write_byte(byte)?;
                }
                // A literal run is never followed by another literal run, so the
                // next bit chooses between reusing the last offset and a new one.
                state = if reader.read_bit()? == 1 {
                    State::CopyFromNewOffset
                } else {
                    State::CopyFromLastOffset
                };
            }

            State::CopyFromLastOffset => {
                let length = reader.read_elias_gamma(false)?;
                win.copy_match(last_offset, length)?;
                state = if reader.read_bit()? == 1 {
                    State::CopyFromNewOffset
                } else {
                    State::CopyLiterals
                };
            }

            State::CopyFromNewOffset => {
                let msb = reader.read_elias_gamma(true)?;
                if msb == END_MARKER {
                    win.flush_remainder()?;
                    if reader.has_trailing_data() {
                        return Err(Zx0Error::TrailingData);
                    }
                    return Ok(());
                }
                if msb > END_MARKER {
                    // Only reachable on corrupt input; keeps the offset math from
                    // overflowing and the result within the valid range.
                    return Err(Zx0Error::InvalidOffset);
                }
                let lsb = reader.read_offset_lsb()?;
                // msb in 1..=255 and lsb in 0..=127, so this is always in 1..=MAX_OFFSET.
                last_offset = msb as usize * 128 - lsb;
                let length = reader.read_elias_gamma(false)? + 1;
                win.copy_match(last_offset, length)?;
                state = if reader.read_bit()? == 1 {
                    State::CopyFromNewOffset
                } else {
                    State::CopyLiterals
                };
            }
        }
    }
}

/// Decompresses a ZX0 v2 stream whose entire output fits in `buffer`, returning
/// the decompressed bytes as a sub-slice of `buffer`.
///
/// Unlike [`decompress`] — which streams output through a callback and only needs
/// a [`MIN_WINDOW_LEN`]-sized scratch window — this leaves the whole decompressed
/// block sitting in `buffer`, so the decode window *is* the destination: no
/// separate output storage is required. This is the entry point for random-access
/// use such as serving fixed-size blocks: decode a block once into `buffer`, then
/// read any byte range of the returned slice as many times as you like.
///
/// `buffer` serves double duty as the back-reference window and the output, so it
/// must be both at least [`MIN_WINDOW_LEN`] bytes *and* large enough to hold the
/// decompressed output. The result borrows `buffer` for as long as it is read.
///
/// # Errors
///
/// - [`Zx0Error::WindowTooSmall`] if `buffer` is shorter than [`MIN_WINDOW_LEN`].
/// - [`Zx0Error::OutputTooLarge`] if the decompressed output exceeds `buffer`.
/// - the same input/format errors as [`decompress`] for a malformed stream.
///
/// ```ignore
/// let mut buf = [0u8; 0x8000];
/// let block = zx0_decompress::decompress_into(compressed_block, &mut buf)?;
/// // `block` is the fully decompressed block; index into it freely.
/// ```
pub fn decompress_into<'b>(
    input: &[u8],
    buffer: &'b mut [u8],
) -> Result<&'b [u8], Zx0Error<Infallible>> {
    let capacity = buffer.len();
    let mut produced = 0usize;
    let mut flushes = 0u32;

    decompress(input, buffer, |chunk| {
        produced += chunk.len();
        flushes += 1;
        Ok::<(), Infallible>(())
    })?;

    // The window flushes (and wraps) exactly when it fills, so a *single* flush
    // means the output never wrapped and is laid out linearly at `buffer[..produced]`.
    // More than one flush means the output is larger than `buffer` and its start
    // has already been overwritten -- unusable, so report it rather than return
    // corrupt data.
    if flushes > 1 || produced > capacity {
        return Err(Zx0Error::OutputTooLarge);
    }
    Ok(&buffer[..produced])
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;

    /// Decompresses `compressed` into a `Vec` using a default-sized window.
    fn decompress_to_vec(compressed: &[u8]) -> Result<Vec<u8>, Zx0Error<Infallible>> {
        let mut window = [0u8; 0x8000];
        let mut out = Vec::new();
        decompress(compressed, &mut window, |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(out)
    }

    /// Compresses `original` with the reference compressor, then asserts the
    /// decompressor reproduces it exactly.
    fn assert_roundtrips(original: &[u8]) {
        let compressed = zx0::compress(original);
        let decompressed = decompress_to_vec(&compressed).expect("decompression failed");
        assert_eq!(decompressed, original);
    }

    /// Deterministic data mixing random bytes with repeated runs, so the stream
    /// exercises both literal blocks and (overlapping) matches.
    fn pseudo_random_mixed(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x1234_5678;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            if next() & 1 == 0 {
                let run = (next() % 50 + 1) as usize;
                for _ in 0..run {
                    out.push(next() as u8);
                }
            } else {
                let byte = next() as u8;
                let run = (next() % 200 + 1) as usize;
                for _ in 0..run {
                    out.push(byte);
                }
            }
        }
        out.truncate(len);
        out
    }

    #[test]
    fn tiny_string() {
        assert_roundtrips(b"Hello, ZX0 world! Hello, ZX0 world!");
    }

    #[test]
    fn single_byte() {
        assert_roundtrips(b"X");
    }

    #[test]
    fn run_length_offset_one() {
        // A single repeated byte compresses to an offset-1 overlapping run.
        assert_roundtrips(&[b'A'; 5000]);
    }

    #[test]
    fn repeated_pattern() {
        let data: Vec<u8> = b"abcd".iter().copied().cycle().take(16_000).collect();
        assert_roundtrips(&data);
    }

    #[test]
    fn crosses_window_boundary() {
        // Larger than the 0x8000 window, exercising the ring wrap and the
        // multi-chunk flush path.
        assert_roundtrips(&pseudo_random_mixed(120_000));
    }

    #[test]
    fn sizes_around_window_boundary() {
        for &n in &[1usize, 127, 128, 255, 256, 32767, 32768, 32769, 65535, 65536] {
            assert_roundtrips(&pseudo_random_mixed(n));
        }
    }

    #[test]
    fn window_too_small_is_rejected() {
        let mut window = [0u8; MIN_WINDOW_LEN - 1];
        let result =
            decompress::<Infallible, _>(&[0, 0, 0, 0], &mut window, |_| Ok(()));
        assert_eq!(result, Err(Zx0Error::WindowTooSmall));
    }

    #[test]
    fn truncated_input_is_rejected() {
        let compressed = zx0::compress(&pseudo_random_mixed(50_000));
        let truncated = &compressed[..compressed.len() - 2];
        let mut window = [0u8; 0x8000];
        let result = decompress::<Infallible, _>(truncated, &mut window, |_| Ok(()));
        // A truncated stream runs out of input mid-decode.
        assert!(matches!(
            result,
            Err(Zx0Error::TruncatedInput) | Err(Zx0Error::InvalidOffset)
        ));
    }

    #[test]
    fn trailing_data_is_rejected() {
        let mut compressed = zx0::compress(b"payload payload payload payload");
        compressed.push(0xFF);
        let mut window = [0u8; 0x8000];
        let result = decompress::<Infallible, _>(&compressed, &mut window, |_| Ok(()));
        assert_eq!(result, Err(Zx0Error::TrailingData));
    }

    #[test]
    fn into_buffer_roundtrips_when_it_fits() {
        // Outputs smaller than and exactly equal to the buffer (the exact-fit
        // case is where the window flushes once and wraps on the final byte).
        for &n in &[1usize, 100, 4096, 32_767, 32_768] {
            let data = pseudo_random_mixed(n);
            let compressed = zx0::compress(&data);
            let mut buf = [0u8; 32_768];
            let out = decompress_into(&compressed, &mut buf).expect("output fits");
            assert_eq!(out, &data[..], "mismatch for n={n}");
        }
    }

    #[test]
    fn into_buffer_rejects_output_larger_than_buffer() {
        // Buffer is >= MIN_WINDOW_LEN (so not WindowTooSmall) but smaller than the
        // 40 KiB output.
        let data = pseudo_random_mixed(40_000);
        let compressed = zx0::compress(&data);
        let mut buf = [0u8; 32_768];
        assert_eq!(
            decompress_into(&compressed, &mut buf),
            Err(Zx0Error::OutputTooLarge)
        );
    }

    #[test]
    fn into_buffer_rejects_window_below_minimum() {
        let compressed = zx0::compress(b"hello");
        let mut buf = [0u8; MIN_WINDOW_LEN - 1];
        assert_eq!(
            decompress_into(&compressed, &mut buf),
            Err(Zx0Error::WindowTooSmall)
        );
    }

    #[test]
    fn callback_error_propagates() {
        let compressed = zx0::compress(b"data the callback will refuse to accept");
        let mut window = [0u8; 0x8000];
        let result = decompress(&compressed, &mut window, |_| Err(42u32));
        assert_eq!(result, Err(Zx0Error::CallbackError(42)));
    }
}
