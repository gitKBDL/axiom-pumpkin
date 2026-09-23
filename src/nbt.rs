//! Block entity NBT, in the form Axiom ships it.
//!
//! Every block entity travels zstd-compressed against a dictionary trained on
//! Minecraft block entity NBT, which is what keeps a chest full of items down to a
//! few dozen bytes. The dictionary is part of the protocol rather than a tuning
//! knob — a blob compressed with it cannot be read without it — so it is embedded
//! here verbatim from the upstream plugin's resources.

use std::borrow::Cow;
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

    /// A tag has to be measured exactly, or every field after it in the packet is
    /// read from the wrong offset.
    #[test]
    fn measures_a_nested_tag_exactly() {
        use crate::buf::Reader;

        // Compound { "a": Int 1, "l": List<String>["hi"], "b": ByteArray[2] }
        let tag: Vec<u8> = vec![
            10, // compound
            3, 0, 1, b'a', 0, 0, 0, 1, // int "a" = 1
            9, 0, 1, b'l', 8, 0, 0, 0, 1, 0, 2, b'h', b'i', // list "l" of one string
            7, 0, 1, b'b', 0, 0, 0, 2, 9, 9, // byte array "b" of two bytes
            0,  // end of compound
        ];
        let mut trailing = tag.clone();
        trailing.extend_from_slice(b"AFTER");

        let mut r = Reader::new(&trailing);
        assert_eq!(take_network_tag(&mut r), Ok(tag.as_slice()));
        assert_eq!(r.rest(), b"AFTER");
    }

    #[test]
    fn an_absent_tag_is_a_single_byte() {
        use crate::buf::Reader;
        let mut r = Reader::new(&[0, 42]);
        assert_eq!(take_network_tag(&mut r), Ok(&[0_u8][..]));
        assert_eq!(r.rest(), &[42]);
    }

    #[test]
    fn a_truncated_tag_is_rejected() {
        use crate::buf::Reader;
        // Claims an int payload but supplies two bytes of it.
        assert!(take_network_tag(&mut Reader::new(&[10, 3, 0, 1, b'a', 0, 0])).is_err());
        // Unknown tag type.
        assert!(take_network_tag(&mut Reader::new(&[99])).is_err());
    }

    fn field<'a>(tag: u8, name: &'a [u8], payload: &'a [u8]) -> Field<'a> {
        Field {
            tag,
            name,
            payload: Cow::Borrowed(payload),
        }
    }

    fn names(fields: &[Field<'_>]) -> Vec<String> {
        fields
            .iter()
            .map(|f| String::from_utf8_lossy(f.name).into_owned())
            .collect()
    }

    #[test]
    fn compounds_round_trip_and_an_absent_tag_is_empty() {
        let tag = write_compound(&[field(1, b"a", &[7]), field(8, b"s", &[0, 2, b'h', b'i'])]);
        assert_eq!(write_compound(&read_compound(&tag).unwrap()), tag);
        assert_eq!(read_compound(&[0]), Ok(Vec::new()));
        assert!(read_compound(&[3, 0, 0, 0, 1]).is_err(), "an int is not a compound");
        assert_eq!(string(&read_compound(&tag).unwrap(), b"s"), Some("hi"));
    }

    /// Anything outside upstream's allow-list is dropped, riders included.
    #[test]
    fn sanitizing_keeps_only_allowed_keys_at_every_level() {
        let rider = write_compound(&[field(1, b"Glowing", &[1]), field(6, b"Health", &[0; 8])]);
        let riders = compound_list(&[rider]);
        let tag = write_compound(&[
            field(8, b"id", &[0, 3, b'p', b'i', b'g']),
            field(5, b"Health", &[0; 4]),
            field(9, b"Passengers", &riders),
        ]);
        let mut fields = read_compound(&tag).unwrap();
        sanitize_entity(&mut fields).unwrap();
        assert_eq!(names(&fields), ["id", "Passengers"]);
        let riders = read_compound_list(&fields[1].payload).unwrap();
        assert_eq!(riders.len(), 1);
        assert_eq!(names(&riders[0]), ["Glowing"]);
    }

    #[test]
    fn merging_recurses_into_compounds_and_an_empty_one_removes_the_key() {
        let pose = write_compound(&[field(1, b"Head", &[1]), field(1, b"Body", &[2])]);
        let brightness = write_compound(&[field(3, b"sky", &[0, 0, 0, 15])]);
        let current = write_compound(&[
            field(10, b"Pose", &pose[1..]),
            field(10, b"brightness", &brightness[1..]),
            field(1, b"Small", &[0]),
        ]);
        let head_only = write_compound(&[field(1, b"Head", &[9])]);
        let changes = write_compound(&[
            field(10, b"Pose", &head_only[1..]),
            field(10, b"brightness", &[0]),
            field(1, b"Small", &[1]),
            field(1, b"Glowing", &[1]),
        ]);

        let mut fields = read_compound(&current).unwrap();
        merge(&mut fields, read_compound(&changes).unwrap()).unwrap();
        assert_eq!(names(&fields), ["Pose", "Small", "Glowing"]);
        assert_eq!(fields[1].payload.as_ref(), &[1]);

        // The limb that was not edited keeps its pose.
        let pose = read_fields(&mut crate::buf::Reader::new(&fields[0].payload)).unwrap();
        assert_eq!(pose, [field(1, b"Head", &[9]), field(1, b"Body", &[2])]);
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

/// Maximum nesting a tag may use. Vanilla stops at 512; the point is only to keep
/// a hostile packet from recursing until the stack gives out.
const MAX_DEPTH: u32 = 512;

/// Consumes one network-form NBT value — a type byte followed by its payload, with
/// no root name — and returns the bytes it spanned.
///
/// Axiom embeds these in the middle of packets, so the only way to reach the next
/// field is to walk the tag and find where it ends.
pub fn take_network_tag<'a>(r: &mut crate::buf::Reader<'a>) -> crate::buf::Result<&'a [u8]> {
    let start = r.position();
    let tag = r.u8()?;
    if tag != 0 {
        skip_payload(r, tag, 0)?;
    }
    r.since(start)
}

fn skip_payload(r: &mut crate::buf::Reader<'_>, tag: u8, depth: u32) -> crate::buf::Result<()> {
    use crate::buf::Error;

    if depth > MAX_DEPTH {
        return Err(Error::TooLarge {
            what: "nbt nesting",
            len: depth as usize,
            max: MAX_DEPTH as usize,
        });
    }
    match tag {
        0 => {}
        1 => {
            r.take(1)?;
        }
        2 => {
            r.take(2)?;
        }
        3 | 5 => {
            r.take(4)?;
        }
        4 | 6 => {
            r.take(8)?;
        }
        7 | 11 | 12 => {
            let width = if tag == 7 {
                1
            } else if tag == 11 {
                4
            } else {
                8
            };
            let len = array_len(r)?;
            r.take(len.checked_mul(width).ok_or(Error::Eof)?)?;
        }
        8 => {
            let len = usize::from(r.u16()?);
            r.take(len)?;
        }
        9 => {
            let element = r.u8()?;
            let len = array_len(r)?;
            // A list of TAG_End carries no payload regardless of its length.
            if element != 0 {
                for _ in 0..len {
                    skip_payload(r, element, depth + 1)?;
                }
            }
        }
        10 => loop {
            let entry = r.u8()?;
            if entry == 0 {
                break;
            }
            let name_len = usize::from(r.u16()?);
            r.take(name_len)?;
            skip_payload(r, entry, depth + 1)?;
        },
        other => {
            return Err(Error::TooLarge {
                what: "nbt tag type",
                len: other as usize,
                max: 12,
            });
        }
    }
    Ok(())
}

fn array_len(r: &mut crate::buf::Reader<'_>) -> crate::buf::Result<usize> {
    let raw = r.i32()?;
    usize::try_from(raw).map_err(|_| crate::buf::Error::TooLarge {
        what: "nbt array length",
        len: 0,
        max: i32::MAX as usize,
    })
}

const TAG_BYTE: u8 = 1;
const TAG_FLOAT: u8 = 5;
const TAG_DOUBLE: u8 = 6;
const TAG_STRING: u8 = 8;
const TAG_LIST: u8 = 9;
const TAG_COMPOUND: u8 = 10;

/// One named value of a compound: its tag type, its name and its payload.
///
/// Payloads stay borrowed from the packet unless an edit replaces them, so
/// filtering a tag copies only what it writes back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field<'a> {
    pub tag: u8,
    pub name: &'a [u8],
    pub payload: Cow<'a, [u8]>,
}

impl<'a> Field<'a> {
    pub const fn list(name: &'a [u8], payload: Vec<u8>) -> Self {
        Self {
            tag: TAG_LIST,
            name,
            payload: Cow::Owned(payload),
        }
    }

    pub fn byte(name: &'a [u8], value: u8) -> Self {
        Self {
            tag: TAG_BYTE,
            name,
            payload: Cow::Owned(vec![value]),
        }
    }
}

/// Replaces the field of the same name, or adds it.
pub fn put<'a>(fields: &mut Vec<Field<'a>>, field: Field<'a>) {
    match fields.iter_mut().find(|existing| existing.name == field.name) {
        Some(existing) => *existing = field,
        None => fields.push(field),
    }
}

/// Splits one network-form compound into its fields. A lone `TAG_End`, which is
/// how the protocol writes "no tag", reads as an empty compound.
pub fn read_compound(tag: &[u8]) -> crate::buf::Result<Vec<Field<'_>>> {
    let mut r = crate::buf::Reader::new(tag);
    let fields = match r.u8()? {
        0 => Vec::new(),
        TAG_COMPOUND => read_fields(&mut r)?,
        _ => return Err(crate::buf::Error::Unexpected("nbt root is not a compound")),
    };
    r.expect_fully_read()?;
    Ok(fields)
}

/// Reads a compound's payload up to and including its `TAG_End`.
fn read_fields<'a>(r: &mut crate::buf::Reader<'a>) -> crate::buf::Result<Vec<Field<'a>>> {
    let mut fields = Vec::new();
    loop {
        let tag = r.u8()?;
        if tag == 0 {
            return Ok(fields);
        }
        let name_len = usize::from(r.u16()?);
        let name = r.take(name_len)?;
        let start = r.position();
        skip_payload(r, tag, 1)?;
        fields.push(Field {
            tag,
            name,
            payload: Cow::Borrowed(r.since(start)?),
        });
    }
}

/// The inverse of [`read_compound`].
pub fn write_compound(fields: &[Field<'_>]) -> Vec<u8> {
    let mut out = vec![TAG_COMPOUND];
    write_fields(&mut out, fields);
    out
}

fn write_fields(out: &mut Vec<u8>, fields: &[Field<'_>]) {
    for field in fields {
        out.push(field.tag);
        out.extend_from_slice(&(field.name.len() as u16).to_be_bytes());
        out.extend_from_slice(field.name);
        out.extend_from_slice(&field.payload);
    }
    out.push(0);
}

/// Splits a list payload into its compounds. A list of anything else holds no
/// entities, so it reads as empty.
pub fn read_compound_list(payload: &[u8]) -> crate::buf::Result<Vec<Vec<Field<'_>>>> {
    let mut r = crate::buf::Reader::new(payload);
    let element = r.u8()?;
    let len = array_len(&mut r)?;
    let mut compounds = Vec::new();
    if element == TAG_COMPOUND {
        for _ in 0..len {
            compounds.push(read_fields(&mut r)?);
        }
    }
    r.expect_fully_read()?;
    Ok(compounds)
}

/// A list payload of the given compounds, each as [`write_compound`] produces it.
pub fn compound_list(compounds: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![TAG_COMPOUND];
    out.extend_from_slice(&(compounds.len() as i32).to_be_bytes());
    for compound in compounds {
        // List elements carry no type byte of their own.
        out.extend_from_slice(&compound[1..]);
    }
    out
}

/// A list payload of doubles, the shape of `Pos`.
pub fn doubles(values: &[f64]) -> Vec<u8> {
    let mut out = vec![TAG_DOUBLE];
    out.extend_from_slice(&(values.len() as i32).to_be_bytes());
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

/// A list payload of floats, the shape of `Rotation`.
pub fn floats(values: &[f32]) -> Vec<u8> {
    let mut out = vec![TAG_FLOAT];
    out.extend_from_slice(&(values.len() as i32).to_be_bytes());
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

/// The value of a byte field, if the compound has one by that name.
pub fn byte(fields: &[Field<'_>], name: &[u8]) -> Option<u8> {
    let field = fields.iter().find(|f| f.name == name && f.tag == TAG_BYTE)?;
    field.payload.first().copied()
}

/// The value of a string field, if the compound has one by that name.
pub fn string<'f>(fields: &'f [Field<'_>], name: &[u8]) -> Option<&'f str> {
    let field = fields.iter().find(|f| f.name == name && f.tag == TAG_STRING)?;
    // Modified UTF-8 only differs from UTF-8 for NUL and astral characters,
    // neither of which appears in the ids this is used for.
    std::str::from_utf8(field.payload.get(2..)?).ok()
}

/// Root keys a client may set on an entity, from upstream's `NbtSanitization`.
/// Anything else a saved entity carries — health, inventories, attributes, AI
/// state — is dropped, so the entity tools cannot be used to hand those out.
const ALLOWED_ENTITY_KEYS: &[&str] = &[
    "id",
    // Any entity.
    "Pos",
    "Rotation",
    "Invulnerable",
    "CustomName",
    "CustomNameVisible",
    "Silent",
    "NoGravity",
    "Glowing",
    "Tags",
    "Passengers",
    // Armor stands.
    "ArmorItems",
    "HandItems",
    "Small",
    "ShowArms",
    "DisabledSlots",
    "NoBasePlate",
    "Marker",
    "Pose",
    // Markers.
    "data",
    // Display entities.
    "transformation",
    "interpolation_duration",
    "start_interpolation",
    "teleport_duration",
    "billboard",
    "view_range",
    "shadow_radius",
    "shadow_strength",
    "width",
    "height",
    "glow_color_override",
    "brightness",
    "line_width",
    "text_opacity",
    "background",
    "shadow",
    "see_through",
    "default_background",
    "alignment",
    "text",
    "block_state",
    "item",
    "item_display",
];

/// Drops every field a client may not set, from the entity and from everything
/// riding it.
pub fn sanitize_entity(fields: &mut Vec<Field<'_>>) -> crate::buf::Result<()> {
    fields.retain(|field| {
        ALLOWED_ENTITY_KEYS.iter().any(|key| key.as_bytes() == field.name)
            && (field.name != b"Passengers" || field.tag == TAG_LIST)
    });
    for field in fields.iter_mut().filter(|field| field.name == b"Passengers") {
        let mut riders = read_compound_list(&field.payload)?;
        let mut sanitized = Vec::with_capacity(riders.len());
        for rider in &mut riders {
            sanitize_entity(rider)?;
            sanitized.push(write_compound(rider));
        }
        field.payload = Cow::Owned(compound_list(&sanitized));
    }
    Ok(())
}

/// Merges `right` into `left` the way upstream's entity editor does: compounds
/// merge key by key, an empty compound removes the key, and any other value
/// replaces what was there.
///
/// Replacing a nested compound outright would reset what the edit left out — a
/// pose for one armor stand limb would put the other limbs back to default.
pub fn merge<'a>(left: &mut Vec<Field<'a>>, right: Vec<Field<'a>>) -> crate::buf::Result<()> {
    for field in right {
        let existing = left.iter().position(|l| l.name == field.name);
        if field.tag == TAG_COMPOUND {
            let changes = read_fields(&mut crate::buf::Reader::new(&field.payload))?;
            if changes.is_empty() {
                if let Some(index) = existing {
                    left.remove(index);
                }
                continue;
            }
            if let Some(index) = existing.filter(|&index| left[index].tag == TAG_COMPOUND) {
                let mut child = read_fields(&mut crate::buf::Reader::new(&left[index].payload))?;
                merge(&mut child, changes)?;
                let mut payload = Vec::new();
                write_fields(&mut payload, &child);
                left[index].payload = Cow::Owned(payload);
                continue;
            }
        }
        match existing {
            Some(index) => left[index] = field,
            None => left.push(field),
        }
    }
    Ok(())
}
