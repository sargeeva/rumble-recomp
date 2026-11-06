# Multiplayer in the hub

## Local multiplayer

With `[game] multiplayer = "local"` (**Esc → Game → Multiplayer: Local**, the default), extra gamepads play as Wii Remotes 2–4. The keyboard is remote 1. Each gamepad plays as its place in the list of connected pads unless **Controls → Players** sets otherwise (`[controls] pad_players`). Remotes 2–4 count as connected while a gamepad plays as them: the Wii Remote replacements (`hle/wpad.rs`) already answer for every channel, and the game polls them, so a pad that connects mid-game can join at the pedestal's player select.

Recorded sessions (`build/remote/sessions`) record remote 1 only.

## With multiplayer off

With `[game] multiplayer = "off"` (**Esc → Game → Multiplayer: Off**), only player 1 plays; the hub and pause menu are as the game has them.

## The pedestal

The hub's multiplayer pedestal is the game's own: `CItem_multi.pkc`, with its model `item.arc/terminal_multiplay.brres`. It stays hidden until the hub shows it (save flag 4); touched, it asks "Start multiplayer?" (message `0x70076`), and yes opens the Pokémon selection screen (`ShowMultiPlayerEntrance`), where remotes 2–4 join. Before multiplayer worked, v0.1.0 replaced it with Ditto as a placeholder.

## The pause menu

The pause menu is native code (`CPauseMenu`, at `0x800d9110` in the Weekend DOL). It builds its list from `main.bmg` messages: Continue `0x80`, To the Terminal `0x81`, Go to Title Menu `0x82`, Take a Picture `0x83`, SD Card `0x84`, Multiplayer `0x86`, each with an action the list widget (`0x801dd850`) returns. It adds Multiplayer (action 1) only in the hub, once save flag 3 is set. With players 2–4 in, the menu also gets an **End Multiplayer** entry after Multiplayer (`hle/pause.rs`, see [online.md](online.md#leaving-and-ending-multiplayer)).

## The game's scripts

The hub's objects are scripts in `script.arc`, bytecode for the game's script engine (`NPpu`, files starting `pk\x1a `). The patches below (`hle/playinfo.rs`, `hle/addons.rs`) rely on these facts, from `rumble-port disasm` and `build/recomp/main.s`:

- **File layout.** The header holds the magic, version, code size, the wide-string pool's size and count, a word the loader doesn't read, and the narrow-string count. Then come the code (big-endian words), the UTF-16 strings and the ASCII strings. The loader is at `0x8016fcd0` in the Weekend DOL.
- **Instructions.** Each is an 8-bit opcode with a 24-bit operand. The interpreter is at `0x80172980`, with a switch over 78 opcodes.
  - `0x1b` calls a native by index; `0x28` does too, with the argument count in the next word.
  - `0x1a` calls a script function; `0x1c` jumps; `0x1d` jumps if false; `0x1e` returns; `0x1f` pops.
  - `0x19` loads and `0x0a` stores (operand bit 16 set for a local, clear for a global).
  - `0x40` pushes an integer and `0x41` a float (from the next word); `0x42` pushes a string and `0x43` a wide string; `0x44` pushes a function reference.
  - `0x27` pushes the object itself; `0x20` starts a function, followed by a word of argument and local counts.
- **Natives.** They live in a table of (function, name, argument count) at `0x80352eb8` in the Weekend DOL. Entry 0 is empty, and a script's native index is the table index. The patches find the table by its first name, `NewStageCamera`, and looks natives up by name. So it doesn't depend on addresses or indices, which can differ between builds.
- **Save flags.** `SetFlag`/`CheckFlag` set and test bits in the current save slot: 64 flags at slot + 8, with the next field at slot + 0x10. The game and its scripts use flags up to 31.

## Other changes made the same way

These live in `rumble-runtime/src/hle/` too. Each changes a file in the archive in RAM as the game unpacks it: `CXSecureUncompressLH` is replaced natively (`hle/cx.rs`) and hands every unpacked archive to `multiplay::unpacked` before the data reaches the game, so the interpreter, the Rust blocks and the Cranelift JIT all run it the same way.

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
