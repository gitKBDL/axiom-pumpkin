//! Handshake state machine and packet dispatch.

use std::sync::atomic::{AtomicU64, Ordering};

use pumpkin_plugin_api::{Server, permission::PermissionLevel, player::Player};

use crate::buf::{Reader, Writer};
use crate::permissions::{self, PERMS};
use crate::tunnel::{self, Tunnel};
use crate::{API_VERSION, SUPPORTED_PACKETS, key_of, kick, send, with_players};

pub type PlayerKey = (u64, u64);

/// Mirrors the `infinite-reach-limit` config key.
const INFINITE_REACH_LIMIT: i32 = 256;

/// Chunk sections a player may push per second, mirroring the default
/// `block-buffer-rate-limit`. The client spends from this budget for every block
/// buffer it sends and stops sending once it runs out, so the server has to keep
/// topping it up — without that, only the first buffer of an edit ever arrives.
const DISPATCH_SENDS_PER_SECOND: i32 = 1024;

static TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct PlayerState {
    pub tunnel: Tunnel,
    /// Set once the client has completed the handshake.
    pub active: bool,
    /// Token we offered in `axiom:hello` and are waiting to see echoed back.
    pub pending_handshake: Option<i64>,
    /// Whether we already told this player why Axiom is unavailable, so the
    /// message is not repeated every second.
    pub told_disabled: bool,
    pub restrictions: Option<Restrictions>,
    /// Remaining block-buffer budget, in twentieths of a section, as upstream
    /// tracks it. `None` until the first top-up is sent.
    pub dispatch_sends_20: Option<i32>,
    /// Whether the player asked not to trigger pressure plates and the like.
    pub no_physical_trigger: bool,
}

#[derive(PartialEq, Eq)]
pub struct Restrictions {
    /// Wire names of granted permissions.
    pub allowed: Vec<&'static str>,
    pub denied: Vec<&'static str>,
    pub infinite_reach_limit: i32,
}

impl Restrictions {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.var_i32(self.allowed.len() as i32);
        for node in &self.allowed {
            w.string(node);
        }
        w.var_i32(self.denied.len() as i32);
        for node in &self.denied {
            w.string(node);
        }
        w.i32(self.infinite_reach_limit);
        // Region bounds come from PlotSquared/WorldGuard upstream; neither exists
        // for Pumpkin, so the client is never fenced in.
        w.var_i32(0);
        w.into_vec()
    }
}

/// Recomputes what the player is allowed to do. Ops (and anyone holding
/// `axiom.all`) short-circuit to the catch-all node, exactly as upstream does.
fn calculate_restrictions(player: &Player) -> Restrictions {
    let is_op = matches!(player.get_permission_level(), PermissionLevel::Four);
    if is_op || player.has_permission(PERMS[permissions::ALL].node) {
        return Restrictions {
            allowed: vec![PERMS[permissions::ALL].wire],
            denied: Vec::new(),
            infinite_reach_limit: INFINITE_REACH_LIMIT,
        };
    }

    let set: Vec<Option<bool>> = PERMS
        .iter()
        .map(|p| player.has_permission_set(p.node))
        .collect();

    // A node whose parent carries the same verdict is redundant on the wire.
    let redundant = |i: usize, want: bool| {
        PERMS[i]
            .parent
            .is_some_and(|parent| set[parent] == Some(want))
    };

    let collect = |want: bool| {
        (0..PERMS.len())
            .filter(|&i| set[i] == Some(want) && !redundant(i, want))
            .map(|i| PERMS[i].wire)
            .collect()
    };

    Restrictions {
        allowed: collect(true),
        denied: collect(false),
        infinite_reach_limit: INFINITE_REACH_LIMIT,
    }
}

/// Whether the player holds one specific Axiom permission. Ops and holders of
/// `axiom.all` pass everything, mirroring upstream's short-circuit.
pub fn has_perm(player: &Player, index: usize) -> bool {
    matches!(player.get_permission_level(), PermissionLevel::Four)
        || player.has_permission(PERMS[permissions::ALL].node)
        || player.has_permission(PERMS[index].node)
}

fn can_use_axiom(player: &Player) -> bool {
    matches!(player.get_permission_level(), PermissionLevel::Four)
        || player.has_permission(PERMS[permissions::ALL].node)
        || player.has_permission(PERMS[permissions::USE].node)
}

/// Runs every tick: tops up each active player's buffer budget. Handshake offers
/// and restriction refreshes only need checking once a second.
pub fn tick(server: Server) {
    let slow = TICKS.fetch_add(1, Ordering::Relaxed).is_multiple_of(20);
    let players = server.get_all_players();

    with_players(|states| {
        for player in &players {
            if !slow {
                let key = key_of(&player.get_id());
                if let Some(state) = states.get_mut(&key)
                    && state.active
                {
                    top_up_dispatch_sends(player, state);
                }
                continue;
            }
            let key = key_of(&player.get_id());
            let state = states.entry(key).or_default();

            if state.active {
                if can_use_axiom(player) {
                    top_up_dispatch_sends(player, state);
                    update_restrictions(player, state);
                } else {
                    // Tell the client to shut Axiom down, then start over.
                    send(player, "axiom:enable", &[0]);
                    *state = PlayerState::default();
                }
                continue;
            }

            if can_use_axiom(player) {
                if state.pending_handshake.is_none() {
                    let token = new_handshake_token();
                    state.pending_handshake = Some(token);
                    state.told_disabled = false;

                    let mut w = Writer::new();
                    w.i64(token);
                    send(player, "axiom:hello", &w.into_vec());
                    tracing::info!("Offered Axiom handshake to {}", player.get_name());
                }
            } else if !state.told_disabled {
                state.told_disabled = true;
                state.pending_handshake = None;
                send_goodbye(
                    player,
                    "You don't have permission to use Axiom. Give yourself /op or the axiom.default permission",
                );
            }
        }

        if slow {
            // Drop state for players who are no longer online.
            let online: Vec<PlayerKey> = players.iter().map(|p| key_of(&p.get_id())).collect();
            states.retain(|key, _| online.contains(key));
        }
    });
}

/// Grants the player another tick's worth of buffer budget and tells the client
/// whenever the whole-section total has moved.
fn top_up_dispatch_sends(player: &Player, state: &mut PlayerState) {
    let allowed = DISPATCH_SENDS_PER_SECOND;
    match state.dispatch_sends_20 {
        None => {
            state.dispatch_sends_20 = Some(allowed * 20);
            send_dispatch_update(player, allowed, allowed);
        }
        Some(previous) => {
            let updated = (previous + allowed).min(allowed * 20);
            state.dispatch_sends_20 = Some(updated);
            if previous / 20 != updated / 20 {
                send_dispatch_update(player, updated / 20 - previous / 20, allowed);
            }
        }
    }
}

fn send_dispatch_update(player: &Player, add: i32, max: i32) {
    let mut w = Writer::new();
    w.var_i32(add).var_i32(max);
    send(player, "axiom:update_available_dispatch_sends", &w.into_vec());
}

/// Charges a block buffer against the player's budget and re-syncs with what the
/// client believes it has left. Returns false when the player is over the limit.
pub fn consume_dispatch_sends(player: &Player, sections: i32, client_available: i32) -> bool {
    let allowed = DISPATCH_SENDS_PER_SECOND;
    let key = key_of(&player.get_id());
    with_players(|states| {
        let state = states.entry(key).or_default();
        let mut current = state.dispatch_sends_20.unwrap_or(allowed * 20);
        current = current.saturating_sub(sections.saturating_mul(20));
        current = current.min(client_available.saturating_mul(20));
        state.dispatch_sends_20 = Some(current);
        current >= -(allowed * 20)
    })
}

fn update_restrictions(player: &Player, state: &mut PlayerState) {
    let restrictions = calculate_restrictions(player);
    if state.restrictions.as_ref() == Some(&restrictions) {
        return;
    }
    send(player, "axiom:restrictions", &restrictions.encode());
    state.restrictions = Some(restrictions);
}

/// A non-zero token; zero is reserved as "no handshake pending" upstream.
fn new_handshake_token() -> i64 {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // WASI always provides randomness; if it ever did not, a fixed token still
        // keeps the handshake functional (it only guards against a client claiming
        // to be enabled without being offered it).
        bytes = 1_u64.to_ne_bytes();
    }
    match i64::from_ne_bytes(bytes) {
        0 => 1,
        token => token,
    }
}

pub fn set_no_physical_trigger(player: &Player, enabled: bool) {
    let key = key_of(&player.get_id());
    with_players(|states| states.entry(key).or_default().no_physical_trigger = enabled);
}

fn is_active(player: &Player) -> bool {
    let key = key_of(&player.get_id());
    with_players(|states| states.get(&key).is_some_and(|s| s.active))
}

pub fn send_goodbye(player: &Player, reason: &str) {
    let mut w = Writer::new();
    w.string(reason);
    send(player, "axiom:goodbye", &w.into_vec());
}

/// Routes one inbound `axiom:*` plugin message.
pub fn on_payload(server: &Server, player: &Player, channel: &str, data: &[u8]) {
    if channel == "axiom:tunnel" {
        let key = key_of(&player.get_id());
        let packet = with_players(|states| {
            let state = states.entry(key).or_default();
            if !state.active {
                return Ok(None);
            }
            state.tunnel.push(data)
        });
        match packet {
            Ok(Some(packet)) => dispatch(server, player, &packet.id, &packet.body),
            Ok(None) => {}
            Err(e) => kick(player, &format!("Axiom: {e}")),
        }
        return;
    }
    dispatch(server, player, channel, data);
}

fn dispatch(server: &Server, player: &Player, id: &str, body: &[u8]) {
    if id != "axiom:hello" && !is_active(player) {
        return;
    }
    let result = match id {
        "axiom:hello" => handle_hello(player, body),
        "axiom:set_block" => crate::handlers::set_block(player, body),
        "axiom:set_buffer" => crate::handlers::set_buffer(player, body),
        "axiom:set_gamemode" => crate::handlers::set_gamemode(player, body),
        "axiom:set_fly_speed" => crate::handlers::set_fly_speed(player, body),
        "axiom:teleport" => crate::handlers::teleport(server, player, body),
        "axiom:set_world_time" => crate::handlers::set_world_time(player, body),
        "axiom:set_no_physical_trigger" => crate::handlers::set_no_physical_trigger(player, body),
        "axiom:request_chunk_data" => crate::handlers::request_chunk_data(player, body),
        // Unknown ids arrive whenever the client runs ahead of what this port
        // implements. Upstream kicks; ignoring is friendlier and equally safe,
        // because nothing was applied.
        _ => {
            tracing::debug!("Ignoring unimplemented Axiom packet {id} ({} bytes)", body.len());
            return;
        }
    };
    if let Err(reason) = result {
        kick(player, &format!("Axiom: error while processing {id}: {reason}"));
    }
}

fn handle_hello(player: &Player, body: &[u8]) -> Result<(), String> {
    let mut r = Reader::new(body);
    let api_version = r.var_i32().map_err(|e| e.to_string())?;
    let _data_version = r.var_i32().map_err(|e| e.to_string())?;
    // Pumpkin rejects mismatched protocol versions at login and has no ViaVersion
    // equivalent, so a connected client always matches the server here.
    let _protocol_version = r.var_i32().map_err(|e| e.to_string())?;
    let token = r.i64().map_err(|e| e.to_string())?;
    if !r.is_empty() {
        tracing::warn!("axiom:hello had {} unread trailing bytes", r.remaining());
    }

    if api_version != API_VERSION {
        let versions = format!(" (C={api_version} S={API_VERSION})");
        let message = if api_version < API_VERSION {
            format!("Unable to use Axiom, you're on an outdated version! Please update to the latest version of Axiom to use it on this server.{versions}")
        } else {
            format!("Unable to use Axiom, server hasn't updated Axiom yet.{versions}")
        };
        send_goodbye(player, &message);
        kick(player, &message);
        return Ok(());
    }

    let key = key_of(&player.get_id());
    let accepted = with_players(|states| {
        let state = states.entry(key).or_default();
        if state.pending_handshake == Some(token) {
            state.pending_handshake = None;
            true
        } else {
            false
        }
    });

    if !accepted {
        tracing::warn!("{} sent an unexpected handshake token", player.get_name());
        send_goodbye(player, "Invalid handshake ID");
        return Ok(());
    }
    if !can_use_axiom(player) {
        send_goodbye(player, "Missing axiom.use permission");
        return Ok(());
    }

    activate(player);
    Ok(())
}

fn activate(player: &Player) {
    let mut w = Writer::new();
    w.bool(true)
        .u8(0) // ServerConfig version
        .var_i32(2) // Blueprint version
        .i32(tunnel::MAX_FRAME_LEN as i32)
        .i32(tunnel::MAX_PACKET_LEN as i32)
        .var_i32(0) // blockWithCustomData
        .var_i32(0) // ignoreRotationSet
        .var_i32(SUPPORTED_PACKETS.len() as i32);
    for id in SUPPORTED_PACKETS {
        w.string(id);
    }
    send(player, "axiom:enable", &w.into_vec());

    let key = key_of(&player.get_id());
    with_players(|states| {
        let state = states.entry(key).or_default();
        state.active = true;
        state.pending_handshake = None;
        state.restrictions = None;
        state.tunnel.reset();
    });

    // The client expects its restrictions immediately after being enabled.
    with_players(|states| {
        if let Some(state) = states.get_mut(&key) {
            update_restrictions(player, state);
        }
    });

    // No world properties are registered yet; the client needs the empty list to
    // finish opening its editor UI.
    send(player, "axiom:register_world_properties", &[0]);

    let translated = crate::handlers::describe_client_registry(player);
    tracing::info!("Axiom enabled for {} ({translated})", player.get_name());
}
