//! Serverside component for Axiom, ported from `AxiomPaperPlugin` to PumpkinMC.
//!
//! Axiom multiplexes its protocol over `axiom:*` plugin-message channels. The
//! handshake is driven by the server: once a second we offer `axiom:hello` with a
//! random token to every player allowed to use Axiom, and the client echoes the
//! token back along with its API version. A matching echo enables the client.

mod annotations;
#[rustfmt::skip] // generated, see tools/
mod biomes;
#[rustfmt::skip] // generated, see tools/
mod block_remap;
mod buf;
mod handlers;
mod nbt;
mod palette;
#[rustfmt::skip] // generated, see tools/
mod permissions;
mod proto;
mod tunnel;

use std::collections::HashMap;
use std::sync::Mutex;

use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    events::{EventHandler, EventPriority, PlayerCustomPayloadEvent},
    events_wit::PlayerCustomPayloadEventData,
    permission::{Permission, PermissionChild, PermissionDefault, PermissionLevel},
    player::{JavaKickOptions, Player},
    register_plugin,
    scheduler::SchedulerExt,
    text::TextComponent,
    uuid::Uuid,
};

use proto::{PlayerKey, PlayerState};

/// Axiom's protocol revision. The client refuses to talk to a server that reports
/// a different one, so this is bumped in lockstep with the upstream plugin.
pub const API_VERSION: i32 = 10;

/// Serverbound packets this port understands. Sent verbatim in `axiom:enable`; the
/// client disables any feature whose packet is missing from the list, so a name is
/// added here only once its handler exists.
pub const SUPPORTED_PACKETS: &[&str] = &[
    "axiom:tunnel",
    "axiom:hello",
    "axiom:set_block",
    "axiom:set_buffer",
    "axiom:set_gamemode",
    "axiom:set_fly_speed",
    "axiom:teleport",
    "axiom:set_world_time",
    "axiom:set_no_physical_trigger",
    "axiom:request_chunk_data",
    "axiom:spawn_entity",
    "axiom:delete_entity",
    "axiom:request_entity_data",
    "axiom:manipulate_entity",
    "axiom:tick_blocks",
    "axiom:annotation_update",
];

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
///
/// Each node lists everything below it in upstream's tree as a child. Bukkit
/// follows children all the way down, but Pumpkin resolves them one level deep,
/// so `axiom.default` has to name every node it grants itself.
fn register_permissions(context: &Context) {
    for (index, perm) in permissions::PERMS.iter().enumerate() {
        let default = if perm.node == "axiom.all" {
            PermissionDefault::Op(PermissionLevel::Four)
        } else {
            PermissionDefault::Deny
        };
        let children = descendants(index)
            .map(|child| PermissionChild {
                node: permissions::PERMS[child].node.to_owned(),
                value: true,
            })
            .collect();
        let _ = context.register_permission(&Permission {
            node: perm.node.to_owned(),
            description: String::new(),
            default,
            children,
        });
    }
}

/// Every node below `ancestor` in the permission tree, at any depth.
fn descendants(ancestor: usize) -> impl Iterator<Item = usize> {
    (0..permissions::PERMS.len()).filter(move |&node| {
        let mut current = node;
        while let Some(parent) = permissions::PERMS[current].parent {
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    })
}

struct PayloadHandler;

impl EventHandler<PlayerCustomPayloadEvent> for PayloadHandler {
    fn handle(
        &self,
        server: Server,
        event: PlayerCustomPayloadEventData,
    ) -> PlayerCustomPayloadEventData {
        if event.channel.starts_with("axiom:") {
            proto::on_payload(&server, &event.player, &event.channel, &event.data);
        }
        event
    }
}

register_plugin!(AxiomPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(node: &str) -> usize {
        permissions::PERMS
            .iter()
            .position(|perm| perm.node == node)
            .unwrap()
    }

    fn child_nodes(node: &str) -> Vec<&'static str> {
        descendants(index_of(node))
            .map(|child| permissions::PERMS[child].node)
            .collect()
    }

    /// Granting `axiom.default` has to reach the nodes the handlers check, however
    /// deep they sit, since Pumpkin looks only one level down.
    #[test]
    fn default_grants_the_whole_public_set() {
        let granted = child_nodes("axiom.default");
        for node in [
            "axiom.use",
            "axiom.build.place",
            "axiom.chunk.request",
            "axiom.player.gamemode.creative",
        ] {
            assert!(granted.contains(&node), "{node}");
        }
        assert!(
            !granted.contains(&"axiom.entity.spawn"),
            "entities are opt-in"
        );
        assert!(!granted.contains(&"axiom.default"));
    }

    #[test]
    fn groups_grant_their_members() {
        assert_eq!(
            child_nodes("axiom.entity.*"),
            [
                "axiom.entity.spawn",
                "axiom.entity.manipulate",
                "axiom.entity.delete",
                "axiom.entity.request_data",
            ]
        );
        assert!(child_nodes("axiom.use").is_empty());
    }
}
