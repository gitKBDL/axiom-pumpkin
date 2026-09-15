//! Serverside component for Axiom, ported from `AxiomPaperPlugin` to PumpkinMC.
//!
//! Axiom multiplexes its protocol over `axiom:*` plugin-message channels. The
//! handshake is driven by the server: once a second we offer `axiom:hello` with a
//! random token to every player allowed to use Axiom, and the client echoes the
//! token back along with its API version. A matching echo enables the client.

mod block_remap;
mod buf;
mod handlers;
mod nbt;
mod palette;
mod permissions;
mod proto;
mod tunnel;

use std::collections::HashMap;
use std::sync::Mutex;

use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    events::{EventHandler, EventPriority, PlayerCustomPayloadEvent},
    events_wit::PlayerCustomPayloadEventData,
    permission::{Permission, PermissionDefault, PermissionLevel},
    player::{JavaKickOptions, Player},
    register_plugin,
    scheduler::SchedulerExt,
    text::TextComponent,
    uuid::Uuid,
};

use proto::{PlayerState, PlayerKey};

/// Axiom's protocol revision. The client refuses to talk to a server that reports
/// a different one, so this is bumped in lockstep with the upstream plugin.
pub const API_VERSION: i32 = 10;

/// Serverbound packets this port understands. Sent verbatim in `axiom:enable`; the
/// client disables any feature whose packet is missing from the list, so a name is
/// added here only once its handler exists.
pub const SUPPORTED_PACKETS: &[&str] = &["axiom:tunnel", "axiom:hello", "axiom:set_block", "axiom:set_buffer"];

static PLAYERS: Mutex<Option<HashMap<PlayerKey, PlayerState>>> = Mutex::new(None);

/// Runs `f` with the player table, recovering rather than propagating a poisoned
/// lock — a panic in one handler must not take Axiom down for everyone.
pub fn with_players<R>(f: impl FnOnce(&mut HashMap<PlayerKey, PlayerState>) -> R) -> R {
    let mut guard = PLAYERS.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

#[must_use]
pub const fn key_of(id: &Uuid) -> PlayerKey {
    (id.high, id.low)
}

/// Sends one `axiom:<channel>` plugin message. Bedrock players have no Axiom client,
/// so they are silently skipped.
pub fn send(player: &Player, channel: &str, data: &[u8]) {
    if let Some(java) = player.as_java() {
        java.send_custom_payload(channel, data);
    }
}

pub fn kick(player: &Player, reason: &str) {
    if let Some(java) = player.as_java() {
        java.kick(JavaKickOptions::new(TextComponent::text(reason)));
    }
}

struct AxiomPlugin;

impl Plugin for AxiomPlugin {
    fn new() -> Self {
        Self
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "axiom".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["Moulberry (original Paper plugin)".into()],
            description: "Serverside component for Axiom".into(),
            dependencies: vec![],
            permissions: vec![],
        }
    }

    fn on_load(&self, context: Context) -> Result<()> {
        register_permissions(&context);

        context.register_event_handler::<PlayerCustomPayloadEvent, _>(
            PayloadHandler,
            EventPriority::Normal,
            true,
        )?;

        // Upstream drives the handshake from a 20-tick timer rather than the join
        // event, so a player who gains permission mid-session is picked up too.
        context.schedule_repeating_task(1, 1, proto::tick);

        tracing::info!("Axiom ready (protocol API version {API_VERSION})");
        Ok(())
    }

    fn on_unload(&self, _context: Context) -> Result<()> {
        with_players(HashMap::clear);
        Ok(())
    }
}

/// Registers every Axiom node so operators can grant them individually. `axiom.all`
/// defaults to ops, which is what makes the plugin work out of the box for an
/// opped player — the same contract the Paper plugin documents.
fn register_permissions(context: &Context) {
    for perm in &permissions::PERMS {
        let default = if perm.node == "axiom.all" {
            PermissionDefault::Op(PermissionLevel::Four)
        } else {
            PermissionDefault::Deny
        };
        let _ = context.register_permission(&Permission {
            node: perm.node.to_owned(),
            description: String::new(),
            default,
            children: Vec::new(),
        });
    }
}

struct PayloadHandler;

impl EventHandler<PlayerCustomPayloadEvent> for PayloadHandler {
    fn handle(
        &self,
        _server: Server,
        event: PlayerCustomPayloadEventData,
    ) -> PlayerCustomPayloadEventData {
        if event.channel.starts_with("axiom:") {
            proto::on_payload(&event.player, &event.channel, &event.data);
        }
        event
    }
}

register_plugin!(AxiomPlugin);
