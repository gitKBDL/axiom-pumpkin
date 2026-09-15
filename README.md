# Axiom for PumpkinMC

Serverside component for [Axiom](https://modrinth.com/mod/axiom), ported from
[AxiomPaperPlugin](https://github.com/Moulberry/AxiomPaperPlugin) to
[PumpkinMC](https://github.com/Pumpkin-MC/Pumpkin).

Builds to a WebAssembly component and runs on a stock Pumpkin server — drop
`axiom.wasm` into `plugins/`.

## Building

```sh
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/axiom_pumpkin.wasm /path/to/server/plugins/axiom.wasm
```

`pumpkin-plugin-api` is a path dependency on a Pumpkin checkout next to this one;
see `Cargo.toml`.

## Generated sources

Two modules are generated and must not be edited by hand:

- `src/permissions.rs` — Axiom's permission nodes, from the upstream Java enum.
  `tools/gen_permissions.py ../axiom-java-reference > src/permissions.rs`
- `src/block_remap.rs` — block state id translation for clients older than the
  server, inverted from Pumpkin's own remap tables.
  `tools/gen_block_remap.py ../pumpkin-src > src/block_remap.rs`

## Status

Working: handshake and permissions, the `axiom:tunnel` transport, individual
block placement, and section buffers (brushes, shapes, paste). Block ids are
translated when the client is on an older Minecraft version than the server.

Not implemented yet: block entity NBT, biome buffers, chunk data requests,
entities, world properties, blueprints and annotations.

Integrations with no Pumpkin equivalent are out of scope: WorldGuard,
PlotSquared, CoreProtect and LuckPerms.
