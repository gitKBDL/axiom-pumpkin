//! Vanilla's `PalettedContainer` network format for block-state sections.
//!
//! Layout, as written since 1.21.5 (and therefore by 26.2):
//!
//! ```text
//! u8   bits_per_entry
//! palette   0      -> varint, one value for the whole section
//!           1..=8  -> varint length, then that many varints
//!           other  -> nothing; entries are registry ids already
//! i64  packed entries, least-significant entry first, never split across words.
//!      The count is implied by bits_per_entry, not length-prefixed.
//! ```

// `write_section` is the outbound half, used once chunk-data requests land.
#![allow(dead_code)]

use crate::buf::{Error, Reader, Result, Writer};

/// 16 × 16 × 16.
pub const SECTION_VOLUME: usize = 4096;

/// Vanilla stores 1–4 bit palettes at 4 bits per entry regardless of what fits.
const MIN_INDIRECT_BITS: u8 = 4;
/// Registry ids are `u16`, so nothing legitimate needs more.
const MAX_BITS: u8 = 16;

/// Index of a block within a section, matching vanilla's `(y << 8) | (z << 4) | x`.
#[must_use]
pub const fn index_of(x: usize, y: usize, z: usize) -> usize {
    (y << 8) | (z << 4) | x
}

const fn packed_len(bits: u8) -> usize {
    let per_word = 64 / bits as usize;
    SECTION_VOLUME.div_ceil(per_word)
}

/// Reads one section's worth of registry ids.
///
/// `direct_bits` is what the server would use for a global-palette section; it is
/// only a fallback, since the declared width describes the bytes actually sent.
pub fn read_section(r: &mut Reader<'_>, direct_bits: u8) -> Result<Vec<u16>> {
    let declared = r.u8()?;

    if declared == 0 {
        let value = read_id(r)?;
        return Ok(vec![value; SECTION_VOLUME]);
    }

    let (bits, palette) = if declared <= 8 {
        let bits = declared.max(MIN_INDIRECT_BITS);
        let len = r.var_len("palette", 1 << bits)?;
        let mut palette = Vec::with_capacity(len);
        for _ in 0..len {
            palette.push(read_id(r)?);
        }
        (bits, Some(palette))
    } else {
        (declared.max(direct_bits).min(MAX_BITS), None)
    };

    let per_word = 64 / bits as usize;
    let mask = (1_u64 << bits) - 1;
    let words = r.take(packed_len(bits) * 8)?;

    let mut out = Vec::with_capacity(SECTION_VOLUME);
    for i in 0..SECTION_VOLUME {
        let word_start = (i / per_word) * 8;
        let word = u64::from_be_bytes([
            words[word_start],
            words[word_start + 1],
            words[word_start + 2],
            words[word_start + 3],
            words[word_start + 4],
            words[word_start + 5],
            words[word_start + 6],
            words[word_start + 7],
        ]);
        let raw = ((word >> ((i % per_word) * bits as usize)) & mask) as usize;

        out.push(match &palette {
            Some(palette) => *palette.get(raw).ok_or(Error::TooLarge {
                what: "palette index",
                len: raw,
                max: palette.len(),
            })?,
            None => u16::try_from(raw).map_err(|_| Error::TooLarge {
                what: "registry id",
                len: raw,
                max: u16::MAX as usize,
            })?,
        });
    }
    Ok(out)
}

fn read_id(r: &mut Reader<'_>) -> Result<u16> {
    let raw = r.var_i32()?;
    u16::try_from(raw).map_err(|_| Error::TooLarge {
        what: "registry id",
        len: 0,
        max: u16::MAX as usize,
    })
}

/// Largest palette an indirect section can carry, at 8 bits per entry.
const MAX_INDIRECT_PALETTE: usize = 1 << 8;

/// Writes a section using an indirect palette.
///
/// Indirect is deliberate: for widths of 4..=8 the reader honours the width we
/// declare, whereas a direct section is decoded at whatever width the reader's own
/// registry implies — which we would have to guess for an older client. Returns
/// `false` if the section holds more distinct states than a palette can address,
/// which a 16³ section realistically never does.
pub fn write_section_indirect(w: &mut Writer, entries: &[u16]) -> bool {
    debug_assert_eq!(entries.len(), SECTION_VOLUME);

    let mut palette: Vec<u16> = Vec::new();
    let mut indices = Vec::with_capacity(SECTION_VOLUME);
    for &entry in entries {
        let index = match palette.iter().position(|&seen| seen == entry) {
            Some(index) => index,
            None => {
                if palette.len() == MAX_INDIRECT_PALETTE {
                    return false;
                }
                palette.push(entry);
                palette.len() - 1
            }
        };
        indices.push(index as u64);
    }

    let bits = encompassing_bits(palette.len()).max(MIN_INDIRECT_BITS);
    let per_word = 64 / bits as usize;

    w.u8(bits);
    w.var_i32(palette.len() as i32);
    for entry in &palette {
        w.var_i32(i32::from(*entry));
    }

    let mut word = 0_u64;
    let mut in_word = 0;
    for (i, &index) in indices.iter().enumerate() {
        word |= index << (in_word * bits as usize);
        in_word += 1;
        if in_word == per_word || i == SECTION_VOLUME - 1 {
            w.i64(word as i64);
            word = 0;
            in_word = 0;
        }
    }
    true
}

/// Bits needed to address `len` palette entries.
fn encompassing_bits(len: usize) -> u8 {
    let len = len.max(2) as u32;
    (u32::BITS - (len - 1).leading_zeros()) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The width a 1.21.9-era registry needs for a global palette.
    const DIRECT_BITS: u8 = 15;

    fn roundtrip(entries: &[u16]) {
        let mut w = Writer::new();
        assert!(
            write_section_indirect(&mut w, entries),
            "palette should fit"
        );
        let bytes = w.into_vec();
        let decoded = read_section(&mut Reader::new(&bytes), DIRECT_BITS).expect("decodes");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn indirect_palette_roundtrips() {
        // 200 distinct states, so the palette needs the full 8 bits.
        let entries: Vec<u16> = (0..SECTION_VOLUME)
            .map(|i| (i % 200 + 15_000) as u16)
            .collect();
        roundtrip(&entries);
    }

    #[test]
    fn a_uniform_section_roundtrips() {
        roundtrip(&vec![1_u16; SECTION_VOLUME]);
    }

    #[test]
    fn a_section_too_varied_for_a_palette_is_refused() {
        let entries: Vec<u16> = (0..SECTION_VOLUME).map(|i| (i % 1000) as u16).collect();
        let mut w = Writer::new();
        assert!(!write_section_indirect(&mut w, &entries));
    }

    #[test]
    fn single_value_palette_fills_the_section() {
        let mut w = Writer::new();
        w.u8(0).var_i32(1234);
        let decoded = read_section(&mut Reader::new(&w.into_vec()), DIRECT_BITS).expect("decodes");
        assert_eq!(decoded, vec![1234_u16; SECTION_VOLUME]);
    }

    /// A 2-bit palette is stored at 4 bits per entry; reading it at its declared
    /// width would desynchronise every entry after the first word.
    #[test]
    fn narrow_indirect_palette_uses_four_bits() {
        let palette = [7_u16, 9, 11];
        let mut w = Writer::new();
        w.u8(2).var_i32(palette.len() as i32);
        for id in palette {
            w.var_i32(i32::from(id));
        }
        // 16 entries per word: indices 0,1,2,0,1,2,... over 256 words.
        for word in 0..packed_len(4) {
            let mut packed = 0_u64;
            for slot in 0..16 {
                let index = ((word * 16 + slot) % palette.len()) as u64;
                packed |= index << (slot * 4);
            }
            w.i64(packed as i64);
        }

        let decoded = read_section(&mut Reader::new(&w.into_vec()), DIRECT_BITS).expect("decodes");
        for (i, &value) in decoded.iter().enumerate() {
            assert_eq!(value, palette[i % palette.len()], "entry {i}");
        }
    }

    #[test]
    fn out_of_range_palette_index_is_rejected() {
        let mut w = Writer::new();
        w.u8(4).var_i32(1).var_i32(5);
        for _ in 0..packed_len(4) {
            // Every entry points at index 1, but the palette only has index 0.
            w.i64(0x1111_1111_1111_1111_u64 as i64);
        }
        assert!(read_section(&mut Reader::new(&w.into_vec()), DIRECT_BITS).is_err());
    }

    #[test]
    fn truncated_section_is_rejected() {
        let mut w = Writer::new();
        w.u8(15);
        w.i64(0);
        assert_eq!(
            read_section(&mut Reader::new(&w.into_vec()), DIRECT_BITS),
            Err(Error::Eof)
        );
    }

    #[test]
    fn index_matches_vanilla_order() {
        assert_eq!(index_of(0, 0, 0), 0);
        assert_eq!(index_of(1, 0, 0), 1);
        assert_eq!(index_of(0, 0, 1), 16);
        assert_eq!(index_of(0, 1, 0), 256);
        assert_eq!(index_of(15, 15, 15), SECTION_VOLUME - 1);
    }
}
