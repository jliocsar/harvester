use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let daemon_source =
        manifest_dir.join("../../target/x86_64-unknown-linux-musl/daemon-release/harvesterd");
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
