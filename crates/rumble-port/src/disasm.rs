//! `disasm` command: analyze build/main.dol and write a listing, a
//! function table and a summary into build/recomp/.

use anyhow::{Context, Result};
use rumble_formats::dol::Dol;
use rumble_recomp::analysis::{self, Exit, SeedSource};
use rumble_recomp::sigs;
use rumble_recomp::ppc::Op;
use std::fmt::Write as _;
use std::path::Path;

pub fn run(build_dir: &Path) -> Result<()> {
    let dol_path = build_dir.join("main.dol");
    let data = std::fs::read(&dol_path)
        .with_context(|| format!("reading {} (run `build` first)", dol_path.display()))?;
    let dol = Dol::parse(&data)?;
    let prog = analysis::analyze(&dol);
    let out = build_dir.join("recomp");
    std::fs::create_dir_all(&out)?;

    // Listing with function and block labels.
    let mut s = String::new();
    for sec in &dol.sections {
        writeln!(s, "# {} 0x{:08x}-0x{:08x} ({} bytes)", sec.name(), sec.addr, sec.end(), sec.data.len())?;
    }
    writeln!(s, "# entry 0x{:08x}, bss 0x{:08x}+0x{:x}\n", dol.entry, dol.bss_addr, dol.bss_size)?;
    for f in &prog.functions {
        writeln!(s, "\nfn_{:08x}:  # {:?}, {} blocks", f.start, f.source, f.blocks.len())?;
        for b in &f.blocks {
            if b.start != f.start {
                writeln!(s, ".L_{:08x}:", b.start)?;
            }
            for a in (b.start..b.end).step_by(4) {
                let i = prog.insn(a).unwrap();
                write!(s, "  {a:08x}  {:08x}  {i}", i.word)?;
                if let Some(t) = i.branch_target().filter(|_| i.lk()) {
                    write!(s, "  # fn_{t:08x}")?;
                }
                if let Some(jt) = prog.jump_tables.get(&a) {
                    write!(s, "  # switch table 0x{:08x}, {} cases", jt.table, jt.targets.len())?;
                }
                s.push('\n');
            }
        }
    }
    std::fs::write(out.join("main.s"), s)?;

    let mut csv = String::from("start,end,size,source,blocks,calls,indirect_calls,illegal,exits\n");
    for f in &prog.functions {
        let exits: Vec<&str> = f
            .blocks
            .iter()
            .filter_map(|b| match b.exit {
                Exit::Local => None,
                Exit::Return => Some("ret"),
                Exit::TailCall(_) => Some("tail"),
                Exit::Switch => Some("switch"),
                Exit::IndirectTailCall => Some("vtail"),
                Exit::IndirectJump => Some("indirect"),
                Exit::Rfi => Some("rfi"),
            })
            .collect();
        writeln!(
            csv,
            "{:08x},{:08x},{},{:?},{},{},{},{},{}",
            f.start,
            f.end,
            f.end - f.start,
            f.source,
            f.blocks.len(),
            f.calls.len(),
            f.indirect_calls,
            f.illegal,
            exits.join(" ")
        )?;
    }
    std::fs::write(out.join("functions.csv"), csv)?;

    let summary = summarize(&prog, &data);
    std::fs::write(out.join("summary.txt"), &summary)?;
    print!("{summary}");
    println!("wrote {}/{{main.s,functions.csv,summary.txt}}", out.display());
    Ok(())
}

fn summarize(prog: &analysis::Program, raw: &[u8]) -> String {
    let st = &prog.stats;
    let mut s = String::new();
    let count = |src| prog.functions.iter().filter(|f| f.source == src).count();
    let blocks: usize = prog.functions.iter().map(|f| f.blocks.len()).sum();
    let in_fn_illegal: usize = prog.functions.iter().map(|f| f.illegal).sum();
    let indirect_calls: usize = prog.functions.iter().map(|f| f.indirect_calls).sum();
    let ps: usize = st
        .op_counts
        .iter()
        .filter(|(op, _)| op.mnemonic().starts_with("ps") || matches!(op, Op::DcbzL))
        .map(|(_, n)| n)
        .sum();

    let _ = writeln!(s, "instructions:     {}", st.insns);
    let _ = writeln!(
        s,
        "functions:        {} (entry {}, called {}, prologue {}, after-padding {}, tail-call {}, data-pointer {})",
        prog.functions.len(),
        count(SeedSource::Entry),
        count(SeedSource::Call),
        count(SeedSource::Prologue),
        count(SeedSource::AfterPadding),
        count(SeedSource::TailCall),
        count(SeedSource::DataPointer)
    );
    let _ = writeln!(s, "basic blocks:     {blocks}");
    let _ = writeln!(
        s,
        "bctr:             {} switch tables, {} indirect tail calls, {} unresolved",
        st.jump_tables, st.indirect_tail_calls, st.unresolved_bctr
    );
    let _ = writeln!(s, "indirect calls:   {indirect_calls} (bctrl)");
    let _ = writeln!(s, "illegal words:    {} total, {in_fn_illegal} inside functions", st.op_counts.get(&Op::Illegal).unwrap_or(&0));
    let _ = writeln!(s, "paired-single:    {ps} instructions");
    let _ = writeln!(s, "\nmost common ops:");
    let mut ops: Vec<_> = st.op_counts.iter().collect();
    ops.sort_by(|a, b| b.1.cmp(a.1));
    for (op, n) in ops.iter().take(25) {
        let _ = writeln!(s, "  {:<10} {n}", op.mnemonic());
    }
    let _ = writeln!(s, "\nSDK build strings:");
    for line in sdk_strings(raw) {
        let _ = writeln!(s, "  {line}");
    }
    s
}

/// Revolution SDK libraries embed "<< RVL_SDK - LIB ... >>" banners; they
/// tell us which library versions the HLE layer has to match.
fn sdk_strings(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = raw[i..].windows(3).position(|w| w == b"<< ") {
        let start = i + p;
        let end = raw[start..].iter().position(|&b| b == 0).map_or(raw.len(), |e| start + e);
        let text = String::from_utf8_lossy(&raw[start..end]).into_owned();
        if text.contains("SDK") || text.contains("NW4R") {
            out.push(text);
        }
        i = end.max(start + 1);
    }
    out
}

/// `sig` command: prints a function's signature as a Rust array, and
/// whether it is unique in this DOL.
pub fn sig(build_dir: &Path, addr: u32, len: Option<usize>) -> Result<()> {
    let data = std::fs::read(build_dir.join("main.dol")).context("reading build/main.dol (run `build` first)")?;
    let dol = Dol::parse(&data)?;
    let prog = analysis::analyze(&dol);
    let f = prog.function_at(addr).filter(|f| f.start == addr).context("no function starts there")?;
    let len = len.unwrap_or(((f.end - f.start) / 4) as usize).min(64);
    let words = sigs::signature(&prog, addr, len).context("signature runs off the code")?;
    let matches = sigs::find(&prog, &words);
    println!("// fn_{addr:08x}, {len} instructions, {} match(es) in this DOL", matches.len());
    for chunk in words.chunks(6) {
        println!("    {}", chunk.iter().map(|w| format!("0x{w:08x},")).collect::<Vec<_>>().join(" "));
    }
    Ok(())
}
