# Developer Setup

Everything needed to build and test zink on a fresh Linux machine (headless is
fine — no GUI tools required anywhere). Reproduces the exact toolchain used in
development; versions are pinned where the ecosystem makes that matter.

## 1. Core (all crates, all tests)

- **Rust** (stable, ≥ 1.97, edition 2024) via [rustup](https://rustup.rs).
- That's it: `cargo build && cargo test` from the repo root.

```sh
cargo build
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

## 2. WASM (browser client, `web/spike`)

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
```

⚠️ The `wasm-bindgen` CLI version must **exactly match** the `wasm-bindgen`
crate version in `Cargo.lock` (currently 0.2.126) — check with
`cargo tree -i wasm-bindgen` after dependency bumps, and reinstall the CLI to
match.

Build the browser spike bundle: `./web/spike/build.sh`.

## 3. Android (native phone client)

No Android Studio needed — everything installs from the command line into
`~/android/`. Total download ≈ 1.5 GB.

### 3.1 JDK 21 (required by `sdkmanager` and Gradle)

```sh
mkdir -p ~/android && cd ~/android
curl -sL -o jdk.tar.gz \
  "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jdk/hotspot/normal/eclipse"
tar xzf jdk.tar.gz && rm jdk.tar.gz && mv jdk-21* jdk
```

### 3.2 Android SDK command-line tools + packages

```sh
cd ~/android
curl -sL -o cmdtools.zip \
  "https://dl.google.com/android/repository/commandlinetools-linux-13114758_latest.zip"
mkdir -p sdk/cmdline-tools
unzip -q cmdtools.zip -d sdk/cmdline-tools
mv sdk/cmdline-tools/cmdline-tools sdk/cmdline-tools/latest
rm cmdtools.zip

export JAVA_HOME=~/android/jdk
yes | sdk/cmdline-tools/latest/bin/sdkmanager --licenses
sdk/cmdline-tools/latest/bin/sdkmanager \
  "platform-tools" "platforms;android-34" "build-tools;34.0.0" "ndk;27.1.12297006"
```

### 3.3 Environment (add to your shell rc)

```sh
export JAVA_HOME="$HOME/android/jdk"
export ANDROID_HOME="$HOME/android/sdk"
export NDK_HOME="$ANDROID_HOME/ndk/27.1.12297006"
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
```

### 3.4 Rust target

```sh
rustup target add aarch64-linux-android   # 64-bit ARM — every modern phone
```

(Add `armv7-linux-androideabi`, `x86_64-linux-android` only if you need old
devices or an emulator.)

### 3.5 Smoke test the cross-compile

Tauri configures the NDK toolchain automatically during `tauri android build`;
for a raw `cargo` cross-build the three env vars below do the same job (the
`CC`/`AR` pair is what C-code build scripts like blake3's look for):

```sh
NDK_BIN="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android24-clang"
export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android24-clang"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"

cargo build -p zink-protocol --target aarch64-linux-android   # all the crypto
cargo build -p zink-relay --lib --target aarch64-linux-android # iroh + tokio + ring
```

Both must finish clean — they prove the whole crypto and networking stack
cross-compiles before any app scaffolding enters the picture.

### 3.6 Building the app (`app/`)

```sh
cargo install tauri-cli --locked            # once
cd app/src-tauri
cargo tauri android init                    # once per checkout (generates gen/android)
cargo tauri android build --debug --target aarch64
# → gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
```

The webview UI is a Leptos crate (`app/ui`, wasm32-only) compiled into
`app/dist/pkg/` by `app/ui/build.sh` — the same `wasm-bindgen` CLI flow as
`web/spike` (§2), no extra toolchain. `tauri.conf.json` runs the script
automatically before `cargo tauri dev` / `build`; run it by hand after UI
changes if you sideload APKs some other way. `app/dto` holds the command
wire types shared by `app/src-tauri` and `app/ui`.

Notes:
- The app crate is **excluded from the workspace** — its *desktop* build needs
  system webkit2gtk (see [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)
  for the `apt` packages; only needed on machines building the desktop app).
  Android builds need nothing beyond §3.1–3.4.
- Debug APKs are auto-signed and sideloadable; release builds need a signing
  config (not set up yet).
- The app's `Cargo.toml` sets `[profile.dev] debug = false, strip = "debuginfo"` —
  without it the debug APK is ~350 MB of Rust debuginfo. Debug via `adb logcat`.
- Gradle repackages APKs **in place**: after big dependency changes the APK can
  carry dead space from stale entries. `rm -rf gen/android/app/build/outputs`
  and rebuild to compact it.

### 3.7 Deploying to a phone

Two ways to get the APK onto a device:

**Over USB (adb):** enable *Developer options → USB debugging* on the phone,
then `adb devices` (or `adb pair` for wireless debugging), and
`adb install <apk>`.

**Over HTTP (no cable — how it's been done in dev):** serve the APK from the
build machine and download it on the phone's browser. From the repo root:

```sh
APK=app/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
# serve just that file's directory on :8080 (Ctrl-C to stop — don't leave it running)
python3 -m http.server 8080 --directory "$(dirname "$APK")"
# on the phone, browse to http://<build-machine-ip>:8080/app-universal-debug.apk
```

Android warns about installing from an unknown source (debug builds are
sideloadable; "install anyway"). Reinstalling a newer build over the same
`identifier` upgrades in place — device key and data survive. **Stop the
server when done** (`Ctrl-C`, or `pkill -f 'http.server 8080'`) rather than
leaving a stray listener bound.

First launch (C4c) asks for two things: **notification permission**
(Android 13+) and the **battery-optimization exemption** — grant both, or
background delivery will stall under Doze (live-delivery.md §5). The
persistent "zink is connected" notification is the foreground service that
keeps live delivery running; it's minimum-importance and collapses out of
the way.

## 4. Optional

- **Node.js ≥ 20** — only for the browser/service-worker unit tests
  (`node --test`, see STYLE.md); no npm packages needed.

## 5. Deploying the relay

On any Linux server with a public IP — no domain, TLS, or root needed
(one `sudo` for lingering aside):

```sh
cargo build --release -p zink-relay
cp target/release/zink-relay ~/.local/bin/
cp deploy/zink-relay.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now zink-relay
sudo loginctl enable-linger $USER     # start on boot without a login session
journalctl --user -u zink-relay | grep 'relay spec:'   # what clients paste
                                                       # into their profile
```

⚠️ The unit runs `~/.local/bin/zink-relay` — `cargo install` puts binaries in
`~/.cargo/bin`, which the service never looks at. Deploy with the `cp` above.
To verify what's actually deployed vs. built:

```sh
~/.local/bin/zink-relay --version      # what the service will run
target/release/zink-relay --version    # what you just built
journalctl --user -u zink-relay -n 5   # the running relay logs its version
                                       # + build commit on every start
```

- Data (mailboxes, blob cache, the relay's identity key) lives in
  `~/zink-relay-data`; the endpoint id, `--port 4400` (mailbox QUIC/UDP), and
  `--relay-port 4401` (embedded iroh relay server, plain HTTP/TCP — D0b peer
  rendezvous; clients home to it) are stable, so the printed relay spec
  `<id>@<ip:4400>#http://<ip>:4401` survives restarts and reboots. Both ports
  must be reachable (4400/udp, 4401/tcp).
- New key files (`relay.key`, client `device.key`) are written `0600`. A key
  created before that change keeps its old mode — `chmod 600` it once.
- Abuse caps are compiled-in defaults for now: 30-day mailbox retention,
  1024 items per mailbox, 30-day blob TTL, 64 MiB max blob (oversized pushes
  are *evicted on the next sweep* — iroh-blobs 0.103 cannot reject a push
  mid-stream).
