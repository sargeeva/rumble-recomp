# The hub's multiplayer pedestal is Ditto

## Local multiplayer

With `[game] multiplayer = "local"` (**Esc → Game → Multiplayer: Local**), the game keeps its own pedestal and pause menu entry, described below, and extra gamepads play as Wii Remotes 2–4. The keyboard is remote 1. Each gamepad plays as its place in the list of connected pads unless **Controls → Players** sets otherwise (`[controls] pad_players`). Remotes 2–4 count as connected while a gamepad plays as them: the Wii Remote replacements (`hle/wpad.rs`) already answer for every channel, and the game polls them, so a pad that connects mid-game can join at the pedestal's player select. It takes effect when the game restarts, since the pedestal is chosen as the hub's files are unpacked.

Recorded sessions (`build/remote/sessions`) record remote 1 only.

## With multiplayer off

In the hub, the pedestal that starts it (two figures on a round base, with a "Multiplayer" balloon) is replaced by the game's own Ditto, idling. Talking to Ditto shows:

> Sargeeva: Multiplayer has not been implemented yet.

The first time on a save, Ditto also drops a silver coin (10 P). That is remembered in the save, so it happens once per save.

Nothing new ships. Ditto's model, textures and animation, the message box and the coin are the game's own, taken from the user's files as the game unpacks them. The code is `rumble-runtime/src/hle/multiplay.rs`, with `rumble-formats/src/brres.rs` for the model file.

The pause menu's Multiplayer entry is left out too.

## Bringing back the original

Local multiplayer restores the original pedestal, its "Start multiplayer?" prompt and the pause menu's Multiplayer entry. So does the hidden setting `[game] original_multiplayer` in `config.toml`, with multiplayer off. The menu doesn't show it, and it is only written to the file when it is on. It is read when the game starts:

```toml
[game]
original_multiplayer = true
```

`boot` honours it as well.

## How the pedestal works

The hub objects are scripts in `script.arc`, bytecode for the game's script engine (`NPpu`, files starting `pk\x1a `). The pedestal is `CItem_multi.pkc`. Like the other hub objects, it is a shared library of functions followed by its own part, which:

1. loads its model, `item.arc/terminal_multiplay.brres`, with `method_loadModel`;
2. registers a callback that notes which player touched it, and the "Multiplayer" balloon (message `0x70075`);
3. stays hidden until the hub shows it (save flag 4), then plays `terminal_multiplay__fall`;
4. loops: when a player touches it, it opens a yes/no prompt (message `0x70076`). Yes calls `ShowMultiPlayerEntrance`; no calls `Resume`.

Script facts this relies on (from `rumble-port disasm` and `build/recomp/main.s`):

- **File layout.** The header holds the magic, version, code size, the wide-string pool's size and count, a word the loader doesn't read, and the narrow-string count. Then come the code (big-endian words), the UTF-16 strings and the ASCII strings. The loader is at `0x8016fcd0` in the Weekend DOL.
- **Instructions.** Each is an 8-bit opcode with a 24-bit operand. The interpreter is at `0x80172980`, with a switch over 78 opcodes.
  - `0x1b` calls a native by index; `0x28` does too, with the argument count in the next word.
  - `0x1a` calls a script function; `0x1c` jumps; `0x1d` jumps if false; `0x1e` returns; `0x1f` pops.
  - `0x19` loads and `0x0a` stores (operand bit 16 set for a local, clear for a global).
  - `0x40` pushes an integer and `0x41` a float (from the next word); `0x42` pushes a string and `0x43` a wide string; `0x44` pushes a function reference.
  - `0x27` pushes the object itself; `0x20` starts a function, followed by a word of argument and local counts.
- **Natives.** They live in a table of (function, name, argument count) at `0x80352eb8` in the Weekend DOL. Entry 0 is empty, and a script's native index is the table index. The patch finds the table by its first name, `NewStageCamera`, and looks natives up by name. So it doesn't depend on addresses or indices, which can differ between builds.
- **Save flags.** `SetFlag`/`CheckFlag` set and test bits in the current save slot: 64 flags at slot + 8, with the next field at slot + 0x10. The game and its scripts use flags up to 31. The coin uses flag 40.
- **Coins.** Items drop money as a `CItem_money.pkc` item made with `NewItem`. Object register 11 holds the value and 12 and 13 hold its speeds. A value of 10–99 is the silver coin (`poke_energy_3`).

## The change

Both files are changed in the archive in RAM as the game unpacks it. `CXSecureUncompressLH` is already replaced natively (`hle/cx.rs`), and it hands every unpacked archive to `multiplay::unpacked` before the data reaches the game. Since this changes data and not code, the interpreter, the Rust blocks and the Cranelift JIT all run it the same way.

**The model.** `terminal_multiplay.brres` in `item.arc` is overwritten with a Ditto model file. It is built from `pii_1-151_387-492.arc`:

- `METAMON.brres`: the `METAMON_slime` model, named `terminal_multiplay`;
- from `textures_1-151_387-492.brres`: the textures it names (`Pii_METAMON_eye`, `high_light`, `light_tex`) and the eye's palette;
- from `anim.brres`: its skeleton type's idle animation, `slime_anim_kamae`, named `terminal_multiplay__wait`.

`brres::Builder` lays the file out, and its dictionaries are built as Nintendo's tools build them. The builder reproduces all 1,879 dictionaries in the game's files exactly. If `item.arc` is unpacked before the Pokemon archive, its place in RAM is remembered, and the model is written there once Ditto is known.

**The script.** `CItem_multi.pkc` is rewritten in place. It must fit in the original's space in `script.arc`:

- Steps 1 and 2 are kept unchanged. The model they load is now Ditto's.
- The main part is new. It waits hidden for flag 4 as before, then plays `terminal_multiplay__wait` (the `fall` string becomes `wait`) and loops on the interaction.
- The interaction is placed where a library function nothing calls was: one that a jump skips over and that nothing calls or references. It:
  - shows the message with `PlayerMessageOpen` and waits for `MessageIsClosed`;
  - if flag 40 is clear, sets it and drops a 10 P coin at Ditto;
  - calls `Resume`.
- The message is a new wide string.

Every step checks what it expects: the strings, the natives, the calls in the original main part, and the room. If any check fails, the file stays as it was and a line is printed. The plain game and Rumble Weekend go through the same code; it was tested on Weekend.

**The pause menu.** The pause menu is native code (`CPauseMenu`, at `0x800d9110` in the Weekend DOL). It builds its list from `main.bmg` messages: Continue `0x80`, To the Terminal `0x81`, Go to Title Menu `0x82`, Take a Picture `0x83`, SD Card `0x84`, Multiplayer `0x86`. It adds Multiplayer only in the hub, and only once save flag 3 is set: `li r3, 3; bl CheckFlag; cmpwi r3, 0; beq skip`, with `addi r3, r3, 0x86` a few instructions on. `multiplay::configure` finds that sequence in RAM at boot and turns the `beq` into an unconditional branch, so the entry is never added. This happens before code is compiled or translated, and blocks whose code differs from RAM are interpreted, so all three backends run it. The menu widget (`0x801dd850`) has no greyed-out or disabled item, so the entry is left out rather than greyed.

Recorded sessions (`boot --replay`) from before this change still play the same until the hub's pedestal is involved.

## Other changes made the same way

These also live in `rumble-runtime/src/hle/`, alongside the pedestal.

### Befriended Shinies on Play Info (Rumble Weekend)

The Information podium (`script.arc/CItem_information.pkc`) shows a fourth line on its Play Info page: "Befriended Shinies", under Befriended Pokémon. `hle/playinfo.rs` does this, only when Rumble Weekend runs. Weekend changes one thing in the DOL: a loop over the Pokédex stops at 493 instead of 150 (`cmplwi r0, 0x1ed` at `0x80079bf4`), and that is how it's recognised.

- **The count.** The collection keeps a 0x1c-byte record per species. `+0` is how far it got (1 seen, 2 defeated, 3 befriended) and `+0x18` has a bit per form for "befriended as a shiny" (`fn_800e1b50` sets it). The line shows how many of those bits are set: shiny species and forms befriended, as the collection screen shows them.
- **Getting the count to the script.** No native gives scripts that word. So the line asks `GetStatValue` for id -1000, which the game doesn't use. `GetStatValue` is replaced at the address the native table gives; its signature also matches another native. The replacement answers -1000 itself and runs the game's function for every other id. It reaches the collection the way the game does: through `GetStatValue`'s call to the save data getter (`lwz r3, d(r13)`), then that object's virtual function 0x28 (`lwz r3, 0x20(r3)`). Both are checked before they're used.
- **The script.**
  - The page is three `DlgInfoSetLine(page, row, label, value, suffix)` calls. The Befriended Pokémon call becomes a call to a new function that draws that line and the new one.
  - The new function goes where a function nothing calls was. The rest of that space is removed, and every code address after it is moved down, to make room for the label.
  - Opcodes `0x1a`, `0x1c`, `0x1d`, `0x24`–`0x26` and `0x44` carry code addresses. Switch tables (`0x4b`: pairs, one word each; `0x4c`: a halfword per case after the default, so ⌈count/2⌉ words) hold offsets relative to themselves. A removal they would cross is refused.

The podium's script is the same in the plain game and in Weekend.

### "Recomped" on the title screen

The title screen's copyright (`title2d.arc/arc/blyt/title_main.brlyt`, text box `copy`) ends with a line "Recomped 10/30/2026". `hle/title.rs` does this in two steps, because the text can't grow in the archive:

1. As the archive is unpacked, the text box's buffer size in its `txt1` block is raised. That's one field, so nothing moves.
2. When the box first draws (the `Pane::Draw` hook in `lyt.rs`), the line is added to the text in the game's buffer, and the text box's length is updated. Both are found in the `TextBox` by their values.

The box keeps its text centred, so the copyright moves up by a line.
