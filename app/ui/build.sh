#!/bin/sh
# Build the Leptos UI into app/dist/pkg/ (loaded by app/dist/index.html).
# Same wasm-bindgen flow as web/spike — no extra toolchain.
set -e
cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
wasm-bindgen --target web --no-typescript \
  --out-dir ../dist/pkg \
  target/wasm32-unknown-unknown/release/zink_ui.wasm
echo "built app/dist/pkg"
