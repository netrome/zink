#!/bin/sh
# Build the A6 spike WASM bundle into web/spike/pkg/.
set -e
cd "$(dirname "$0")/../.."
cargo build -p zink-client --target wasm32-unknown-unknown --release
wasm-bindgen --target web --no-typescript \
  --out-dir web/spike/pkg \
  target/wasm32-unknown-unknown/release/zink_client.wasm
echo "built web/spike/pkg — serve web/spike/ and open it"
