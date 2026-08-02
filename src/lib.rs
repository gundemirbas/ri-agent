//! ri-agent library surface.
//!
//! The executable entry point lives in `src/main.rs`; this crate exists so
//! integration tests (`tests/`) can drive real behaviors that unit tests
//! inside the binary cannot — most importantly the rootless sandbox, which
//! spawns the `ri-sandbox` bin target (via `CARGO_BIN_EXE_ri-sandbox`) and
//! assembles a scratch image through [`container::rootfs`].
//!
//! Keep additions deliberate: only expose what cross-crate tests genuinely
//! need.

pub mod container;
