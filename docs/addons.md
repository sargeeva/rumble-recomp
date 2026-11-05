# Addons

Optional changes the player picks in setup. They appear under **Addons** when Rumble Weekend is ticked, and are saved in `config.toml`:

```toml
[addons]
custom_sparkle = true
```

The game reads them when it starts. Like the other changes in `rumble-runtime/src/hle/`, they are made to the game's files in RAM as the game unpacks them, so the interpreter, the Rust blocks and the Cranelift JIT all run them the same way. The code is `hle/addons.rs`.

## Custom sparkling effect (Rumble Weekend)

Shiny Pokémon sparkle with stars in yellow and cyan, at random angles, instead of the orange sparkle.

Shiny enemies and the toys they leave when knocked out both get the stars. Weekend makes shinies guaranteed befriends. An enemy that is one shows the `drop` sparkle (effect 80), from `script.arc/CEnemyPii.pkc` and `CBossPii.pkc`: `self; int 80; int 0; method_newEffectWithRelDir`, right after the boss's version with 81 (`drop_boss`). Guaranteed befriends that aren't shiny keep the orange sparkle. Bosses keep `drop_boss`.

**The effects** (`effect/main.breff`). Two copies of `drop` are made:

- The star texture `EFF_star` replaces `EFF_hikari`. `drop` mirrors its texture (a quarter of a sparkle) into a whole one, but the star is whole, so the copies clamp it, as the game's star emitters do.
- Rotation about the view axis is fully random.
- The colour: `drop` is red ff, blue 00 and green 0x88 ± 0x6f per particle (red to yellow). The copies are ff, e0 ± 10, 00 (yellow) and 00, e0 ± 10, ff (cyan).
- Each emits half as often as `drop`, so together they are as dense.

The copies need effect ids the game doesn't play: 89 (`test`) and 78 (`otamesi_out`). Those emitters are smaller than `drop`, so the copies go in the space of two unused ones (`item_bakuhatu_star_old`, `hit_critical_tmp`). The name table's entries for `test` and `otamesi_out` point at them. At boot, the effect table in the DOL is checked to give those ids (and 79, below) those names; if it doesn't, the addon stays off.

**The floor toy.** A knocked-out enemy leaves a toy you can befriend, `script.arc/CDummy_ball.pkc`. It plays one of effects 84–87 (`item_capsule_01`–`04`; 03 and 04 aren't in `main.breff`) with `load g; int id; int 180; method_newEffectWithAbsDir`. The toy notes whether the Pokémon it came from was shiny in global `0xc` (`method_isRareColor`, stored once).

`item_capsule_01` is an invisible emitter. Its child track (curve type 5) has two keys, each spawning a child by name index: 0 is `item_capsule_star_01` (the sparkles) and 1 is `item_capsule_kage_01` (the ring under the toy). A key is 24 bytes and ends with that index. The ring can't play by itself because it emits for only 15 frames.

So a third copy, of `item_capsule_01`, keeps one key, which names the ring. It goes in the space of `kaifuku_tmp`, under effect 79's name (`otamesi_in`). Each of the toy's four calls becomes `load g; int id; call sparkle; pop; jump past`. `sparkle(target, id)` plays 79, 89 and 78 for a shiny toy, and `id` otherwise. A shiny toy that would have shown `item_capsule_02` gets `item_capsule_01`'s ring, which is a slightly different colour.

**The enemy script.** The `drop` call becomes `call sparkle; pop; jump past`, and `sparkle` goes where a function nothing calls, refers to or jumps into was. It plays effects 89 and 78 if `method_isRareColor(method_getPPD(self))`, and effect 80 otherwise.

### REFF layout used

Offsets are from BrawlCrate's `REFF.cs` and were checked against the game's emitters:

- **Emitter table**, at 0x30: u16 count, then per emitter a u16 name length, the name, and u32 offset (from 0x30) and size.
- **Entry**: u32, u32 `EmitterDesc` size (0x14c), `EmitterDesc` (emission rate f32 at +0x10), then u32 particle block size and the `ParticleParameterDesc`:
  - colour 1 primary RGBA at +0
  - texture wrap at +0x74 (u16; low 4 bits are texture 1's S and T)
  - rotate offset random (percent) at +0x79..0x7b
  - texture names at +0x88: u16 length including the NUL, then the name
- **Animation tracks** follow: u16 particle track count and init count, then pointer and size words for each, then the same for emitter tracks, then the tracks. A track is a 0x20 header (0xac, kind, curve type, enabled components, ...; key, range, random, name and info table sizes at +0x0c..+0x1c) and its tables. In `drop`'s colour track (curve 0, kind 0, green only), the range table holds the green base and its spread.
