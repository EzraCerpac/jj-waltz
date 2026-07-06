#!/bin/sh
set -eu

plugin_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)

cargo build --release --locked --manifest-path "$plugin_root/Cargo.toml"
cargo build \
  --release \
  --locked \
  --manifest-path "$plugin_root/../../Cargo.toml" \
  --bin jw \
  --target-dir "$plugin_root/target/jj-waltz"
