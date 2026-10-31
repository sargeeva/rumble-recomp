# Static recompilation pipeline

This document covers how `build/main.dol` (the game's PowerPC executable, taken from the user's own WAD after mods are applied) becomes native code, and what runtime has to surround it.

## Ground rules

- **Recompile on the user's machine.** `rumble-port build` produces `main.dol`, then the recompiler emits Rust that `cargo` compiles locally. No game code or recompiled output ever ships with the project. Mods that patch the executable also just work: Weekend Edition changes content 1, so its DOL differs from retail, and it simply gets recompiled. A build that is to be distributed instead translates the code at load time with Cranelift (`--jit`, see Codegen), so it needs neither game code nor a toolchain.
- **Keep the hardware boundary low.** The game talks to the Wii through memory-mapped hardware and IOS IPC. Emulating at that boundary (GPU FIFO, IPC ioctls) is more robust than replacing hundreds of SDK functions. SDK functions are replaced only where host behaviour must differ: threads, context switches and exceptions.

## What the executable looks like

These are the results of `rumble-port disasm` on the Weekend Edition 1.5 DOL:

| | |
|---|---|
| Instructions | 789,216 (text0 at `0x80004000`, text1 at `0x80052500`) |
| Entry point | `0x80004050`; small-data bases r2 = `0x804c1c60`, r13 = `0x804bda20` |
| Functions | 13,422 (8,754 found via `bl`, the rest via prologue, padding, tail-call and data-pointer heuristics) |
| Basic blocks | 145,675 |
| Switch tables | 130 resolved |
| Indirect jumps | 228 vtable/function-pointer tail calls, 28 through pointers passed in registers |
| Indirect calls | 4,579 `bctrl` (C++ virtual calls, callbacks) |
| Paired-single instructions | 7,241 |
| Undecodable words in functions | 14, all inside the OS exception-vector block at `0x800046b4` |
| Libraries | RVL SDK (Feb–Jun 2009): OS, GX, VI, AX, DSP, AI, NAND, CNT, WPAD/KPAD, PAD, HBM, SC, ENC, NCD, FA, EXI, SI, DVD. NW4R: G3D, LYT, SND, EF |

## Stages

```
main.dol ─► 1 decode ─► 2 analyze ─► 3 codegen (Rust) ─► cargo ─► game binary
                                         │
                       overrides.toml ───┘      runtime crate: memory, MMIO,
                                                OS/IOS HLE, GX→wgpu, AX→cpal,
                                                input, interpreter fallback
```

### 1. Decode — `rumble-recomp/src/ppc.rs` (done)

This stage covers the full Gekko/Broadway ISA, including the paired-single and quantized load/store instructions. Each instruction becomes an `Op` plus field accessors, so the disassembler, the analysis and the code generator all share one decoder.

### 2. Analyze — `rumble-recomp/src/analysis.rs` (done, first pass)

- **Function seeds:** the entry point, `bl` targets, prologues (`stwu r1,-N(r1)` / `mflr r0`) after a function end, code after zero padding, `b` tail calls that land on a prologue, and code pointers in data (vtables, callbacks). Pointers that turn out to be jump-table entries are excluded.
- **Function extents** run from one seed to the next, minus trailing padding.
- **Jump tables:** the Metrowerks `lis/addi … lwzx; mtctr; bctr` idiom. The scan traces the base register back up to 128 instructions and also handles tables addressed through the r2/r13 small-data bases.
- **Blocks and exits:** each block ends in one of local, return, tail call, switch, indirect tail call, indirect jump, or `rfi`.
- **Still to do:**
  - An `overrides.toml` for manual function splits and jump tables.
  - Register liveness (CR, XER and FPR) so generated code skips flag updates nobody reads.
  - SDK function identification by byte signature, since the DOL has no symbols. This tells the code generator which functions to replace. A first pass exists (`rumble-recomp/src/sigs.rs`, `rumble-port sig <addr>`): a signature is a function's leading instructions with branch targets, address halves and data displacements masked. The runtime uses it to install its replacements (`rumble-runtime/src/hle/`).

### 3. Codegen (next)

#### Where the time goes now

Measured with `boot --steps 600000000 --press a@400` (boot to the title menu) on the release build:

| Configuration | Time |
|---|---|
| Interpreter, `--no-trace` | 5.7 s (105 MIPS) |
| plus draw building, `--capture` | 5.9 s |
| plus function-entry counting (the `boot` default) | about +20% |

Wins taken so far:

- A direct-mapped decode cache: 64K entries keyed by PC, checked against the fetched word, so code that changes is decoded again. +11%.
- A MEM1 fast path for instruction fetch. +17%.
- Decoded textures cached across frames by content hash. Only the source is hashed each frame. This took draw building from about 10% of run time to about 4%.

Tried and dropped:

- The same fast path for data loads and stores. No change.
- Removing the per-instruction copy of the GPR file. About 2%, not worth the unsafe code.

A guest profile sampled at branches is flat. The busiest function takes about 8%, and the top 25 together about 70%: game logic, NW4R and the SDK's register save/restore helpers. High-level replacements of single functions won't buy much more. Recompiling everything will.

Speed in emulated time: the title screen runs at 1.3–2× real time, because the idle-loop skip turns waiting into nothing. Busy scenes run at about real time. Heavier gameplay will not keep up until codegen lands. `rumble-port run` now holds emulation to real time and shows the speed it reaches in the title bar.

#### Design

Codegen works per basic block, not as functions on the host stack. `rumble-port recomp` compiles each guest function into one host function:

```rust
fn f80052500(c: &mut Cpu, m: &mut Memory) -> Result<(), Fault> {
    loop {
        match c.pc {
            0x80052500 => { if c.budget < 6 { return Ok(()); } /* 6 ops */ c.budget -= 6; }
            0x80052518 => { /* ... */ }
            _ => return Ok(()), // left the function
        }
        if m.mmio_touched { return Ok(()); }
    }
}
```

It chains from block to block inside the function and leaves the next PC in `c.pc`.

`Cpu::run` is the dispatcher. At each PC it looks up the block table (`rumble-runtime/src/cpu/blocks.rs`). If a block starts there, the dispatcher sets an instruction budget and calls that block's function; otherwise it interprets one instruction. After either, it does the timebase, idle-loop and interrupt handling.

**Why not host calls.** Guest calls as host calls (`bl` → Rust call) break whenever the game does something only a real CPU allows:

- The OS switches threads by loading another context and returning through `rfi`, onto a different stack, in the middle of another function.
- Interrupts arrive between any two instructions and return the same way.

With the dispatcher, none of this needs special handling:

- `bl` writes LR and returns the target.
- `blr` returns LR.
- `rfi` returns SRR0.

The OS's own context-switch code then runs as compiled, as it does in the interpreter now.

**The block table.** It is dense, with one entry per word of MEM1 code, each holding the host function to call. The blocks:

- **Where blocks start.** A block starts at every function and basic-block start the analysis found, and after every branch, so a call's return lands on one.
- **Where they stop.** A block runs to a branch (included), an instruction `ops` has no function for, the next block's start, or 64 instructions.
- **Coverage.** That gives about 193K blocks covering 97% of `main.dol`'s instructions.

**Chaining.** Calls, returns and jumps out of a function go back through the dispatcher; everything inside a function stays in its `loop`. The budget is what keeps this safe:

- **The budget.** It is the number of instructions until the next device event or decrementer expiry (`Memory::next_event`, `Cpu::dec_fire`), clamped to 64–65536. Nothing the dispatcher handles can come due before then.
- **Where the chain stops early:**
  - **A block that doesn't fit the budget** hands back before it runs.
  - **A hardware register access** (`Memory::mmio_touched`) may have raised an interrupt.
  - **`mtmsr`, `rfi`, `sc`** change the interrupt enable.
  - **Timebase reads, DEC reads and writes, and locked-cache DMA** need the dispatcher's timebase.
  - **An OS-style idle loop** sets `Cpu::idle_branch`, so the idle skip still works.

**Shared semantics (done).** Every instruction lives in `rumble-runtime/src/cpu/ops.rs`, as one small function per instruction or per family of instructions of one shape:

```rust
pub fn rlwinm(c: &mut Cpu, m: &mut Memory, i: &Insn) -> Result<(), Fault>
```

- **One table.** The `ops!` table maps opcodes to these functions. It generates `ops::exec`, the interpreter's dispatch, and `ops::handler(op)`, which gives the function's name for the code generator.
- **Generated code is a series of calls.** It calls these functions with constant instruction words, for example `ops::rlwinm(c, m, &Insn { addr: 0x80052504, word: 0x5464103a, op: Op::Rlwinm })?`. Field extraction and the family's inner `match` fold away, so each call compiles to the few host instructions it stands for.
- **One definition.** The interpreter and the recompiled code share every instruction's definition.

The split changed no behaviour: the cutscene run ends with the same timebase and GX counts. It also made the interpreter faster (105 → 116 MIPS on the title run), because the small functions optimise better than one large `match`.

**Generated code is never committed.** It is derived from the game, so it is generated on the user's machine from `build/main.dol`:

- `rumble-port recomp` writes `build/recomp/gen/blocks.rs`, about 72 MB of Rust.
- The `rumble-blocks` crate's build script compiles it in when it exists. `RUMBLE_GEN` points it at another file; pointing it at a missing path builds without blocks.
- Builds without it interpret everywhere.
- Compiling the blocks takes 17–19 minutes in release. It happens whenever the generated file or `rumble-runtime` changes, so runtime work is quicker in a build without them (`RUMBLE_GEN=/nonexistent cargo build --release --target-dir target/fast`).

**What each block preserves from the interpreter:**

- **Time:** compiled code takes each block's length off its budget, and the dispatcher counts what ran towards the timebase when it returns. Device events and interrupts are then handled. Interrupts are asynchronous, so a block boundary is as good a delivery point as any instruction. Code inside a chain reads a timebase that is behind by what the chain has run so far, but never past a device event.
- **FP unavailable:** a block that uses the FPU hands back without running while MSR[FP] is clear. The interpreter then takes that path and raises the exception on the exact instruction.
- **High-level replacements:** at startup each block's instruction words are hashed and checked against RAM. A function with any block that differs is left out whole, since its host function chains between all of them. That covers the trap words `hle::install` writes, so replaced SDK functions run through the interpreter and `hle::call`.
- **Idle loops:** the generator recognises the load/compare/branch-back pattern and stops the chain there with `Cpu::idle_branch` set, so the dispatcher skips the wait as the interpreter does.
- **Memory:** loads and stores go through the same `Memory::r32`/`w32`, with a forced-inline fast path for cached MEM1 and MEM2. MMIO and the gather pipe sit behind a cold slow path. Locked-cache DMA stays an `mtspr` side effect.
- **Self-modifying code:** not handled yet. Blocks are checked against RAM once, at startup. Relocatable modules and Gecko codes would need writes to code to invalidate blocks.

**Checking it.** `boot --exact` sets the budget to what is left before the next timebase tick, so compiled code never runs into one. Every interrupt and device event then lands on the same instruction as in the interpreter, and the whole run must match the interpreter's exactly. This only exercises blocks shorter than 12 instructions, so the check uses `recomp --max-len 11`, which covers 89% of instructions.

Over the 4-billion-instruction cutscene workload, `--exact` and `--interp` end with the same timebase, the same 3,110,134 draws, 23,550,921 vertices and 1,940 frames. A per-block check against the interpreter on copied state is still worth adding, for the instructions only long blocks cover. It would also stop at the first mismatching block instead of only showing a different end state.

**Profiling.** `boot --profile` samples the emulation thread's instruction pointer about every half millisecond and prints the busiest host functions (`rumble-port/src/profile.rs`, Windows). Names come from the PDB, built with line tables for `rumble-runtime` and `rumble-port` (see `Cargo.toml`). Samples in compiled code are summed as "[recompiled blocks]". `boot` also reports `draw building took …` whenever capture is on.

What profiling found, in order:

1. **The ops weren't inlined.** The generated code called the family functions (`ops::load` with a runtime `match`) instead of folding them: `#[inline]` is only a hint, and LLVM judged them too big. Now every op, the small CPU helpers and the RAM fast path are `#[inline(always)]`.
2. **Per-block dispatch dominated.** Blocks average about 7 instructions, and each return to `Cpu::run` cost about 20 ns. This is what chaining removes.
3. **Draw building is now the largest cost besides the game's own code.** Mostly the transform, lighting and vertex reading per vertex, plus allocation for vertex buffers.

**Speed (cutscene workload):**

| Mode | Speed | With draw building |
|---|---|---|
| Interpreter | 97 MIPS | |
| Blocks returning to the dispatcher (11 / 64 long) | 232 / 289 MIPS | 222 MIPS (64) |
| Inlined ops, compact table (64 long) | 303 MIPS | 274 MIPS |
| Chained, at most 11 long | 399 MIPS | 282 MIPS |
| Chained, at most 64 long | 421 MIPS | 271 MIPS |

The window holds 60 fps through the intro cutscene.

#### Load-time translation with Cranelift (prototype)

The Rust blocks make the executable contain game code, so it cannot be distributed. `rumble-jit` translates the same code at load time instead, from the DOL the user's own WAD produced, so a public build needs no game code and no Rust toolchain. `boot --jit` and `run --jit` select it; the default is still the Rust blocks. It is built only with `--features jit` (`cargo build --release -p rumble-port --features jit`), because linking Cranelift in slows the Rust blocks by about 2% (see the speed notes below); without the feature, `--jit` interprets. A build with the feature and without Rust blocks (`RUMBLE_GEN=/nonexistent cargo build --release -p rumble-port --features jit`) uses the JIT by default: that is the build to distribute. It is 18 MB, and sampling the DOL's sections finds none of the game's code or data in it, apart from a few trivial counting sequences that the Rust-blocks build has too. The setup app (`rumble-port/src/setup.rs`, see the README) builds the game from the user's WAD and key into `data/<name>/` and copies the exe to `<name>.exe`, which plays that build when opened. The replay session with this build, given no backend flag, translates at load and drifts at the same frame 12592, with the same timebase. `boot` now also reports functions whose code in RAM changed during the run; there were none here (it compares the end state with the start, so code changed and then restored would not show).

**What it does.**
- **Same blocks.** `cpu::blocks::plan` now holds the block analysis that `recomp` used to do itself, and both backends call it (`recomp`'s output is byte-identical to before).
- **Same contract.** One host function per guest function, built in Cranelift IR, follows the same rules as the generated Rust:
  - a `br_table` over the function's words dispatches on `c.pc`;
  - the budget check and the FP-unavailable check run before each block;
  - the chain stops on `mmio_touched`, `stops_chain` instructions and idle loops;
  - the same RAM check leaves out functions that hold HLE trap words.
- **Direct jumps.** Blocks jump straight to their known successors inside the function. Indirect branches go back through the `br_table`.
- **The table.** The dispatcher's table holds a `blocks::Entry`, either a Rust `BlockFn` or an `extern "C"` `JitFn`. A `JitFn` returns 0, or a fault encoded as `1 << 32 | address`.
- **Emitted as IR:**
  - integer arithmetic (including carries and the division edge cases), logic, shifts and rotates;
  - compares and CR ops;
  - integer loads and stores (not the byte-reversed or reservation forms);
  - `b`/`bc`/`bclr`/`bcctr`;
  - `mfspr`/`mtspr` (except DEC and DMA_L), `mfcr`/`mtcrf`, `mfmsr`;
  - FP and paired singles: the scalar and paired arithmetic (multiply-adds round twice, as in `ops`), `frsp`, `fctiw`/`fctiwz` (rounding per FPSCR[RN]), `fsel`, `fres`/`frsqrte`, the merges, sums and compares;
  - FP loads and stores, and `psq_l`/`psq_st` when the GQR type is a float. Outside the cached mirrors, or for an integer GQR type, they call the `ops` function.
  That is over 99.9% of the instructions translated.
- **Everything else** calls its `ops` function through an `extern "C"` shim with a pointer to the `Insn`, so all three paths still share one definition. This covers `lmw`/`stmw`, the string ops, the FPSCR moves, `sc`, `rfi`, `mtmsr`, `mftb` and `dcbz`.
- **Memory.** A load or store looks up a 16-entry table, one entry per 256 MB of address space, holding the host base and size. Only the cached MEM1 and MEM2 mirrors have entries. A hit goes straight to host RAM with a byte swap; anything else calls `Memory::r*`/`w*`.
- **Guest registers stay in `Cpu`.** Every access is a load or store. Guest state and guest RAM are in separate Cranelift alias regions, so redundant loads are removed within a block.
- **Translation time.** At boot all 13,407 functions are compiled on every core (`opt_level=speed`). Each is compiled to bytes, which need no relocation. This takes about 2.7 s for 55 MB of code. Translated code is not cached on disk yet.

**Checks.**
- **Differential test.** `cargo test -p rumble-jit` runs 2,000 random three-block functions both translated and interpreted, and compares GPRs, CR, XER, LR, CTR, PC, the instruction count and RAM. The functions mix ALU ops, compares, CR ops, loads and stores, and a random conditional branch with CTR and CR forms.
- **Replay.** The replay session (70G instructions) with `--jit --dump-frames` drifts at the same frame 12592 as the Rust blocks. It ends with the same timebase, 38,327,914 draws and 15,107 frames. All 504 dumped frames are byte-identical to the Rust-block build's.

**Speed** (the replay session, `--no-trace --capture --threaded`, same machine, runs alternated in two rounds):

| Backend | MIPS |
|---|---|
| Interpreter | 78 |
| Cranelift, prototype | 343 / 351 |
| Rust blocks (LLVM) | 352 / 387 |
| Rust blocks, build from before this change | 384 / 396 |
| Cranelift with FP and paired singles as IR (later run, same day) | 388, against 380 for the Rust blocks |

So the prototype runs at about 94% of the Rust blocks. Run-to-run spread on this machine is large: the old build alone measured 343–430 over the day. On a quieter machine, 12 replays in ABBA order put the Rust blocks in the new build about 2.5% under the old build: a median of 430 MIPS against 442, and a best run of 433 against 444. Five of the six pairs went to the old build. The gap is small but real. To rule out the dispatcher, the Rust blocks went back to their own table and the original lookup and `compiled()`. Translated code now has a separate table and an out-of-line `Cpu::translated`, which is reached only when there is no Rust block. A second run of 12 replays still measured about 2.3% (a median of 378 against 387 MIPS on a busier machine, with the old build ahead in all 6 pairs). Profiles show the same time in the blocks themselves; the extra is in the runtime around them. The hot path is now the same source on the same dependency versions, so what remains is most likely code layout from the larger binary rather than anything the change does on that path. A third run of 12 replays confirmed it: the same build without the JIT crate (`--no-default-features` at the time) was level with the old build. It won 4 of the 6 pairs, with a median of 374 against 377 MIPS and a best run of 392 against 386 on a noisy machine. So the JIT is now an opt-in cargo feature, `jit`, and default builds don't link Cranelift. Real time needs about 294 MIPS, so the prototype already keeps up on this workload.

**Where the time goes.** Profiled over frames 3000–9000:

| | Cranelift | Rust blocks |
|---|---|---|
| Translated or compiled code | 49% | 62% |
| `ops` functions through shims (mostly FP) | about 20% | (inlined in the 62%) |
| Dispatcher (`Cpu::run`) | 14% | 18% |

`rumble-jit` has no line tables, so the profiler files the shim samples under `rumble_jit::translate`.

**Sound dropouts in busy scenes (fixed).** Played in real time, the JIT used to fall behind in busy scenes, and the sound ran dry. A speed log of the replay (`RUMBLE_SPEED_LOG`) shows it lagging past the audio buffer at 7 points, worst 1.8 s. The Rust blocks lag at 3, worst 0.85 s. Those 3 are the boot load, frame 748 and the end of the replay, so the JIT's 5 extra (frames 2342, 5642, 6047, 6698 and 8002) were gameplay. With FP and paired singles as IR (item 1), it lags at 2 points, the boot load and the end of the replay, worst 0.9 s. The replay still drifts at frame 12592 with the same timebase, draws and vertices, and all 504 dumped frames are byte-identical to the Rust blocks'.

**Where to take it, in order of what it should buy:**
1. **FP and paired singles as IR (done).** These were most of the 20% in shims. Each shim call stores and reloads guest state around an opaque call and breaks redundant-load removal. `lfs`/`stfs`/`lfd`/`stfd`, `fmr`, the scalar arithmetic and `ps_*` map directly onto Cranelift float and `f64x2` ops. The quantized loads and stores only need a GQR-type switch. `lmw`/`stmw` unroll. This alone should close most of the gap.
2. **Guest registers in host registers.** Cache the GPRs, CR fields and XER[CA] a function uses in Cranelift variables. Write them back only at exits and before calls, so Cranelift's register allocator keeps them in host registers across blocks. This is the main thing LLVM does better today: it sees through `c.gpr[n]` across a whole chained function. Keeping CR as eight separate fields would also remove most of the shifting and masking around compares and branches.
3. **Fastmem.** Reserve 4 GB of host address space, map MEM1 and MEM2 at their cached and uncached mirrors, and leave the rest unmapped. Each access then becomes one `load [base + ea]` plus a byte swap. MMIO would be caught by a guard-page fault handler, or as now by the region check, skipped where the address is known to be RAM (stack-relative accesses). This replaces about 8 instructions and a branch per access.
4. **Smaller wins.** Mark slow paths cold, drop `mmio_touched` checks after blocks whose only accesses are stack-relative, cache translations on disk keyed by the DOL hash (load time is already only seconds), and chain across functions for direct `bl` and `b` to known functions.

With 1 and 2 the translated code should at least match the Rust blocks. The Rust path has the same register-in-memory model and calls every op through an inlined function, so LLVM's advantage is mostly cross-instruction cleanup that items 2 and 3 do by design. With all three it should beat them: the Rust blocks also pay the region check on every access and keep every guest register in memory. The dispatcher's share (14–18%) is the same for both and is the next limit after that.

#### Order

1. ~~Move the instruction semantics out of `step` into shared `ops` functions.~~ Done.
2. ~~Generate blocks, wire up the dispatcher and check them against the interpreter.~~ Done. Every instruction with an `ops` function is compiled, including FP and paired singles.
3. ~~Profile and chain blocks within functions.~~ Done.
4. Next:
   - **Draw building.** Transform, lighting and vertex reads per vertex, and allocation churn in vertex buffers. Moving it to its own thread is worth it once it is lean.
   - **A per-block check** against the interpreter on copied state, for what only long blocks cover.
   - **Invalidating blocks** on writes to code.

### 4. Runtime

- **Threads:** each `OSThread` runs on its own host thread (or fiber) under one global CPU lock, because the Wii has a single core. `OSSwitchThread`/`OSLoadContext` and `rfi` paths are replaced, not recompiled.
- **Time:** the timebase (60.75 MHz) advances once per 12 instructions, Broadway's 729 MHz at one instruction per cycle, so the game gets the CPU time per frame it expects (with less, its sound thread starves everything else). A load/compare/branch-back loop polling RAM, such as the OS idle loop, skips straight to the next device event or decrementer expiry. The interpreter runs at about 105 MIPS this way: faster than real time on menus, about real time in busier scenes (see Codegen for the measurements). Codegen is what makes gameplay keep up.
- **FPU:** FP and paired-single instructions trap (FP unavailable, 0x800) while MSR[FP] is clear, as on hardware; the OS saves a thread's FP registers from that trap, so interrupt handlers that use floats do not corrupt the thread they interrupted.
- **Speed stand-ins:** until codegen exists, `CXSecureUncompressLH` runs natively (`rumble-runtime/src/hle/cx.rs`, using `rumble_formats::lh`, whose output matches the game's own). The game unpacks tens of megabytes of archives behind its first loading screen, which takes minutes in the interpreter.
- **Interrupts:** VI retrace, AI/DSP and the decrementer are delivered at safe points (function entries and loop back-edges) by calling the game's registered handlers.
- **IOS (IPC):**
  - `/dev/fs` and the ES content reads behind the CNT library serve files from `build/fs`, which already has mod overlays applied. Loose-file mods need no further work.
  - Saves map to a host folder.
  - **Importing and exporting saves.** The menu's Game tab imports and exports the save as an SD-card `data.bin`, the file a Wii's Data Management copies (`rumble-formats/src/wiisave.rs`, `rumble-port/src/saves.rs`).
    - **Import.**
      - Checks the whole file before touching anything: the SD key, the header MD5, title WPSE (other regions are refused by name), and safe file names.
      - The emulation thread applies it once the game has stopped. It copies the save folder to `build/save-backups/<UTC time>/` (never overwriting a backup), replaces everything except `banner.bin`, and boots the game again.
    - **Export.**
      - Builds the encrypted header with its MD5, the Bk header and the files, each file with a random IV.
      - Signs the result as the System Menu does: the AP key (private key 1, named `AP0000000100000002`) is certified by the console's NG key, and the AP key signs SHA-1(SHA-1(Bk header and files)).
      - Before writing, it reads the result back and checks both signatures.
      - The order and permission bytes match a real Wii's export of this game.
    - **Signing identity.** It is the console in a BootMii `keys.bin` next to the common key, if there is one, laid out as Dolphin's `BootMiiKeyDump`. Without one it signs as Dolphin's default console (`DEFAULT_DEVICE_ID`, `DEFAULT_KEY_ID`, `DEFAULT_PRIVATE_KEY` and `DEFAULT_SIGNATURE` in Dolphin's `Source/Core/Core/IOS/IOSC.cpp`), whose exports real Wiis accept.
    - **The curve code** (`rumble-formats/src/ec.rs`, sect233r1) is tested against a real Wii's export: both of its signatures verify. Nintendo's signature on the NG certificate can't be checked without Nintendo's key.
  - Wii Remotes: WPAD and KPAD run as compiled, so the game keeps their channel state, callbacks and sample processing. Below them, WUD (the Bluetooth device manager) is replaced: `WUDInit` reports success without starting the Bluetooth stack, and `WUDGetStatus` reports ready. No remote connects over Bluetooth. Host input comes in at the top: `KPADReadEx` and `WPADProbe` are replaced to answer from `rumble-runtime/src/input.rs`, which `rumble-port run` fills from the keyboard and gamepads (`rumble-port/src/controls.rs`, remote held sideways) and `boot --press BUTTON@FRAME` scripts. Accelerometer and pointer read as a remote at rest. Commands WPAD queues for a remote are never sent or answered, so anything that waits for an answer is replaced as well. `WPADGetInfo` answers at once with a full battery and the player LED; the game checks the battery at the launch pad before a stage and would otherwise wait forever. `WPADWriteGameData` and `WPADReadGameData` (saving Pokemon to the Wii Remote) keep each remote's memory in `build/<nand>/wiimote/remoteN.bin`, laid out as the remote's EEPROM: the game data header at 0x2A and a copy at 0x62, then the data at 0x9A. The game looks for a remote carrying its data with `WPADGetGameTitleUtf16`, which answers from the header in the remote's file (WPAD reads it when a remote connects, which never happens here) and copies it into WPAD's channel state. A read checks the header's game code as the SDK does and answers `WPAD_ERR_NOPERM` for a remote with no data for this game. Both finish at once and call the game's callback before returning, as the SDK does when a call fails. A replacement calls a guest function with `hle::call_guest`, which pushes a small frame and returns through the `GuestReturn` trap installed in the word after each replaced entry. Headless `boot` runs keep their NAND in `build/nand-boot`, so tests and replays never touch the player's saves.
- **GX → wgpu:** this is the largest piece.
  - Decode the FIFO (CP/BP/XF commands). Done in `rumble-runtime/src/gx.rs`: gather-pipe bursts go to the FIFO in RAM and, while the CP is linked, through a decoder that sizes draws from the vertex descriptor/format registers, runs display lists, keeps CP/BP/XF state and raises the PE finish/token interrupts.
  - First renderer (`rumble-runtime/src/gx/draw.rs`, `rumble-port/src/render.rs`, `rumble-port run`): with capture on, primitives become draw commands with vertices transformed to EFB clip space on the CPU and textures decoded to RGBA8; wgpu renders them with one TEV shader (all stages, konst, alpha compare), GX blend and depth modes and scissor, and an XFB copy is what gets presented. `boot --dump-frames DIR` writes frames to PNGs instead. Vertex lighting (ambient, material, up to 8 lights with diffuse and spot/specular attenuation) and texgen from position, normal (environment maps, with the dual-texture post transform) or texcoords run on the CPU per vertex. EFB copies to a texture in RAM stay on the GPU: a texture sampled from the copy's address at its size uses the copy. The hub after the Battle Royale draws the scene back through one; with stale RAM it was black. Nothing is written back to RAM, and copies keep the EFB's color whatever pixel format they ask for. Back faces are culled as BP gen mode asks (front faces wind clockwise; strip triangles keep the strip's winding). Copies and draws are bound in the order the game issues them, so a scratch buffer copied to, drawn from and copied to again within a frame gives each draw its own copy. A copy covers rows of tiles a stride apart (`CopySpan`), not one block: the collection screen copies icons side by side into one sheet, so overlap, windows and moves are worked out row by row. The runtime hashes the RAM under each copy when it is made; when a texture is read from inside one whose RAM has since changed (the game loaded something else there), it sends `GpuCommand::Forget` and the renderer drops the copy. `boot --dump-frames` renders every frame, saving every Nth, since later frames draw from earlier copies. Decoded textures are shared by content, and copies never reach RAM, so the bytes behind many copies are alike (the befriend panel's and collection's portraits, each rendered into its own copy): a draw's `TexBinding` carries its own address, which copies are looked up by. Not yet: lines/points, TEV swap tables, indirect texturing, mipmaps, logic ops, the copy pixel formats and the copy pixel formats.
  - Draw building runs on a thread of its own by default (`gx/worker.rs`, the Game menu's "Build frames on a separate thread"): the emulator's `Gx` still decodes every command for the register state, interrupts and what the widescreen hooks read (`Gx::note_view`), and hands the executed FIFO bytes to a second `Gx` that builds the frames, with the hooks' changes (HUD anchors, widened shadow light, cameras) sent along at their FIFO positions. The worker reads RAM as the GPU does; at each draw-done the emulator waits for it, since the game takes that as the GPU being finished with what it drew from. Nothing the game sees depends on the worker, so sessions replay the same either way. On the first stage of a recorded session it took the emulator from 320 to 380 MIPS.
  - Widescreen HUD: NW4R layouts of the in-game HUD (portrait, combo, minimap, money and lives) are recognised as they are built (`hle/lyt.rs`, `Layout::Build`), and while one's root pane draws (`Pane::Draw`) its 2D draws move to their side of the wider screen.
  - Generate WGSL shaders from TEV stage configurations, and cache them.
  - Decode the texture formats: I4/I8/IA4/IA8/RGB565/RGB5A3/RGBA8/CMPR, plus the C4/C8/C14X2 palette formats.
  - Resolution and widescreen hook in here, not in game code:
    - With the widescreen hack, the EFB is k times as wide as 4:3 (`Gx::widescreen`, `Renderer::set_widescreen`). The game's 4:3 EFB sits centered in it and every draw keeps its place and shape there (clip-space x divided by k), as do copies and scissors narrower than 640 pixels. Draws through a full-width viewport can also reach the sides: perspective scenes show more of the world (a wider field of view), and a 2D primitive covering the whole width (a backdrop or fade) is stretched to fill. Full-width copies and scissors span the wider EFB. The game culls what its camera can't see, so the camera itself is widened: `C_MTXPerspective` is replaced (`hle/mtx.rs`) and gives 4:3 projections k times the aspect ratio; draws whose projection is already that wide aren't narrowed again. Small viewports drawn straight to the screen (the ranking podium) stay where they belong. `boot --widescreen` dumps 16:9 frames.
    - Viewport and scissor are scaled to the render target.
- **Audio:** the DSP is high-level emulated (`dsp.rs`), and each frame's AX command list (3 ms, 96 samples at 32 kHz) is run on the host (`ax.rs`): the voices' parameter blocks (0x140 bytes, linked) are decoded (ADPCM, PCM16, PCM8), resampled, enveloped and mixed into the main buses, their playback state is written back, and the output buffer is written where the game's audio DMA then plays it. `Memory` keeps what the DMA plays (`Memory::audio`), and `rumble-port run` sends it to the default device through cpal (`audio.rs`), resampled, at the master volume, stereo or mono. `boot --wav FILE` records it. Music, sound effect and Pokemon voice volumes are applied in the game, to NW4R's sound players (`snd.rs`: player 2 is the BGM, 3 the cries), as the game's own volume times the setting. Not yet: the aux buses (reverb), surround, the biquad filter and the Wii Remote speakers.
- **Remote control:** `rumble-port run` reads commands appended to `build/remote/commands.txt`: `press`, `hold`, `release`, `wait FRAMES`, `screenshot`, `pause`, `resume`. They run on exact frames alongside the keyboard and gamepads. Results go to `log.txt`, and `status.txt` gets the frame, speed and timings every second. The full list is in `rumble-port/src/remote.rs`. It lets a script, or an assistant without access to the window, play and check the running game. Every change to remote 1's buttons is recorded to `input.log`, and `boot --replay FILE` plays a recording back by frame. A problem met while playing can then be reproduced in `boot`, where the report lists the OS threads with their call stacks.
- **Interpreter fallback:** a Gekko interpreter built on the same decoder runs any code the recompiler doesn't cover. That includes misdetected functions, code generated at runtime, and Gecko-code mods. It also serves as the reference to compare recompiled functions against.

## Recommended order

1. **Runtime and interpreter first.** Memory, MMIO stubs and an interpreter can boot `main.dol` to its first VI wait. That validates the HLE boundary before codegen exists.
2. **IOS file and content access.** The game loads its archives from `build/fs`.
3. **GX FIFO capture.** Log draw calls and dump textures, then bring up the wgpu renderer.
4. **Input and audio.**
5. **Codegen.** Recompiled functions replace interpreted ones, checked by running each against the interpreter.
6. **Widescreen, resolution, frame pacing and polish.**
