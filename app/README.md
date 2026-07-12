# zink app (Tauri v2 · Android + Linux desktop)

The MVP client. This is a *map* of the as-built shape (C3) — design rationale
lives in `docs/design/client-core.md` and `docs/design/live-delivery.md`;
toolchain setup in `docs/DEV-SETUP.md` §3.

## Layout & layering

```
dto/        wire types of the Tauri commands — the ONE place both sides of the
            IPC agree on shapes; src-tauri serializes, ui deserializes, drift
            is a compile error. Plain serde structs, no other deps.
src-tauri/  the command layer: one long-lived zink-client `Client` in managed
            state; commands are thin — resolve petnames, hex-encode ids,
            base64 blobs (JSON IPC), render DTOs from the *stored DAG*.
            All Kotlin under gen/android is Tauri-generated except (from C4c
            on) the foreground-service shell.
ui/         the webview UI: Leptos (CSR, wasm32-only). Presentation ONLY —
            naming, ordering, threading, crypto all happen behind `invoke`.
            Talks to Tauri via a ~35-line hand-rolled shim (src/invoke.rs; no
            tauri-sys dependency). Image downscaling happens here on a canvas
            (src/image.rs) so the Rust side needs no image codec.
dist/       frontendDist: index.html (source, incl. all CSS) + pkg/
            (generated wasm bundle, gitignored).
```

Rule of thumb: if it decides anything, it belongs in `zink-client` (shared
with the CLI) or the command layer — never in `ui/`.

## Build pipeline

No npm, no trunk: `ui/build.sh` = `cargo build --target wasm32-unknown-unknown`
+ `wasm-bindgen` CLI into `dist/pkg/` (same flow as `web/spike`). Wired into
`beforeDevCommand`/`beforeBuildCommand` in `tauri.conf.json` (via
`git rev-parse --show-toplevel`, because Android builds run hooks from a
different cwd). Desktop: `cargo tauri dev` (needs system webkit2gtk). Android:
`cargo tauri android build --debug --target aarch64` (DEV-SETUP §3.6; APK
bloats if Gradle repackages in place — clean `gen/android/app/build/outputs`
when it grows).

All three crates are outside the workspace (root `Cargo.toml` excludes them):
desktop builds need system libs the workspace shouldn't demand, and `ui` is
wasm32-only.

## Testing

The command layer and UI have no automated tests — everything that decides is
in `zink-client`/`zink-protocol`, which the CLI e2e suite covers
(`crates/zink-cli/tests/`). Manual acceptance runs per slice are recorded in
`docs/design/mvp-build-plan.md`. Two desktop instances on one machine need
distinct app identifiers and `--no-watch` on the first, or they fight over the
state dir.
