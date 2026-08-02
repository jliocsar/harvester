//! Embeds the static musl harvesterd (slice-3 verdict 3a) so the shipped
//! desktop binary bootstraps machines with nothing else on disk.
//!
//! A missing musl artifact is a warning, not an error, so
//! `cargo check --workspace` works on a fresh clone; dev builds fall back to
//! reading the target dir at runtime (see `daemon_binary` in main.rs).

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let daemon_source =
        manifest_dir.join("../../target/x86_64-unknown-linux-musl/release/harvesterd");
    let embedded = out_dir.join("harvesterd");

    println!("cargo:rerun-if-changed={}", daemon_source.display());

    if daemon_source.exists() {
        fs::copy(&daemon_source, &embedded).expect("copy harvesterd into OUT_DIR");
    } else {
        println!(
            "cargo:warning=no musl harvesterd at {}; embedding nothing (dev fallback reads it at runtime)",
            daemon_source.display()
        );
        fs::write(&embedded, []).expect("write empty harvesterd placeholder");
    }
}
