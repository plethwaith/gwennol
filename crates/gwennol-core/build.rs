//! One cfg, `dir_handles`: the target has the `openat` family and
//! `fdopendir`, so `steps::dir` holds directories as descriptors. That
//! is every unix target but Redox, where nix has no `dir` module; Redox
//! and every non-unix target get the path-based fallback.
//!
//! `GWENNOL_NO_DIR_HANDLES=1` turns the cfg off on a target that has it,
//! so the fallback can be compiled where CI runs; the race pins are
//! not expected to pass that way, and CI only type-checks it.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(dir_handles)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GWENNOL_NO_DIR_HANDLES");
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let forced_off = std::env::var("GWENNOL_NO_DIR_HANDLES").is_ok_and(|v| v == "1");
    if unix && os != "redox" && !forced_off {
        println!("cargo:rustc-cfg=dir_handles");
    }
}
