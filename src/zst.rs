//! Zstandard streams, through the pure-Rust structured-zstd.
//!
//! Decompression accepts what the reference zstd command writes, and decodes
//! to the same bytes. Compressed output is deterministic but differs from
//! the reference implementation's.

use std::io::{self, Read, Write};

use structured_zstd::decoding::{ContentChecksum, FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{CompressionLevel, StreamingEncoder};

/// The zstd command's default compression level.
pub const DEFAULT_LEVEL: i32 = 3;

/// Whether `prefix` starts a zstd frame or a skippable frame.
pub fn is_zstd(prefix: &[u8]) -> bool {
    match prefix {
        [0x28, 0xb5, 0x2f, 0xfd, ..] => true,
        [b, 0x2a, 0x4d, 0x18, ..] => b & 0xf0 == 0x50,
        _ => false,
    }
}

/// An encoder of one frame with a content checksum, as the zstd command
/// writes; a known `size` is recorded in the frame header. The encoder must
/// not be flushed, which would end a block early: `finish` it instead.
pub fn encoder<W: Write>(out: W, level: i32, size: Option<u64>) -> io::Result<StreamingEncoder<W>> {
    let mut encoder = StreamingEncoder::new(out, CompressionLevel::from_level(level));
    encoder.set_content_checksum(true)?;
    if let Some(size) = size {
        encoder.set_pledged_content_size(size)?;
    }
    Ok(encoder)
}

/// A decoder of concatenated frames that verifies content checksums.
pub fn decoder<R: Read>(input: R) -> StreamingDecoder<R, FrameDecoder> {
    let mut frames = FrameDecoder::new();
    frames.set_content_checksum(ContentChecksum::Verify);
    StreamingDecoder::new_with_decoder(input, frames)
}
