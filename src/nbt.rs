//! Block entity NBT, in the form Axiom ships it.
//!
//! Every block entity travels zstd-compressed against a dictionary trained on
//! Minecraft block entity NBT, which is what keeps a chest full of items down to a
//! few dozen bytes. The dictionary is part of the protocol rather than a tuning
//! knob — a blob compressed with it cannot be read without it — so it is embedded
//! here verbatim from the upstream plugin's resources.

use std::sync::{Mutex, OnceLock};

use ruzstd::FrameDecoder;
use ruzstd::decoding::dictionary::Dictionary;

const DICTIONARY: &[u8] = include_bytes!("../assets/block_entities_v1.dict");

/// The only dictionary upstream defines. A different id means the client is
/// speaking a newer protocol than this port understands.
pub const SUPPORTED_DICTIONARY: u8 = 0;

/// Ceiling on one decompressed block entity. A shulker box of written books is
/// the realistic worst case and stays far below this.
pub const MAX_DECOMPRESSED: usize = 1 << 20;

static DECODER: OnceLock<Option<Mutex<FrameDecoder>>> = OnceLock::new();

fn decoder() -> Option<&'static Mutex<FrameDecoder>> {
    DECODER
        .get_or_init(|| {
            let dictionary = Dictionary::decode_dict(DICTIONARY)
                .inspect_err(|e| tracing::error!("Block entity dictionary is unusable: {e}"))
                .ok()?;
            let mut decoder = FrameDecoder::new();
            decoder
                .add_dict(dictionary)
                .inspect_err(|e| tracing::error!("Block entity dictionary rejected: {e}"))
                .ok()?;
            Some(Mutex::new(decoder))
        })
        .as_ref()
}

/// Decompresses one block entity into exactly `original_size` bytes.
///
/// The declared size is what bounds the allocation, so a blob that expands to
/// anything else is rejected rather than trusted.
pub fn decompress(compressed: &[u8], original_size: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;

    if original_size > MAX_DECOMPRESSED {
        return Err(format!(
            "block entity NBT too large: {original_size} > {MAX_DECOMPRESSED}"
        ));
    }
    let decoder = decoder().ok_or("block entity dictionary unavailable")?;
    let mut guard = decoder.lock().unwrap_or_else(|e| e.into_inner());

    let mut stream = ruzstd::StreamingDecoder::new_with_decoder(compressed, &mut *guard)
        .map_err(|e| e.to_string())?;

    let mut out = vec![0_u8; original_size];
    let mut filled = 0;
    while filled < original_size {
        match stream.read(&mut out[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    if filled != original_size {
        return Err(format!(
            "block entity NBT size mismatch: {filled} != {original_size}"
        ));
    }
    Ok(out)
}

/// Rewrites a Java-written NBT document into the form Pumpkin reads.
///
/// `NbtIo.write` emits a tag id, a root *name* (always empty here), then the
/// payload. Pumpkin's reader expects the network form, where the root carries no
/// name at all, so the two name-length bytes have to come out.
pub fn to_unnamed_root(named: &[u8]) -> Option<Vec<u8>> {
    const COMPOUND: u8 = 10;

    let (&tag, rest) = named.split_first()?;
    if tag != COMPOUND {
        return None;
    }
    let (len_bytes, rest) = rest.split_at_checked(2)?;
    let name_len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    let (_name, payload) = rest.split_at_checked(name_len)?;

    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(COMPOUND);
    out.extend_from_slice(payload);
    Some(out)
}

/// The inverse of [`to_unnamed_root`], for NBT we send back to the client.
///
/// Pumpkin hands out the network form; `NbtIo.read` on the other side expects a
/// root name, so an empty one goes back in.
pub fn to_named_root(unnamed: &[u8]) -> Option<Vec<u8>> {
    const COMPOUND: u8 = 10;

    let (&tag, payload) = unnamed.split_first()?;
    if tag != COMPOUND {
        return None;
    }
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(COMPOUND);
    out.extend_from_slice(&0_u16.to_be_bytes());
    out.extend_from_slice(payload);
    Some(out)
}

/// Wraps bytes in a zstd frame built entirely from raw (stored) blocks.
///
/// Axiom's format demands zstd, and the reader on the other side holds the block
/// entity dictionary — but a frame that declares no dictionary decodes fine with
/// one loaded, and raw blocks need no entropy tables at all. That buys format
/// compatibility without dragging a zstd *encoder* into the WASM build, which
/// would mean a C toolchain. The cost is that what we send back is not actually
/// compressed; these payloads are small and infrequent.
#[must_use]
pub fn compress_raw(data: &[u8]) -> Vec<u8> {
    const MAGIC: u32 = 0xFD2F_B528;
    /// Largest payload a single raw block may carry.
    const MAX_BLOCK: usize = 128 * 1024;

    let mut out = Vec::with_capacity(data.len() + 32);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    // Single segment, 4-byte content size, no dictionary, no checksum.
    out.push(0b1010_0000);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());

    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0]);
        return out;
    }
    let mut blocks = data.chunks(MAX_BLOCK).peekable();
    while let Some(block) = blocks.next() {
        let last = u32::from(blocks.peek().is_none());
        // Block_Size in the upper bits, block type 0 (raw) in bits 1..2.
        let header = ((block.len() as u32) << 3) | last;
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.extend_from_slice(block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_an_empty_root_name() {
        // Compound, name "", one byte of payload (TAG_End).
        let named = [10_u8, 0, 0, 0];
        assert_eq!(to_unnamed_root(&named), Some(vec![10, 0]));
    }

    #[test]
    fn strips_a_non_empty_root_name() {
        let mut named = vec![10_u8, 0, 2, b'h', b'i'];
        named.push(0);
        assert_eq!(to_unnamed_root(&named), Some(vec![10, 0]));
    }

    #[test]
    fn rejects_documents_that_are_not_a_compound() {
        assert_eq!(to_unnamed_root(&[1, 0, 0]), None);
        assert_eq!(to_unnamed_root(&[]), None);
        // Truncated before the name length, and before the name itself.
        assert_eq!(to_unnamed_root(&[10, 0]), None);
        assert_eq!(to_unnamed_root(&[10, 0, 4, b'x']), None);
    }

    /// The dictionary has to load, or every block entity in every paste is lost.
    #[test]
    fn embedded_dictionary_loads() {
        assert!(decoder().is_some(), "block entity dictionary failed to load");
    }

    #[test]
    fn named_and_unnamed_roots_round_trip() {
        let unnamed = vec![10_u8, 1, 0, 1, b'a', 7, 0];
        let named = to_named_root(&unnamed).expect("wraps");
        assert_eq!(&named[..3], &[10, 0, 0]);
        assert_eq!(to_unnamed_root(&named), Some(unnamed));
    }

    /// The frames we hand back must be readable by a real zstd decoder, or every
    /// block entity we send is garbage on arrival.
    #[test]
    fn raw_frames_decode() {
        use std::io::Read;

        for payload in [
            Vec::new(),
            b"short".to_vec(),
            // Long enough to span more than one raw block.
            (0..300_000_u32).map(|i| i as u8).collect(),
        ] {
            let frame = compress_raw(&payload);
            let mut decoded = Vec::new();
            ruzstd::StreamingDecoder::new(frame.as_slice())
                .expect("frame header is valid")
                .read_to_end(&mut decoded)
                .expect("frame body is valid");
            assert_eq!(decoded, payload, "payload of {} bytes", payload.len());
        }
    }

    /// And the decompressor we use for inbound data must accept them too, since
    /// that is the path a round trip through the client would take.
    #[test]
    fn raw_frames_survive_our_own_decompressor() {
        let payload = b"a block entity".to_vec();
        let frame = compress_raw(&payload);
        assert_eq!(decompress(&frame, payload.len()), Ok(payload));
    }
}
