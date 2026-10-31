//! A sampling profiler for `boot --profile` (Windows only): a thread
//! suspends the emulation thread about every half millisecond, reads its
//! instruction pointer, and at the end resolves the samples to function
//! names through the PDB (built with line tables for that, see Cargo.toml).
//! Recompiled blocks have no symbols: samples are matched to them through
//! the block table's function pointers, summed as one entry, and the
//! busiest listed separately.

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetThreadContext, SymFromAddr, SymInitialize, CONTEXT, SYMBOL_INFO,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
        THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
    };

    const CONTEXT_CONTROL: u32 = 0x0010_0001;

    /// GetThreadContext needs a 16-byte aligned CONTEXT.
    #[repr(C, align(16))]
    struct Aligned(CONTEXT);
    const INTERVAL: Duration = Duration::from_micros(500);
    /// About 15 minutes of samples.
    const MAX_SAMPLES: usize = 2_000_000;

    pub struct Profiler {
        stop: Arc<AtomicBool>,
        thread: JoinHandle<Vec<u64>>,
    }

    /// Starts sampling the calling thread.
    pub fn start() -> Profiler {
        let target = unsafe { GetCurrentThreadId() };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            // Never allocate while the thread is suspended: it may hold the
            // heap lock. Sampling stops when the buffer is full.
            let mut samples = Vec::with_capacity(MAX_SAMPLES);
            let handle: HANDLE =
                unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, target) };
            if handle.is_null() {
                return samples;
            }
            let mut next = Instant::now();
            while !flag.load(Ordering::Relaxed) && samples.len() < MAX_SAMPLES {
                // Spin rather than sleep: Windows sleeps last ~1-15 ms.
                next += INTERVAL;
                while Instant::now() < next {
                    std::hint::spin_loop();
                }
                unsafe {
                    if SuspendThread(handle) == u32::MAX {
                        break;
                    }
                    let mut ctx: Aligned = std::mem::zeroed();
                    ctx.0.ContextFlags = CONTEXT_CONTROL;
                    if GetThreadContext(handle, &mut ctx.0) != 0 {
                        samples.push(ctx.0.Rip);
                    }
                    ResumeThread(handle);
                }
            }
            unsafe { CloseHandle(handle) };
            samples
        });
        Profiler { stop, thread }
    }

    /// Function name at `addr`, from the PDB. Matches far from the address
    /// are a public symbol before code without one, so they don't count.
    fn symbol(addr: u64) -> Option<String> {
        const MAX_NAME: usize = 1024;
        let mut buf = vec![0u64; (std::mem::size_of::<SYMBOL_INFO>() + MAX_NAME) / 8 + 1];
        let info = buf.as_mut_ptr() as *mut SYMBOL_INFO;
        unsafe {
            (*info).SizeOfStruct = std::mem::size_of::<SYMBOL_INFO>() as u32;
            (*info).MaxNameLen = MAX_NAME as u32;
            let mut disp = 0u64;
            if SymFromAddr(GetCurrentProcess(), addr, &mut disp, info) == 0 || disp > 0x10000 {
                return None;
            }
            let name = std::slice::from_raw_parts((*info).Name.as_ptr() as *const u8, (*info).NameLen as usize);
            Some(String::from_utf8_lossy(name).into_owned())
        }
    }

    impl Profiler {
        /// Stops sampling and prints the busiest functions. `blocks` is each
        /// recompiled block's host code address and guest start address.
        pub fn report(self, blocks: &[(u64, u32)]) {
            self.stop.store(true, Ordering::Relaxed);
            let samples = self.thread.join().unwrap_or_else(|e| {
                println!("profile: sampler failed: {e:?}");
                Vec::new()
            });
            if samples.is_empty() {
                println!("profile: no samples");
                return;
            }
            // The executable names its PDB without a path; look next to it.
            let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.display().to_string()));
            let search = std::ffi::CString::new(dir.unwrap_or_default()).unwrap_or_default();
            unsafe { SymInitialize(GetCurrentProcess(), search.as_ptr() as *const u8, 1) };
            let mut by_addr: HashMap<u64, u64> = HashMap::new();
            for &s in &samples {
                *by_addr.entry(s).or_default() += 1;
            }
            let mut sorted = blocks.to_vec();
            sorted.sort_unstable();
            // Blocks have no symbols: an address without one, among the
            // blocks, belongs to the block starting below it.
            let range = sorted.first().map_or(0..0, |f| f.0..sorted.last().unwrap().0 + 0x1000);
            let mut by_fn: HashMap<String, u64> = HashMap::new();
            let mut blocks: HashMap<String, u64> = HashMap::new();
            let mut in_blocks = 0;
            for (addr, n) in by_addr {
                match symbol(addr) {
                    // Blocks can resolve to their own (mangled) names too.
                    Some(name) if name.contains("rumble_blocks") => {
                        in_blocks += n;
                        *blocks.entry(name).or_default() += n;
                    }
                    Some(name) => *by_fn.entry(name).or_default() += n,
                    None if range.contains(&addr) => {
                        let k = sorted.partition_point(|b| b.0 <= addr) - 1;
                        in_blocks += n;
                        *blocks.entry(format!("block {:08x}", sorted[k].1)).or_default() += n;
                    }
                    None => *by_fn.entry(format!("?{addr:#x}")).or_default() += n,
                }
            }
            by_fn.insert("[recompiled blocks]".into(), in_blocks);
            let total = samples.len() as f64;
            println!("profile: {} samples", samples.len());
            let mut v: Vec<_> = by_fn.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            for (name, n) in v.iter().take(25) {
                println!("  {:5.1}%  {name}", *n as f64 * 100.0 / total);
            }
            let mut v: Vec<_> = blocks.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            println!("  busiest blocks:");
            for (name, n) in v.iter().take(8) {
                println!("  {:5.1}%  {name}", *n as f64 * 100.0 / total);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Profiler;

    pub fn start() -> Profiler {
        Profiler
    }

    impl Profiler {
        pub fn report(self, _blocks: &[(u64, u32)]) {
            println!("profile: only available on Windows");
        }
    }
}

pub use imp::start;
