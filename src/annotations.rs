//! Shared annotations — Axiom's collaborative drawing layer.
//!
//! The server never needs to understand what an annotation *is*; it stores the
//! encoded body and relays it to everyone in the world. Only the move and rotate
//! actions reach inside, and only for the two kinds that carry a position, so the
//! bodies are kept as bytes and patched in place rather than modelled.

use std::sync::Mutex;

use pumpkin_plugin_api::{Server, player::Player};

use crate::buf::{Error, Reader, Result, Writer};

/// Upstream's `allow-annotations`, which defaults to off.
pub const ENABLED: bool = true;

/// Guards against a client filling the server's memory with drawings.
const MAX_ANNOTATIONS: usize = 4096;

type Uuid = (u64, u64);

struct Annotation {
    uuid: Uuid,
    /// The encoded `AnnotationData`, starting with its type byte.
    body: Vec<u8>,
}

/// Annotations live per world, keyed by dimension id.
///
/// ponytail: in memory only, so they are lost on restart — upstream keeps them in
/// the world's persistent data. Persisting needs the plugin's data folder and the
/// `fs.write.data` permission.
static WORLDS: Mutex<Vec<(String, Vec<Annotation>)>> = Mutex::new(Vec::new());

fn with_world<R>(dimension: &str, f: impl FnOnce(&mut Vec<Annotation>) -> R) -> R {
    let mut guard = WORLDS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = guard.iter().position(|(id, _)| id == dimension) {
        return f(&mut guard[index].1);
    }
    guard.push((dimension.to_owned(), Vec::new()));
    let last = guard.len() - 1;
    f(&mut guard[last].1)
}

/// Where an annotation body keeps its position and rotation, if it has them.
///
/// Both text and image annotations store three floats of position followed by four
/// of rotation, immediately after a leading string.
const POSITION_BYTES: usize = 12;
const ROTATION_BYTES: usize = 16;

fn positioned_offset(body: &[u8]) -> Option<usize> {
    let mut r = Reader::new(body);
    // Only text (1) and image (2) carry a transform; the rest ignore move/rotate
    // upstream too.
    if !matches!(r.u8().ok()?, 1 | 2) {
        return None;
    }
    r.string().ok()?;
    let offset = r.position();
    (body.len() >= offset + POSITION_BYTES + ROTATION_BYTES).then_some(offset)
}

/// Consumes one encoded annotation body and returns the bytes it spans.
fn take_body<'a>(r: &mut Reader<'a>) -> Result<&'a [u8]> {
    /// Offsets and point lists are bounded so a malformed body cannot make us
    /// allocate; the values themselves are never interpreted here.
    const MAX_POINTS: usize = 1 << 20;

    let start = r.position();
    match r.u8()? {
        // Line: quantised start, width, colour, packed offsets.
        0 => {
            for _ in 0..3 {
                r.var_i32()?;
            }
            r.f32()?;
            r.i32()?;
            r.byte_array(MAX_POINTS)?;
        }
        // Text: the string, transform, facing, then styling.
        1 => {
            r.string()?;
            r.take(POSITION_BYTES + ROTATION_BYTES)?;
            r.u8()?;
            r.f32()?;
            r.f32()?;
            r.u8()?;
            r.i32()?;
            r.bool()?;
        }
        // Image: the url, transform, facing, then size and opacity.
        2 => {
            r.string()?;
            r.take(POSITION_BYTES + ROTATION_BYTES)?;
            r.u8()?;
            r.f32()?;
            r.f32()?;
            r.f32()?;
            r.u8()?;
        }
        // Freehand outline: start, count, colour, packed offsets.
        3 => {
            for _ in 0..4 {
                r.var_i32()?;
            }
            r.i32()?;
            r.byte_array(MAX_POINTS)?;
        }
        // Lines outline: packed positions, then colour.
        4 => {
            let count = r.var_len("outline positions", MAX_POINTS)?;
            r.take(count.checked_mul(8).ok_or(Error::Eof)?)?;
            r.i32()?;
        }
        // Box outline: two corners and a colour.
        5 => {
            for _ in 0..6 {
                r.var_i32()?;
            }
            r.i32()?;
        }
        other => {
            return Err(Error::TooLarge {
                what: "annotation type",
                len: other as usize,
                max: 5,
            });
        }
    }
    r.since(start)
}

/// `axiom:annotation_update` — a batch of create/delete/move/rotate/clear actions.
pub fn handle(server: &Server, player: &Player, body: &[u8]) -> core::result::Result<(), String> {
    let err = |e: Error| e.to_string();
    let mut r = Reader::new(body);
    let count = r
        .var_len("annotation actions", MAX_ANNOTATIONS)
        .map_err(err)?;

    let dimension = player.get_world().get_dimension();
    let mut applied = Writer::new();
    let mut applied_count = 0_usize;

    with_world(
        &dimension,
        |annotations| -> core::result::Result<(), String> {
            for _ in 0..count {
                let action = r.u8().map_err(err)?;
                let start = r.position();

                match action {
                    0 => {
                        let uuid = r.uuid().map_err(err)?;
                        let body = take_body(&mut r).map_err(err)?.to_vec();
                        if annotations.len() >= MAX_ANNOTATIONS {
                            continue;
                        }
                        annotations.retain(|a| a.uuid != uuid);
                        annotations.push(Annotation { uuid, body });
                    }
                    1 => {
                        let uuid = r.uuid().map_err(err)?;
                        annotations.retain(|a| a.uuid != uuid);
                    }
                    2 | 4 => {
                        let uuid = r.uuid().map_err(err)?;
                        let width = if action == 2 {
                            POSITION_BYTES
                        } else {
                            ROTATION_BYTES
                        };
                        let value = r.take(width).map_err(err)?;
                        if let Some(annotation) = annotations.iter_mut().find(|a| a.uuid == uuid)
                            && let Some(offset) = positioned_offset(&annotation.body)
                        {
                            let at = if action == 2 {
                                offset
                            } else {
                                offset + POSITION_BYTES
                            };
                            annotation.body[at..at + width].copy_from_slice(value);
                        }
                    }
                    3 => annotations.clear(),
                    other => return Err(format!("unknown annotation action: {other}")),
                }

                // Relay exactly the bytes we accepted, so every client ends up with the
                // same state we hold.
                applied.u8(action).bytes(r.since(start).map_err(err)?);
                applied_count += 1;
            }
            Ok(())
        },
    )?;

    if applied_count > 0 {
        let mut payload = Writer::new();
        payload
            .var_i32(applied_count as i32)
            .bytes(&applied.into_vec());
        broadcast(server, &dimension, &payload.into_vec());
    }
    Ok(())
}

/// Sends a player the whole current state: clear, then one create per annotation.
pub fn send_all(player: &Player) {
    let dimension = player.get_world().get_dimension();
    let (count, body) = with_world(&dimension, |annotations| {
        let mut w = Writer::new();
        // Action 3 clears whatever the client was holding.
        w.u8(3);
        for annotation in annotations.iter() {
            w.u8(0).uuid(annotation.uuid.0, annotation.uuid.1);
            w.bytes(&annotation.body);
        }
        (annotations.len() + 1, w.into_vec())
    });

    let mut payload = Writer::new();
    payload.var_i32(count as i32).bytes(&body);
    crate::send(player, "axiom:annotation_update", &payload.into_vec());
}

fn broadcast(server: &Server, dimension: &str, payload: &[u8]) {
    for player in server.get_all_players() {
        if player.get_world().get_dimension() == dimension {
            crate::send(&player, "axiom:annotation_update", payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box outline: six varints and a colour, and nothing that can be moved.
    fn box_outline() -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(5);
        for value in [1, 2, 3, 4, 5, 6] {
            w.var_i32(value);
        }
        w.i32(0x00FF_00FF);
        w.into_vec()
    }

    /// A text annotation, which is one of the two kinds that carry a transform.
    fn text(position: [f32; 3], rotation: [f32; 4]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(1).string("hello");
        for value in position {
            w.f32(value);
        }
        for value in rotation {
            w.f32(value);
        }
        w.u8(0).f32(0.0).f32(1.0).u8(0).i32(-1).bool(true);
        w.into_vec()
    }

    /// Bodies sit inside a stream of actions, so a body measured wrong desyncs
    /// everything after it.
    #[test]
    fn measures_bodies_exactly() {
        for body in [box_outline(), text([1.0, 2.0, 3.0], [0.0; 4])] {
            let mut stream = body.clone();
            stream.extend_from_slice(b"NEXT");
            let mut r = Reader::new(&stream);
            assert_eq!(take_body(&mut r), Ok(body.as_slice()));
            assert_eq!(r.rest(), b"NEXT");
        }
    }

    #[test]
    fn finds_the_transform_only_where_there_is_one() {
        let body = text([1.0, 2.0, 3.0], [4.0, 5.0, 6.0, 7.0]);
        let offset = positioned_offset(&body).expect("text carries a transform");
        assert_eq!(&body[offset..offset + 4], &1.0_f32.to_be_bytes());
        assert_eq!(
            &body[offset + POSITION_BYTES..offset + POSITION_BYTES + 4],
            &4.0_f32.to_be_bytes()
        );
        assert!(positioned_offset(&box_outline()).is_none());
    }

    #[test]
    fn rejects_unknown_and_truncated_bodies() {
        assert!(take_body(&mut Reader::new(&[99])).is_err());
        let body = box_outline();
        assert!(take_body(&mut Reader::new(&body[..body.len() - 2])).is_err());
    }
}
