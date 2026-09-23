# Axiom Pumpkin Plugin

Serverside component for [Axiom](https://modrinth.com/mod/axiom) on [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin), ported from the [Axiom Paper Plugin](https://github.com/Moulberry/AxiomPaperPlugin).

## Download

1. **The plugin:** `axiom.wasm` from the [latest release](https://github.com/gitKBDL/axiom-pumpkin/releases/latest). Put it in the server's `plugins/` folder.
2. **The server:** the plugin needs a few plugin API additions that upstream Pumpkin does not have yet. Until it does, run the [Pumpkin for Axiom](https://github.com/gitKBDL/Pumpkin-Core/releases) build for your platform, made from the [`axiom` branch](https://github.com/gitKBDL/Pumpkin-Core/tree/axiom) of our fork. On a stock Pumpkin server the plugin will not load.

Players need Minecraft 26.3 with Axiom 6.1 or newer.

## FAQ

**Axiom works in singleplayer but not when I connect to a Pumpkin server running this plugin. What gives?**

First, the player must be an op on the server. If the player does not have op permissions, run `/op <playername>`. This player must then disconnect from the server and reconnect.

If you're using an alternative solution for permission management, you must give players the `axiom.default` permission.

**It says Axiom does not support my Minecraft version.**

Pumpkin itself only accepts clients on its own version, 26.3. Older clients can join through [pumpkin-java-multiversion](https://github.com/Pumpkin-MC/pumpkin-java-multiversion), and the plugin translates their block ids, but that plugin cannot yet get them past the configuration phase. Use Minecraft 26.3 for now.

**What works?**

Everything the Paper plugin turns on by default: building (placing, brushes, shapes, pasting, beyond render distance too), block entity data, biome painting, copying, the entity tools, block ticking, shared annotations, and the player and world controls (game mode, fly speed, teleport, time).

Not yet: blueprints, world properties, markers and the custom blocks API, and annotations do not survive a restart. WorldGuard, PlotSquared, CoreProtect and LuckPerms have no Pumpkin counterpart, so there is no integration with them.

## Building

```sh
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

The plugin is `target/wasm32-wasip2/release/axiom_pumpkin.wasm`; rename it to `axiom.wasm` if you like.

Three modules are generated and must not be edited by hand: `src/permissions.rs` from the Paper plugin's permission enum, `src/biomes.rs` from Pumpkin's biome list, and `src/block_remap.rs` with `assets/remap/` from pumpkin-java-multiversion's block remap tables. The scripts that regenerate them are in `tools/`.

## License

MIT, like the Axiom Paper Plugin this is ported from.
