//! Serverbound packet handlers.

use std::sync::atomic::{AtomicU32, Ordering};

use pumpkin_plugin_api::{
    GameRule, GameRuleValue, Server,
    common::{BlockPos, GameMode},
    java_packets::{CAcknowledgeBlockChange, ClientboundPacket},
    player::Player,
    world::{self, BlockFlags, Chunk, Entity, EntityType, World},
};

use crate::block_remap::{self, Tables};
use crate::buf::{Reader, Writer, pack_block_pos, unpack_block_pos};
use crate::biomes;
use crate::nbt;
use crate::palette::{self, SECTION_VOLUME, index_of};
use crate::permissions;
use crate::proto::{consume_dispatch_sends, has_perm};
use crate::send;

/// Upstream's default `packetCollectionReadLimit`, which bounds how many entries a
/// single packet may declare before we start allocating for them.
const MAX_COLLECTION: usize = 1024;

/// A block buffer is terminated by this sentinel key rather than a count.
const MIN_POSITION_LONG: i64 = pack_block_pos(-33_554_432, -2048, -33_554_432);

/// Upstream caps a section at one block entity per block.
const MAX_BLOCK_ENTITIES: usize = SECTION_VOLUME;

/// A single compressed block entity; generous, since these are NBT blobs.
const MAX_BLOCK_ENTITY_BYTES: usize = 1 << 20;

/// Total decompressed block entity NBT allowed per buffer. Each blob is bounded
/// on its own, but a buffer full of tiny blobs that each expand to the per-blob
/// ceiling would still be a decompression bomb.
const MAX_BUFFER_NBT_BYTES: usize = 32 << 20;

/// A block entity waiting for its block to be placed. The compressed payload is
/// borrowed from the packet rather than copied, since most of them are dropped:
/// a section carries entities for blocks the buffer may leave untouched.
struct PendingBlockEntity<'a> {
    /// Position within the section, packed as `x | y << 4 | z << 8` — note this
    /// is not the order the block palette uses.
    offset: u16,
    original_size: usize,
    dictionary: u8,
    compressed: &'a [u8],
}

fn apply_block_entity(
    world: &World,
    pos: BlockPos,
    entity: &PendingBlockEntity<'_>,
    budget: &mut usize,
) {
    if entity.dictionary != nbt::SUPPORTED_DICTIONARY {
        tracing::debug!("Skipping block entity with dictionary {}", entity.dictionary);
        return;
    }
    if entity.original_size > *budget {
        return;
    }
    *budget -= entity.original_size;

    match nbt::decompress(entity.compressed, entity.original_size) {
        Ok(named) => match nbt::to_unnamed_root(&named) {
            Some(unnamed) => {
                if let Err(e) = world.set_block_entity_nbt(pos, &unnamed) {
                    tracing::debug!("Block entity at {pos:?} rejected: {e}");
                }
            }
            None => tracing::debug!("Block entity at {pos:?} is not an NBT compound"),
        },
        Err(e) => tracing::debug!("Block entity at {pos:?} failed to decompress: {e}"),
    }
}

/// Cached `get_block_state_count`; 0 means "not looked up yet".
static STATE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Cached id of `minecraft:void_air`, which a block buffer uses to mean
/// "leave this block alone"; `u16::MAX` means "not looked up yet".
static VOID_AIR: AtomicU32 = AtomicU32::new(u32::MAX);

fn state_count() -> u32 {
    let mut count = STATE_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        count = world::get_block_state_count();
        STATE_COUNT.store(count, Ordering::Relaxed);
    }
    count
}

/// Width of the server's global palette, which is what a direct-palette section
/// in a block buffer is packed at.
fn direct_bits() -> u8 {
    let count = state_count().max(2);
    (u32::BITS - (count - 1).leading_zeros()) as u8
}

fn void_air() -> u16 {
    let cached = VOID_AIR.load(Ordering::Relaxed);
    if cached != u32::MAX {
        return cached as u16;
    }
    // Absent void_air the buffer has no "unchanged" marker, so fall back to an id
    // no section can contain rather than silently overwriting the world with air.
    let id = world::resolve_block_state("minecraft:void_air", &[]).unwrap_or(u16::MAX);
    VOID_AIR.store(u32::from(id), Ordering::Relaxed);
    id
}

/// The id translation this client needs, if any.
///
/// Pumpkin serves clients older than its own version and remaps block state ids in
/// the chunk data it sends, but plugin messages pass through untouched — so an
/// older client's Axiom packets arrive in *its* registry, where the same number
/// means a different block. Upstream leans on ViaVersion for exactly this.
fn client_remap(player: &Player) -> Option<&'static Tables> {
    player
        .as_java()
        .and_then(|java| block_remap::tables_for(java.get_version()))
}

/// Human-readable note about whether this client's block ids need translating.
pub fn describe_client_registry(player: &Player) -> String {
    let Some(java) = player.as_java() else {
        return "not a Java client".to_owned();
    };
    let version = java.get_version();
    if block_remap::tables_for(version).is_some() {
        format!("client {version:?}, translating block ids to the server registry")
    } else {
        format!("client {version:?}, same block registry as the server")
    }
}

/// Converts one id from the client's registry into the server's and checks it.
///
/// `None` means the id has no equivalent here; callers treat that as "leave this
/// block alone", which is the only safe reading — guessing a replacement would
/// silently rewrite the player's build.
fn server_state(remap: Option<&'static Tables>, client_id: u16) -> Option<u16> {
    let id = match remap {
        Some(tables) => block_remap::to_server(tables.to_server, client_id)?,
        None => client_id,
    };
    (u32::from(id) < state_count()).then_some(id)
}

/// The reverse: one of our ids expressed in the client's registry.
fn client_state(remap: Option<&'static Tables>, server_id: u16) -> Option<u16> {
    match remap {
        Some(tables) => block_remap::to_client(tables.to_client, server_id),
        None => Some(server_id),
    }
}

/// Flags matching Axiom's two placement modes.
fn placement_flags(update_neighbors: bool) -> BlockFlags {
    if update_neighbors {
        BlockFlags::NOTIFY_NEIGHBORS | BlockFlags::NOTIFY_LISTENERS
    } else {
        // Axiom's "no updates" capability: the state lands exactly as sent, without
        // the placement callbacks that would pop a torch off a wall or reshape a
        // fence. `NOTIFY_LISTENERS` still queues the change for nearby clients.
        // `MOVED` is what actually suppresses the neighbour shape pass; without it
        // Pumpkin reshapes neighbours even with NOTIFY_NEIGHBORS cleared. Lighting
        // is refreshed regardless of flags, so it stays correct either way.
        BlockFlags::NOTIFY_LISTENERS
            | BlockFlags::MOVED
            | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK
            | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK
    }
}

/// `axiom:set_block` — a handful of individual placements, as produced by the
/// regular place/break tools rather than a brush.
pub fn set_block(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let err = |e: crate::buf::Error| e.to_string();

    let count = r.var_len("block map", MAX_COLLECTION).map_err(err)?;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        let pos = r.block_pos().map_err(err)?;
        let state = r.var_i32().map_err(err)?;
        blocks.push((pos, state));
    }

    let update_neighbors = r.bool().map_err(err)?;
    let mut prevent_updates_at = Vec::new();
    if update_neighbors {
        let n = r.var_len("prevent-updates set", MAX_COLLECTION).map_err(err)?;
        prevent_updates_at.reserve(n);
        for _ in 0..n {
            prevent_updates_at.push(r.block_pos().map_err(err)?);
        }
    }

    let _reason = r.var_i32().map_err(err)?;
    let _breaking = r.bool().map_err(err)?;
    // BlockHitResult: position, face, the three hit offsets, then "inside block".
    let _hit_pos = r.block_pos().map_err(err)?;
    let _hit_face = r.var_i32().map_err(err)?;
    let _hit_x = r.f32().map_err(err)?;
    let _hit_y = r.f32().map_err(err)?;
    let _hit_z = r.f32().map_err(err)?;
    let _inside = r.bool().map_err(err)?;
    let _hand = r.var_i32().map_err(err)?;
    let sequence_id = r.var_i32().map_err(err)?;

    // The client predicts its own placements and rolls them back unless the
    // sequence is acknowledged, so this has to happen even if we place nothing.
    if sequence_id >= 0
        && let Some(java) = player.as_java()
    {
        java.send_packet(&ClientboundPacket::CAcknowledgeBlockChange(
            CAcknowledgeBlockChange { sequence_id },
        ));
    }

    if !has_perm(player, permissions::BUILD_PLACE) {
        return Ok(());
    }

    let world = player.get_world();
    let remap = client_remap(player);
    for ((x, y, z), state) in blocks {
        let Some(state) = u16::try_from(state).ok().and_then(|id| server_state(remap, id)) else {
            continue;
        };
        // Upstream skips neighbour updates for a block adjacent to any position the
        // client asked to leave alone, so a no-update placement cannot be undone by
        // its neighbour's update.
        let near_protected = prevent_updates_at.iter().any(|&(px, py, pz)| {
            (px - x).abs() + (py - y).abs() + (pz - z).abs() <= 1
        });
        world.set_block_state(
            BlockPos { x, y, z },
            state,
            placement_flags(update_neighbors && !near_protected),
        );
    }
    Ok(())
}

/// Logs what the first buffer actually contains, once per server start.
///
/// The "leave this alone" marker is a registry id agreed on implicitly between
/// client and server; if the two disagree, every untouched block in the edited
/// volume gets overwritten and nothing else in the pipeline complains.
static DIAGNOSED: AtomicU32 = AtomicU32::new(0);

fn describe(id: u16) -> String {
    world::block_state_to_info(id).map_or_else(
        || format!("{id} (unknown)"),
        |info| format!("{id} ({})", info.name),
    )
}

fn diagnose_first_section(remap: Option<&'static Tables>, declared_bits: u8, entries: &[u16]) {
    if DIAGNOSED.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    let mut counts: Vec<(u16, usize)> = Vec::new();
    for &id in entries {
        match counts.iter_mut().find(|(seen, _)| *seen == id) {
            Some((_, n)) => *n += 1,
            None => counts.push((id, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    // Report ids as the server sees them; the raw wire value is in the client's
    // registry and naming it with our own table is how this looked like a
    // turtle_egg problem the first time round.
    let top: Vec<String> = counts
        .iter()
        .take(3)
        .map(|(id, n)| match server_state(remap, *id) {
            Some(server) => format!("{}×{n}", describe(server)),
            None => format!("{id} (no server equivalent)×{n}"),
        })
        .collect();

    tracing::info!(
        "Axiom buffer diagnostics: state_count={}, direct_bits={}, empty marker={}, \
         section declared {declared_bits} bits, {} distinct ids, most common: {}",
        state_count(),
        direct_bits(),
        describe(void_air()),
        counts.len(),
        top.join(", "),
    );
}

/// Compares a dimension id off the wire with the server's, tolerating an implicit
/// `minecraft:` namespace on either side.
fn same_dimension(a: &str, b: &str) -> bool {
    fn bare(s: &str) -> &str {
        s.strip_prefix("minecraft:").unwrap_or(s)
    }
    bare(a) == bare(b)
}

/// `axiom:set_buffer` — whole chunk sections at a time, as produced by brushes,
/// shapes and pastes. Blocks left as `void_air` in the buffer are untouched.
pub fn set_buffer(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let err = |e: crate::buf::Error| e.to_string();

    // Dimension the client believes it is editing, then a buffer id we do not need.
    let world_key = r.string().map_err(err)?.to_owned();
    let _buffer_id = r.uuid().map_err(err)?;

    match r.u8().map_err(err)? {
        0 => apply_block_buffer(player, &mut r, &world_key),
        1 => apply_biome_buffer(player, &mut r, &world_key),
        other => Err(format!("unknown buffer type: {other}")),
    }
}

fn apply_block_buffer<'a>(
    player: &Player,
    r: &mut Reader<'a>,
    world_key: &str,
) -> Result<(), String> {
    let err = |e: crate::buf::Error| e.to_string();

    if !has_perm(player, permissions::BUILD_SECTION) {
        return Ok(());
    }
    let world = player.get_world();
    let dimension = world.get_dimension();
    if !same_dimension(&dimension, world_key) {
        // Normally means the player changed dimension while the buffer was in
        // flight. Logged because the alternative cause — an id spelled differently
        // on either side — would otherwise look like edits silently doing nothing.
        tracing::warn!("Dropping block buffer for {world_key}; player is in {dimension}");
        return Ok(());
    }

    let empty = void_air();
    let remap = client_remap(player);
    let direct = direct_bits();
    let flags = placement_flags(false);
    let allow_nbt = has_perm(player, permissions::BUILD_NBT);
    let mut nbt_budget = MAX_BUFFER_NBT_BYTES;
    let mut sections = 0_usize;
    let mut changed = 0_usize;

    loop {
        let key = r.i64().map_err(err)?;
        if key == MIN_POSITION_LONG {
            break;
        }
        let (section_x, section_y, section_z) = unpack_block_pos(key);
        let declared_bits = r.peek_u8().map_err(err)?;
        let entries = palette::read_section(r, direct).map_err(err)?;
        diagnose_first_section(remap, declared_bits, &entries);

        let count = r
            .var_len("block entities", MAX_BLOCK_ENTITIES)
            .map_err(err)?;
        let mut entities: Vec<PendingBlockEntity<'a>> = Vec::new();
        for _ in 0..count {
            let offset = r.i16().map_err(err)? as u16;
            let original_size = r
                .var_len("block entity NBT", nbt::MAX_DECOMPRESSED)
                .map_err(err)?;
            let dictionary = r.u8().map_err(err)?;
            let compressed = r.byte_array(MAX_BLOCK_ENTITY_BYTES).map_err(err)?;
            if allow_nbt {
                entities.push(PendingBlockEntity {
                    offset,
                    original_size,
                    dictionary,
                    compressed,
                });
            }
        }
        entities.sort_unstable_by_key(|entity| entity.offset);

        let base_x = section_x * 16;
        let base_y = section_y * 16;
        let base_z = section_z * 16;
        for y in 0..16 {
            for z in 0..16 {
                for x in 0..16 {
                    let Some(state) = server_state(remap, entries[index_of(x, y, z)]) else {
                        continue;
                    };
                    if state == empty {
                        continue;
                    }
                    let pos = BlockPos {
                        x: base_x + x as i32,
                        y: base_y + y as i32,
                        z: base_z + z as i32,
                    };
                    world.set_block_state(pos, state, flags);
                    changed += 1;

                    if !entities.is_empty() {
                        let offset = (x | (y << 4) | (z << 8)) as u16;
                        if let Ok(found) =
                            entities.binary_search_by_key(&offset, |entity| entity.offset)
                        {
                            apply_block_entity(&world, pos, &entities[found], &mut nbt_budget);
                        }
                    }
                }
            }
        }
        sections += 1;
    }

    // The client spends its own copy of this budget before sending; if the server
    // never reconciles and tops it up, the client goes quiet after one buffer.
    let client_available = r.var_i32().unwrap_or(0);
    let within_limit = consume_dispatch_sends(
        player,
        i32::try_from(sections).unwrap_or(i32::MAX),
        client_available,
    );

    tracing::debug!(
        "Applied {changed} blocks across {sections} sections for {}",
        player.get_name()
    );

    if within_limit {
        Ok(())
    } else {
        Err("you are sending updates too fast".to_owned())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_remap::{tables_for, to_client, to_server};
    use pumpkin_plugin_api::player::JavaMinecraftVersion;

    /// Regression: a 1.21.9 client calls `void_air` 15090 while the 26.2 server
    /// calls it 15292. Reading that id literally made every block a buffer meant
    /// to leave untouched get overwritten.
    #[test]
    fn older_client_ids_translate_to_the_server_registry() {
        let tables = tables_for(JavaMinecraftVersion::V1219).expect("1.21.9 needs translation");
        assert_eq!(to_server(tables.to_server, 15090), Some(15292), "void_air");
        assert_eq!(to_server(tables.to_server, 1), Some(1), "stone is unchanged");
        assert_eq!(to_server(tables.to_server, 0), Some(0), "air is unchanged");
        // And back again, for ids we send to the client.
        assert_eq!(to_client(tables.to_client, 15292), Some(15090), "void_air");
        assert_eq!(to_client(tables.to_client, 1), Some(1), "stone is unchanged");
    }

    /// A client on the server's own version must not be translated at all.
    #[test]
    fn current_version_needs_no_translation() {
        assert!(tables_for(JavaMinecraftVersion::V262).is_none());
    }
}

/// `axiom:set_gamemode`
pub fn set_gamemode(player: &Player, body: &[u8]) -> Result<(), String> {
    let mode = Reader::new(body).u8().map_err(|e| e.to_string())?;
    let (mode, permission) = match mode {
        0 => (GameMode::Survival, permissions::PLAYER_GAMEMODE_SURVIVAL),
        1 => (GameMode::Creative, permissions::PLAYER_GAMEMODE_CREATIVE),
        2 => (GameMode::Adventure, permissions::PLAYER_GAMEMODE_ADVENTURE),
        3 => (GameMode::Spectator, permissions::PLAYER_GAMEMODE_SPECTATOR),
        other => return Err(format!("unknown game mode: {other}")),
    };
    if has_perm(player, permission) {
        player.set_gamemode(mode);
    }
    Ok(())
}

/// `axiom:set_fly_speed`
pub fn set_fly_speed(player: &Player, body: &[u8]) -> Result<(), String> {
    let speed = Reader::new(body).f32().map_err(|e| e.to_string())?;
    if !has_perm(player, permissions::PLAYER_SPEED) {
        return Ok(());
    }
    let mut abilities = player.get_abilities();
    abilities.fly_speed = speed.clamp(-1.0, 1.0);
    player.set_abilities(abilities);
    Ok(())
}

/// `axiom:teleport`
pub fn teleport(server: &Server, player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let err = |e: crate::buf::Error| e.to_string();

    let dimension = r.string().map_err(err)?.to_owned();
    let x = r.f64().map_err(err)?;
    let y = r.f64().map_err(err)?;
    let z = r.f64().map_err(err)?;
    let yaw = r.f32().map_err(err)?;
    let pitch = r.f32().map_err(err)?;

    if !has_perm(player, permissions::PLAYER_TELEPORT) {
        return Ok(());
    }
    let Some(target) = server
        .get_all_worlds()
        .into_iter()
        .find(|world| same_dimension(&world.get_dimension(), &dimension))
    else {
        return Ok(());
    };
    player.teleport((x, y, z), Some(yaw), Some(pitch), target);
    Ok(())
}

/// `axiom:set_world_time`
pub fn set_world_time(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let err = |e: crate::buf::Error| e.to_string();

    let dimension = r.string().map_err(err)?.to_owned();
    // Both fields are optional; the client sends whichever the user changed.
    let time = r.bool().map_err(err)?.then(|| r.i32()).transpose().map_err(err)?;
    let freeze = r.bool().map_err(err)?.then(|| r.bool()).transpose().map_err(err)?;

    if !has_perm(player, permissions::WORLD_TIME) || (time.is_none() && freeze.is_none()) {
        return Ok(());
    }
    let world = player.get_world();
    if !same_dimension(&world.get_dimension(), &dimension) {
        return Ok(());
    }
    if let Some(time) = time {
        world.set_time_of_day(u64::from(time.unsigned_abs()));
    }
    if let Some(freeze) = freeze {
        world.set_game_rule(GameRule::AdvanceTime, GameRuleValue::Bool(!freeze));
    }
    Ok(())
}

/// `axiom:set_no_physical_trigger`
///
/// ponytail: the flag is tracked but not enforced — Pumpkin's `interact-action`
/// has no physical variant and its generic game event carries no entity, so
/// there is nothing to cancel from a plugin. Enforcing it needs a server-side
/// hook; until then a player with this on still trips pressure plates.
pub fn set_no_physical_trigger(player: &Player, body: &[u8]) -> Result<(), String> {
    let enabled = Reader::new(body).bool().map_err(|e| e.to_string())?;
    if has_perm(player, permissions::PLAYER_SETNOPHYSICALTRIGGER) {
        crate::proto::set_no_physical_trigger(player, enabled);
    }
    Ok(())
}

/// Above this, answering a chunk request cost more than a server tick.
const SLOW_REQUEST: std::time::Duration = std::time::Duration::from_millis(50);

/// One clientbound payload may carry a mebibyte; upstream leaves the same leeway.
const MAX_RESPONSE_BYTES: usize = (1 << 20) - 64;

/// Sections served per request. Every block crosses the WASM boundary on its own
/// host call, so an unbounded request would stall the server tick; a partial
/// answer is legitimate — upstream also drops sections it cannot reach.
const MAX_SECTIONS_PER_REQUEST: usize = 128;

/// Accumulates a chunk data response, splitting it across payloads as it fills.
///
/// The wire shape is a block entity list, then a section list, each closed by a
/// sentinel, then a "finished" flag. A split has to close whichever lists are
/// still open before sending, and the continuation reopens at the same point —
/// which is the whole reason this is a type rather than inline code.
struct ChunkResponse<'a> {
    player: &'a Player,
    id: i64,
    buf: Writer,
    entities_open: bool,
}

impl<'a> ChunkResponse<'a> {
    fn new(player: &'a Player, id: i64) -> Self {
        let mut buf = Writer::new();
        buf.i64(id);
        Self {
            player,
            id,
            buf,
            entities_open: true,
        }
    }

    /// Appends one already-encoded entry, starting a new payload if it would not fit.
    fn push(&mut self, entry: &[u8]) {
        if self.buf.len() + entry.len() > MAX_RESPONSE_BYTES {
            self.flush(false);
        }
        self.buf.bytes(entry);
    }

    /// Closes the block entity list and moves on to sections.
    fn end_entities(&mut self) {
        self.buf.i64(MIN_POSITION_LONG);
        self.entities_open = false;
    }

    fn flush(&mut self, finished: bool) {
        if self.entities_open {
            self.buf.i64(MIN_POSITION_LONG);
        }
        self.buf.i64(MIN_POSITION_LONG);
        self.buf.bool(finished);

        let payload = core::mem::take(&mut self.buf).into_vec();
        send(self.player, "axiom:response_chunk_data", &payload);

        self.buf.i64(self.id);
        if !self.entities_open {
            // Block entities were already closed; the continuation says so again.
            self.buf.i64(MIN_POSITION_LONG);
        }
    }

    fn finish(mut self) {
        self.flush(true);
    }
}

/// `axiom:request_chunk_data` — the client asking for world data it cannot see
/// itself, which is what makes copying beyond render distance work.
pub fn request_chunk_data(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let err = |e: crate::buf::Error| e.to_string();
    let id = r.i64().map_err(err)?;

    // The client waits on a response for every request, so a refusal still has to
    // be answered — just with nothing in it.
    if !has_perm(player, permissions::CHUNK_REQUEST) {
        ChunkResponse::new(player, id).finish();
        return Ok(());
    }

    let dimension = r.string().map_err(err)?.to_owned();
    let _entities_in_chunks = r.bool().map_err(err)?;

    let requested_entities = r.var_len("block entity list", MAX_COLLECTION).map_err(err)?;
    let mut entity_positions = Vec::with_capacity(requested_entities);
    for _ in 0..requested_entities {
        entity_positions.push(r.i64().map_err(err)?);
    }

    let requested_sections = r.var_len("section list", MAX_COLLECTION).map_err(err)?;
    let mut section_keys = Vec::with_capacity(requested_sections.min(MAX_SECTIONS_PER_REQUEST));
    for i in 0..requested_sections {
        let key = r.i64().map_err(err)?;
        if i < MAX_SECTIONS_PER_REQUEST {
            section_keys.push(key);
        }
    }

    let world = player.get_world();
    if !same_dimension(&world.get_dimension(), &dimension) {
        ChunkResponse::new(player, id).finish();
        return Ok(());
    }

    let remap = client_remap(player);
    let started = std::time::Instant::now();
    let mut response = ChunkResponse::new(player, id);

    if has_perm(player, permissions::CHUNK_REQUESTBLOCKENTITY) {
        for packed in entity_positions {
            let (x, y, z) = unpack_block_pos(packed);
            if world.get_chunk(x >> 4, z >> 4).is_none() {
                continue;
            }
            let Some(nbt) = world.get_block_entity_nbt(BlockPos { x, y, z }) else {
                continue;
            };
            let Some(named) = nbt::to_named_root(&nbt) else {
                continue;
            };
            let mut entry = Writer::new();
            entry.i64(packed);
            entry.var_i32(named.len() as i32);
            entry.u8(nbt::SUPPORTED_DICTIONARY);
            entry.byte_array(&nbt::compress_raw(&named));
            response.push(&entry.into_vec());
        }
    }
    response.end_entities();

    let mut served = 0_usize;
    for key in section_keys {
        let (section_x, section_y, section_z) = unpack_block_pos(key);
        let Some(chunk) = world.get_chunk(section_x, section_z) else {
            // Not loaded, and a plugin cannot force a load; the client keeps
            // whatever it already had for this section.
            continue;
        };

        let mut entry = Writer::new();
        entry.i64(key);
        match section_for_client(&chunk, section_y, remap) {
            Some(entries) => {
                entry.bool(true);
                if !palette::write_section_indirect(&mut entry, &entries) {
                    continue;
                }
            }
            None => {
                entry.bool(false);
            }
        }
        response.push(&entry.into_vec());
        served += 1;
    }
    response.finish();

    // Every block read is its own host call, so this is the one place a single
    // Axiom packet can stall a tick. Surface it when it actually does.
    let elapsed = started.elapsed();
    if elapsed > SLOW_REQUEST {
        tracing::info!(
            "Served {served} chunk sections to {} in {elapsed:?}",
            player.get_name()
        );
    } else {
        tracing::debug!(
            "Served {served} chunk sections to {} in {elapsed:?}",
            player.get_name()
        );
    }
    Ok(())
}

/// `axiom:set_buffer` type 1 — the biome painter.
///
/// Biomes travel as a byte per 4x4x4 cell indexing a small per-buffer palette of
/// registry names, grouped into 16x16x16 blocks of cells.
fn apply_biome_buffer(player: &Player, r: &mut Reader<'_>, world_key: &str) -> Result<(), String> {
    let err = |e: crate::buf::Error| e.to_string();

    let palette_len = usize::from(r.u8().map_err(err)?);
    let mut palette = Vec::with_capacity(palette_len);
    for _ in 0..palette_len {
        // A biome this server does not know stays `None` and its cells are skipped.
        palette.push(biomes::from_name(r.string().map_err(err)?));
    }

    let unset = r.u8().map_err(err)?;
    if !has_perm(player, permissions::BUILD_SECTION) {
        return Ok(());
    }
    let world = player.get_world();
    let dimension = world.get_dimension();
    if !same_dimension(&dimension, world_key) {
        tracing::warn!("Dropping biome buffer for {world_key}; player is in {dimension}");
        return Ok(());
    }

    let mut painted = 0_usize;
    loop {
        let key = r.i64().map_err(err)?;
        if key == MIN_POSITION_LONG {
            break;
        }
        let (block_x, block_y, block_z) = unpack_block_pos(key);
        let cells = r.take(SECTION_VOLUME).map_err(err)?;

        for (index, &entry) in cells.iter().enumerate() {
            // 0 means "never set"; the default value means "unchanged".
            if entry == 0 || entry == unset {
                continue;
            }
            let Some(Some(biome)) = palette.get(usize::from(entry) - 1) else {
                continue;
            };
            // Cells are laid out with x fastest, then y, then z.
            let (x, y, z) = (index % 16, (index / 16) % 16, index / 256);
            // Key and index are in cell units; a cell covers four blocks per axis.
            world.set_biome(
                BlockPos {
                    x: (block_x * 16 + x as i32) * 4,
                    y: (block_y * 16 + y as i32) * 4,
                    z: (block_z * 16 + z as i32) * 4,
                },
                *biome,
            );
            painted += 1;
        }
    }
    tracing::debug!("Painted {painted} biome cells for {}", player.get_name());
    Ok(())
}

/// Reads one section out of a loaded chunk and re-expresses it in the client's
/// registry.
///
/// Returns `None` for a section that is outside the world or entirely air; the
/// response encodes that as "no data" rather than 4096 copies of air.
fn section_for_client(
    chunk: &Chunk,
    section_y: i32,
    remap: Option<&'static Tables>,
) -> Option<Vec<u16>> {
    // One host call for the whole section. Reading it block by block was 4096
    // calls and made a copy of any size stall the tick.
    let states = chunk.read_section(section_y)?;
    if states.len() != SECTION_VOLUME || states.iter().all(|&id| id == 0) {
        return None;
    }
    Some(
        states
            .into_iter()
            // An id the client has no name for is sent as air; showing the player
            // a wrong block would be worse than showing a gap.
            .map(|id| client_state(remap, id).unwrap_or(0))
            .collect(),
    )
}

/// Looks up the entities a packet named by UUID, in one sweep of the world.
///
/// The plugin API has no lookup by UUID, so the alternative would be a full sweep
/// per requested entity.
fn entities_by_uuid(world: &World, wanted: &[(u64, u64)]) -> Vec<(usize, Entity)> {
    let mut found = Vec::new();
    for entity in world.get_entities() {
        let id = entity.get_uuid();
        if let Some(index) = wanted.iter().position(|&w| w == (id.high, id.low)) {
            found.push((index, entity));
        }
    }
    found
}

fn read_uuid_list(r: &mut Reader<'_>) -> Result<Vec<(u64, u64)>, String> {
    let err = |e: crate::buf::Error| e.to_string();
    let count = r.var_len("uuid list", MAX_COLLECTION).map_err(err)?;
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        list.push(r.uuid().map_err(err)?);
    }
    Ok(list)
}

/// `axiom:delete_entity`
pub fn delete_entity(player: &Player, body: &[u8]) -> Result<(), String> {
    let wanted = read_uuid_list(&mut Reader::new(body))?;
    if !has_perm(player, permissions::ENTITY_DELETE) {
        return Ok(());
    }
    let world = player.get_world();
    let mut removed = 0_usize;
    for (_, entity) in entities_by_uuid(&world, &wanted) {
        // Removing a player, or a vehicle carrying one, is never what the tool means.
        if entity.get_type() == EntityType::Player {
            continue;
        }
        entity.remove();
        removed += 1;
    }
    tracing::debug!("Removed {removed} entities for {}", player.get_name());
    Ok(())
}

/// `axiom:request_entity_data` — the client asking for the NBT of entities it
/// wants to edit.
pub fn request_entity_data(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let id = r.i64().map_err(|e| e.to_string())?;

    // As with chunk data, a refusal still has to be answered or the client waits.
    if !has_perm(player, permissions::ENTITY_REQUESTDATA) {
        send_entity_data(player, id, true, &[]);
        return Ok(());
    }
    let wanted = read_uuid_list(&mut r)?;
    let world = player.get_world();

    let mut batch: Vec<(u64, u64, Vec<u8>)> = Vec::new();
    let mut batch_bytes = 0_usize;
    for (index, entity) in entities_by_uuid(&world, &wanted) {
        if entity.get_type() == EntityType::Player {
            continue;
        }
        let nbt = entity.get_nbt();
        if nbt.len() >= MAX_RESPONSE_BYTES {
            // Too big to share a payload with anything else.
            send_entity_data(player, id, false, &[(wanted[index].0, wanted[index].1, nbt)]);
            continue;
        }
        if batch_bytes + nbt.len() > MAX_RESPONSE_BYTES {
            send_entity_data(player, id, false, &batch);
            batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += nbt.len();
        batch.push((wanted[index].0, wanted[index].1, nbt));
    }
    send_entity_data(player, id, true, &batch);
    Ok(())
}

fn send_entity_data(player: &Player, id: i64, finished: bool, entries: &[(u64, u64, Vec<u8>)]) {
    let mut w = Writer::new();
    w.i64(id).bool(finished).var_i32(entries.len() as i32);
    for (high, low, nbt) in entries {
        w.uuid(*high, *low).bytes(nbt);
    }
    send(player, "axiom:response_entity_data", &w.into_vec());
}
