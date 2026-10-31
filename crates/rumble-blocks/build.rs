//! Compiles in build/recomp/gen/blocks.rs (written by `rumble-port recomp`)
//! when it exists; without it the crate has no blocks and the game runs in
//! the interpreter. RUMBLE_GEN overrides the path.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(generated)");
    println!("cargo::rerun-if-env-changed=RUMBLE_GEN");
    let path = match std::env::var_os("RUMBLE_GEN") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/recomp/gen/blocks.rs"),
    };
    println!("cargo::rerun-if-changed={}", path.display());
    if path.is_file() {
        println!("cargo::rustc-cfg=generated");
        println!("cargo::rustc-env=RUMBLE_GEN_PATH={}", path.canonicalize().unwrap().display());
    }
}
