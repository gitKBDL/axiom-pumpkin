# Axiom for PumpkinMC

Serverside component for [Axiom](https://modrinth.com/mod/axiom), ported from
[AxiomPaperPlugin](https://github.com/Moulberry/AxiomPaperPlugin) to
[PumpkinMC](https://github.com/Pumpkin-MC/Pumpkin).

Builds to a WebAssembly component. Drop `axiom.wasm` into the server's
`plugins/` directory.

## Requires a patched server

This needs plugin API additions that are not in upstream Pumpkin yet, on the
`axiom-plugin-api` branch of [gitKBDL/Pumpkin-Core](https://github.com/gitKBDL/Pumpkin-Core):

- `chunk.read-section` — a whole section in one call. Reading one block at a
  time costs a host call per block, which dominates any bulk operation.
- `world.set-biome` — biomes could only be set through the world generator's
  chunk buffer, so a plugin had no way to change one at runtime.
- `entity.get-nbt` / `entity.set-nbt` / `world.spawn-entity-from-nbt` — Axiom's
  entity tools are almost entirely about entity NBT.

The plugin will not load on a stock Pumpkin binary.

## Building

```sh
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/axiom_pumpkin.wasm /path/to/server/plugins/axiom.wasm
```

## Generated sources

Two modules are generated and must not be edited by hand:

- `src/permissions.rs` — Axiom's permission nodes, from the upstream Java enum.
  `tools/gen_permissions.py ../axiom-java-reference > src/permissions.rs`
- `src/biomes.rs` — biome lookup by registry name.
  `tools/gen_biomes.py ../pumpkin-src > src/biomes.rs`

## Client versions

Axiom works for clients on the server's own Minecraft version, 26.3. Pumpkin
lets older clients in only through a multi-version plugin, and their Axiom
packets carry block ids from the older registry, where the same number names
a different block — so Axiom stays off for them rather than corrupting builds.

## Status

Working: the handshake and permission model, the `axiom:tunnel` transport,
individual block placement, section buffers (brushes, shapes, paste), block
entity NBT, biome painting, chunk data requests, entity spawn / delete /
manipulate / data requests, block ticking, shared annotations, and the player
and world controls (game mode, fly speed, teleport, time).

Entity NBT from the client is filtered through the same allow-list upstream
uses, so the entity tools cannot hand out items, health or anything else a
saved entity carries.

Not implemented:

- **Blueprints and world properties.** Both are off by default upstream;
  world properties and the custom block and display APIs exist for other
  plugins to build on, and there are none here yet.
- **Markers.** Niche, and off by default upstream.
- **`set_no_physical_trigger`.** The flag is tracked but cannot be enforced:
  Pumpkin's `interact-action` has no physical variant and its generic game
  event carries no entity, so there is nothing for a plugin to cancel.
- **Annotation persistence.** Annotations are kept in memory and lost on
  restart; upstream stores them in the world's persistent data.

Integrations with no Pumpkin equivalent are out of scope: WorldGuard,
PlotSquared, CoreProtect and LuckPerms. Blueprints from older Minecraft
versions would need DataFixerUpper, which has no Rust equivalent.
