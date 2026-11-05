# Rumble Recomp

A PC port of **Pokémon Rumble** (USA, WiiWare, `WPSE`) in Rust, with optional [Rumble Weekend](https://projectpokemon.org/home/files/file/4256-pokemon-rumble-weekend-edition/) support. It runs **your own** copy of the game: you give it your WAD and your Wii's common key, and it builds the game from those. No game code or data and no common key come with it.

## Playing

1. Download `Rumble-Recomp-v0.1.0.zip` from [Releases](../../releases) and unzip it. It holds a `Rumble Recomp` folder with `Pokemon Rumble Setup.exe`.
2. Run `Pokemon Rumble Setup.exe`, choose your WAD (USA, `WPSE`) and your common key, and tick Rumble Weekend if you want it (setup can fetch its patches for you). Build.
3. Play with the `Pokemon Rumble.exe` (or `Pokemon Rumble Weekend.exe`) that setup puts next to it. Saves stay in `data/<name>/nand` across rebuilds, and the in-game menu can import and export a Wii `data.bin` save.

Windows 10/11, 64-bit. Keyboard, gamepads and remote-held-sideways controls are configurable in the menu (Esc).

## What's different from the Wii

- Runs at any resolution with a widescreen mode, a separate render thread, and a Cranelift translation of the game's code at load time.
- Local multiplayer: with **Esc → Game → Multiplayer: Local**, extra gamepads play as players 2–4 (who plays as whom is under **Controls → Players**), and the hub has the game's own multiplayer pedestal and the pause menu its Multiplayer entry (after a restart). With multiplayer off, the pedestal is Ditto, who says multiplayer is off (and gives a coin once), and the pause menu's entry is hidden. See [docs/multiplayer-ditto.md](docs/multiplayer-ditto.md). Online play is planned.
- With Rumble Weekend, the Information podium's Play Info page also counts befriended shinies.
- With Rumble Weekend, setup offers addons. The first, **Custom sparkling effect**, makes shinies, and the toys they leave, sparkle with yellow and cyan stars at random angles. See [docs/addons.md](docs/addons.md).

## Layout

- `crates/rumble-formats`: BPS patching, WAD parsing/decryption, U8 archive extraction
- `crates/rumble-recomp`: Gekko/Broadway decoder and disassembler, plus function, block and jump-table analysis
- `crates/rumble-port`: CLI, config, mod loader, and display/widescreen logic
- `mods/<id>/mod.toml`: mods. `wad_patches` lists alternative BPS files, and the first one whose source CRC matches the current WAD is applied. Files under `files/` overlay the extracted game filesystem. `build` applies the mods in `[mods] enabled`.
- `config.toml`: created on first run

## Usage

```bash
cargo run --release -- info     # WAD, mods, resolved display
cargo run --release -- mods     # list mods ([x] = enabled)
cargo run --release -- build    # apply mods -> build/game.wad, then extract to build/fs
cargo run --release -- disasm   # analyze build/main.dol -> build/recomp/{main.s,functions.csv,summary.txt}
```

Extraction needs the Wii common key dumped from your own console, saved as `keys/common-key` (16 bytes). Without it, `build` still produces the patched WAD, which also runs in Dolphin.

## The release build and setup app

The build to distribute has no game code in it: it translates the game's code at load time (`--features jit`, no recompiled blocks):

```bash
RUMBLE_GEN=/nonexistent RUSTFLAGS="-C target-feature=+crt-static" cargo build --release -p rumble-port --features jit --target-dir target/dist
```

`+crt-static` links the C runtime into the exe, so it needs only DLLs that come with Windows (no Visual C++ Redistributable). It is no slower: in an ABBA replay it measured 382 and 385 MIPS, against 374 and 379 for the default build.

Ship it as `Pokemon Rumble Setup.exe` on its own. Rumble Weekend's patches don't come with it: when the checkbox is ticked and they're missing, setup offers "Get Rumble Weekend…" (opens its Project Pokemon page) and "Choose the downloaded zip…", which unpacks the `.bps` files into `mods/weekend-edition/`. Opened without arguments, the exe chooses what to do by its own name:

- **`Pokemon Rumble Setup.exe`** opens the setup window. It asks for the user's WAD (USA, `WPSE`) and Wii common key, has a checkbox for Rumble Weekend and a list of extra `.bps` patches (applied in order), and builds with a progress bar. It checks the WAD, the key and each patch before writing anything. It writes the game to `data/<name>/` and copies itself to `<name>.exe`, `name` being `Pokemon Rumble` or `Pokemon Rumble Weekend`. The copy's icon is that version's title logo, taken from the game files it just built (`en_US/title2d.arc`: `logo_fore` with `logo_mask` as alpha); setup's own icon is `crates/rumble-port/assets/setup-icon.png` (original art), built in by `build.rs`. Running it again rebuilds: the game files are replaced and the save (`data/<name>/nand`) is kept. The two builds keep separate saves.
- **`<name>.exe`** plays `data/<name>/`. With no build for its name it opens setup instead.

`rumble-port setup WAD KEY [--weekend] [--weekend-zip ZIP] [--patch P]...` does the same build from the command line. With any arguments, the exe is the usual command-line tool, so `Play.bat` and `cargo run -- run` work as before.

## Display

```toml
[display]
resolution = "native"              # or { width = 2560, height = 1440 }
aspect = "auto"                    # auto | 4:3 | 16:9 | 16:10 | 21:9 | 32:9 | stretch
widescreen_hack = true             # Hor+ FOV widening; false = pillarboxed 4:3
render_scale = 1.0                 # internal resolution multiplier
```

With the widescreen hack on, 3D perspective projections get their x scale multiplied by `(4/3) / aspect`. The HUD stays 4:3 and centered.

## Build output

- `build/game.wad`: the patched WAD (runs in Dolphin)
- `build/contents/`: decrypted WAD contents. Content 1 is the LZ11-compressed main executable, and content 6 is a small loader DOL.
- `build/main.dol`: the game executable, about 3.1 MB of PowerPC code. Code loads at `0x80004000`/`0x80052500`, and execution starts at `0x80004050`.
- `build/fs/`: the raw game filesystem
- `build/assets/`: every `.cx` file decompressed, with U8 archives expanded into folders. The bulk is standard NW4R formats: `.brres` models, `.tpl` textures, `.brlyt`/`.brlan` UI, `.bmg` text, `.brfnt` fonts.

## Status

Done: WAD and BPS handling, extraction, a Gekko interpreter, static recompilation to Rust blocks for development builds and load-time Cranelift translation for releases, IOS/ES/FS, Wii Remote input from keyboard and gamepads, GX to wgpu rendering with widescreen and resolution scaling, AX/DSP audio, save import and export, and the setup app. The details are in [docs/recomp.md](docs/recomp.md).

Not yet: online multiplayer, the aux (reverb) buses and Wii Remote speaker, and a few GX features (indirect textures, TEV swap tables, lines and points).

## License

MIT, see [LICENSE](LICENSE). Pokémon and Pokémon Rumble are trademarks of Nintendo, Creatures and Game Freak; this project is not affiliated with them.
