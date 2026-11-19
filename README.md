# Veinminer & Autosmelt for Horizon

Port of the "Veinmine & Autosmelt Enchantments" datapack (veinminer + smelting) to a Horizon native mod.

## Build and run

Already pinned to Horizon Nightly build `6d765e0` (see `horizon-mod.toml`). If you play a different
build, run `cargo horizon-mod update` first.

1. `cargo horizon-mod build` writes `target/horizon-mod/veinminer.hmod`.
2. Copy it into your game's `horizon-mods/` folder (or run
   `horizon-launcher native-build . --instance "My Instance" --trust-native-code`, which compiles it
   for your instance).
3. Enable it with `mod.veinminer:true` in `horizon-mods.txt`.

## Settings

`config/veinminer-common.toml` is written on first run: `hunger`, `cooldown` (ticks),
`require_enchantment`, `max_vein_size` and one toggle per pickaxe tier. Edits reload while the game runs.

## How it differs from the datapack

- Breaks use the player's own break, so Fortune, Silk Touch, Unbreaking wear and XP are vanilla's.
  Drops are merged and dropped at the ore you mined.
- Settings are a config file, not in-game `/scoreboard` chat buttons.
- `require_enchantment = true` matches the datapack bundle, where veinmining needed the Veinmine
  enchantment. Set it to false to veinmine with any enabled pickaxe.
- Hunger is applied once per veinmine, not once per block.
- Veinmine is capped at `max_vein_size` blocks (default 128); the datapack had no cap.
- Autosmelt is the `veinminer:smelting` enchantment: drops of raw iron, gold and copper (and their raw blocks) are smelted into ingots and blocks, exactly the datapack's six conversions. Fortune counts carry over, and it works with veinmining. It happens in the drop list, so there are no item-entity scans or delays. The fire-extinguish hiss and smoke puff are kept.
