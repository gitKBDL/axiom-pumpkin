# Axiom Pumpkin Plugin

Serverside component for [Axiom](https://modrinth.com/mod/axiom) on [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin), ported from the [Axiom Paper Plugin](https://github.com/Moulberry/AxiomPaperPlugin).

## Download

1. **The plugin:** `axiom.wasm` from the [latest release](https://github.com/gitKBDL/axiom-pumpkin/releases/latest). Put it in the server's `plugins/` folder.
2. **The server:** the plugin needs a few plugin API additions that upstream Pumpkin does not have yet ([#3719](https://github.com/Pumpkin-MC/Pumpkin/pull/3719), [#3720](https://github.com/Pumpkin-MC/Pumpkin/pull/3720), [#3721](https://github.com/Pumpkin-MC/Pumpkin/pull/3721)). Until it does, run the [Pumpkin for Axiom](https://github.com/gitKBDL/Pumpkin-Core/releases) build for your platform, made from the [`axiom` branch](https://github.com/gitKBDL/Pumpkin-Core/tree/axiom) of our fork. On a stock Pumpkin server the plugin will not load.

Players need Minecraft 26.3 with Axiom 6.1 or newer. Older versions have to wait for Pumpkin, see the FAQ.

## FAQ

**Axiom works in singleplayer but not when I connect to a Pumpkin server running this plugin. What gives?**

First, the player must be an op on the server. If the player does not have op permissions, run `/op <playername>`. This player must then disconnect from the server and reconnect.

If you're using an alternative solution for permission management, you must give players the `axiom.default` permission.

**Can I play on an older Minecraft version, like 1.21.11?**

Not at the moment. Up to its 26.2 release Pumpkin accepted older clients itself, and the plugin worked with them. Since 26.3 Pumpkin only accepts its own version and leaves older ones to [pumpkin-java-multiversion](https://github.com/Pumpkin-MC/pumpkin-java-multiversion), which cannot get them past the configuration phase until Pumpkin hands it those packets too ([Pumpkin#3354](https://github.com/Pumpkin-MC/Pumpkin/pull/3354)). The plugin still translates block ids for older clients, so they will work again once that lands.

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
