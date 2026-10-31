//! The game's code compiled to Rust blocks by `rumble-port recomp`, for
//! `rumble_runtime::cpu::blocks`. Empty until that has been run.

#[cfg(generated)]
include!(env!("RUMBLE_GEN_PATH"));

#[cfg(not(generated))]
pub static BLOCKS: &[rumble_runtime::cpu::blocks::Block] = &[];
