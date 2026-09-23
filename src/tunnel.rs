//! The `axiom:tunnel` transport.
//!
//! Axiom multiplexes every serverbound packet over one plugin channel. A logical
//! packet is split into frames of at most `MAX_FRAME_LEN` bytes; each frame carries
//! a flag byte marking it as the first and/or last of its packet. Once reassembled
//! the payload is `identifier | i32 uncompressed_len | u8 flags | [zstd] body`.

// `split_frames` is the outbound half; it is used once responses get tunnelled.
#![allow(dead_code)]

use crate::buf::{self, Reader};

pub const FRAME_FLAG_FIRST: u8 = 1;
pub const FRAME_FLAG_LAST: u8 = 2;
pub const PACKET_FLAG_ZSTD: u8 = 1;

/// Mirrors Axiom's `tunnel-split-size`.
pub const MAX_FRAME_LEN: usize = 31000;
/// Mirrors Axiom's `maximum-tunnel-packet-size`.
pub const MAX_PACKET_LEN: usize = 2 * 1024 * 1024;

/// Upstream reassembles frames without bounding the total, so a client that sends
/// FIRST and then never LAST grows the buffer forever. Cap it at the same limit a
/// finished packet is allowed to reach.
const MAX_PENDING_LEN: usize = MAX_PACKET_LEN;

#[derive(Debug)]
pub enum Error {
    FrameTooLarge(usize),
    EmptyFrame,
    PendingTooLarge,
    PacketTooLarge(usize),
    Decode(buf::Error),
    Zstd(String),
    /// Decompressed body did not match the declared length.
    SizeMismatch {
        declared: usize,
        actual: usize,
    },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FrameTooLarge(n) => write!(f, "sent split buffer that was too large ({n})"),
            Self::EmptyFrame => f.write_str("sent empty tunnel frame"),
            Self::PendingTooLarge => f.write_str("reassembly buffer exceeded the packet limit"),
            Self::PacketTooLarge(n) => write!(f, "sent packet was too large ({n})"),
            Self::Decode(e) => write!(f, "malformed tunnel packet: {e}"),
            Self::Zstd(e) => write!(f, "zstd failed to decompress: {e}"),
            Self::SizeMismatch { declared, actual } => {
                write!(
                    f,
                    "uncompressed size didn't match real size ({declared} vs {actual})"
                )
            }
        }
    }
}

impl From<buf::Error> for Error {
    fn from(e: buf::Error) -> Self {
        Self::Decode(e)
    }
}

/// A fully reassembled, decompressed Axiom packet.
pub struct Packet {
    /// The `axiom:<name>` identifier the packet was tunnelled under.
    pub id: String,
    pub body: Vec<u8>,
}

/// Per-player reassembly state.
#[derive(Default)]
pub struct Tunnel {
    pending: Vec<u8>,
    /// Set when we dropped the first frame of a packet and must ignore the rest of it.
    skipping: bool,
}

impl Tunnel {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            skipping: false,
        }
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.skipping = false;
    }

    /// Feeds one `axiom:tunnel` frame, returning a packet once its last frame lands.
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Packet>, Error> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge(frame.len()));
        }
        let (&flags, rest) = frame.split_first().ok_or(Error::EmptyFrame)?;

        let first = flags & FRAME_FLAG_FIRST != 0;
        let last = flags & FRAME_FLAG_LAST != 0;

        if first && last {
            return decode(rest).map(Some);
        }

        if first {
            self.skipping = false;
            self.pending.clear();
        } else if self.skipping {
            return Ok(None);
        } else if self.pending.is_empty() {
            // Joined mid-packet; wait for the next FIRST frame.
            self.skipping = true;
            return Ok(None);
        }

        if self.pending.len() + rest.len() > MAX_PENDING_LEN {
            self.reset();
            return Err(Error::PendingTooLarge);
        }
        self.pending.extend_from_slice(rest);

        if !last {
            return Ok(None);
        }
        let combined = core::mem::take(&mut self.pending);
        decode(&combined).map(Some)
    }
}

fn decode(payload: &[u8]) -> Result<Packet, Error> {
    let mut r = Reader::new(payload);
    let id = r.string()?.to_owned();
    let declared = r.i32()?;
    let flags = r.u8()?;

    let declared = usize::try_from(declared).map_err(|_| Error::PacketTooLarge(0))?;
    if declared > MAX_PACKET_LEN {
        return Err(Error::PacketTooLarge(declared));
    }

    let body = if flags & PACKET_FLAG_ZSTD == 0 {
        r.rest().to_vec()
    } else {
        zstd_decompress(r.rest(), declared)?
    };
    Ok(Packet { id, body })
}

/// Decompresses into a buffer of exactly `expected` bytes, rejecting anything that
/// does not fill it — the declared size is what bounds our allocation, so a frame
/// that expands to more or less than it claimed is a protocol violation.
fn zstd_decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, Error> {
    use std::io::Read;

    let mut decoder =
        ruzstd::StreamingDecoder::new(input).map_err(|e| Error::Zstd(e.to_string()))?;

    let mut out = vec![0_u8; expected];
    let mut filled = 0;
    while filled < expected {
        match decoder.read(&mut out[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(Error::Zstd(e.to_string())),
        }
    }
    if filled != expected {
        return Err(Error::SizeMismatch {
            declared: expected,
            actual: filled,
        });
    }
    // Anything still pending means the frame expanded past its declared size.
    let mut extra = [0_u8; 1];
    match decoder.read(&mut extra) {
        Ok(0) => Ok(out),
        Ok(_) => Err(Error::SizeMismatch {
            declared: expected,
            actual: expected + 1,
        }),
        Err(e) => Err(Error::Zstd(e.to_string())),
    }
}

/// Splits a clientbound payload into tunnel frames. Axiom's client applies the same
/// reassembly rules in reverse.
#[must_use]
pub fn split_frames(payload: &[u8]) -> Vec<Vec<u8>> {
    let chunk = MAX_FRAME_LEN - 1;
    let mut frames = Vec::new();
    let mut offset = 0;
    loop {
        let end = (offset + chunk).min(payload.len());
        let mut flags = 0_u8;
        if offset == 0 {
            flags |= FRAME_FLAG_FIRST;
        }
        if end == payload.len() {
            flags |= FRAME_FLAG_LAST;
        }
        let mut frame = Vec::with_capacity(end - offset + 1);
        frame.push(flags);
        frame.extend_from_slice(&payload[offset..end]);
        frames.push(frame);
        if end == payload.len() {
            return frames;
        }
        offset = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::Writer;

    /// Builds the reassembled form of a tunnel packet: id, length, flags, body.
    fn packet(id: &str, body: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(id).i32(body.len() as i32).u8(0).bytes(body);
        w.into_vec()
    }

    #[test]
    fn single_frame_roundtrip() {
        let mut t = Tunnel::new();
        let mut frame = vec![FRAME_FLAG_FIRST | FRAME_FLAG_LAST];
        frame.extend_from_slice(&packet("axiom:hello", b"body"));

        let p = t.push(&frame).expect("accepted").expect("complete");
        assert_eq!(p.id, "axiom:hello");
        assert_eq!(p.body, b"body");
    }

    #[test]
    fn split_frames_reassemble() {
        let body = vec![7_u8; MAX_FRAME_LEN * 2];
        let frames = split_frames(&packet("axiom:set_buffer", &body));
        assert!(frames.len() > 2, "payload should span several frames");

        let mut t = Tunnel::new();
        let mut done = None;
        for f in &frames {
            assert!(f.len() <= MAX_FRAME_LEN);
            if let Some(p) = t.push(f).expect("accepted") {
                done = Some(p);
            }
        }
        let p = done.expect("last frame completes the packet");
        assert_eq!(p.id, "axiom:set_buffer");
        assert_eq!(p.body, body);
    }

    #[test]
    fn joining_mid_packet_skips_until_next_first() {
        let mut t = Tunnel::new();
        // A continuation frame with no FIRST seen yet is dropped, not misparsed.
        assert!(t.push(&[0, 1, 2, 3]).expect("accepted").is_none());
        assert!(
            t.push(&[FRAME_FLAG_LAST, 4, 5])
                .expect("accepted")
                .is_none()
        );

        let mut frame = vec![FRAME_FLAG_FIRST | FRAME_FLAG_LAST];
        frame.extend_from_slice(&packet("axiom:hello", b"ok"));
        let p = t.push(&frame).expect("accepted").expect("complete");
        assert_eq!(p.body, b"ok");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut t = Tunnel::new();
        let frame = vec![FRAME_FLAG_FIRST | FRAME_FLAG_LAST; MAX_FRAME_LEN + 1];
        assert!(matches!(t.push(&frame), Err(Error::FrameTooLarge(_))));
    }

    #[test]
    fn unterminated_stream_is_bounded() {
        let mut t = Tunnel::new();
        let mut flags = FRAME_FLAG_FIRST;
        // Never send LAST; reassembly must give up instead of growing forever.
        for _ in 0..(MAX_PENDING_LEN / (MAX_FRAME_LEN - 1) + 2) {
            let frame = vec![flags; MAX_FRAME_LEN];
            flags = 0;
            if let Err(e) = t.push(&frame) {
                assert!(matches!(e, Error::PendingTooLarge));
                return;
            }
        }
        panic!("reassembly buffer grew past its cap");
    }
}
