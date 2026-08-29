#!/bin/sh
# Runs the component gallery. The crate is its own workspace (excluded from
# the root — SPEC V5), so we run it from inside and share the root target/
# to avoid a second 1.1G build cache.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

cd "$repo_root/crates/harvester-gallery"
CARGO_TARGET_DIR="$repo_root/target" exec cargo run "$@"
