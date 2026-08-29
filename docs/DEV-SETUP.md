# Developer Setup

Everything needed to build and test zink on a fresh Linux machine (headless is
fine — no GUI tools required anywhere). Reproduces the exact toolchain used in
development; versions are pinned where the ecosystem makes that matter.

Two ways to get the toolchain: **Nix** (§0, reproducible, one command) or the
**manual rustup + downloads** path (§1 onward). The manual sections remain the
source of truth for *what* each target needs; the flake just assembles it.

## 0. Nix (recommended on NixOS / any machine with Nix + flakes)

`flake.nix` pins the whole toolchain via `rust-toolchain.toml` and
`flake.lock` — same Rust (1.97, edition 2024), same `wasm-bindgen` 0.2.126,
same Android SDK 34 / NDK 27.1. Three shells, layered so the common case is
light:

```sh
nix develop            # core + WASM: cargo build / nextest run / clippy / fmt, ./web/spike/build.sh
nix develop .#desktop  # + webkit2gtk + tauri-cli — the Tauri desktop app
nix develop .#android  # + JDK 21 + Android SDK/NDK + aarch64 target (x86_64-linux only)
```

Each shell exports the env the corresponding manual section sets by hand (the
wasm clang bridge for `ring`, and the Android `JAVA_HOME`/`NDK_HOME`/linker
vars of §3.3/§3.5). The `default` and `desktop` shells work on both
`x86_64-linux` and `aarch64-linux`; the `android` shell is `x86_64-linux` only
(Google ships the NDK toolchain prebuilt for that host). First `.#android`
entry downloads the SDK (~1.5 GB); `cargo tauri android` then pulls Gradle
deps over the network as usual.

On a **non-NixOS host** (Nix installed on Ubuntu/TUXEDO/etc.), Nix's
glvnd/Mesa look for GPU drivers under `/run/opengl-driver` — a NixOS-only
path — so the desktop app would abort at startup with `Could not create
default EGL display: EGL_BAD_PARAMETER`. The `desktop` shell detects the
missing path and points EGL/GBM at the nixpkgs Mesa instead; nothing to do by
hand. (Note the shells are layered by *purpose*, not supersets: `.#android`
does not include the desktop GTK libs.)

The `desktop` shell also folds `GSETTINGS_SCHEMAS_PATH` into `XDG_DATA_DIRS`.
GIO only looks for GSettings schemas under `XDG_DATA_DIRS`; installed GTK apps
get that translation from `wrapGAppsHook`, but a dev shell has no wrapper, so
without it WebKitGTK's file picker (`<input type="file">`, e.g. choosing a
profile picture) kills the process with `No GSettings schemas are installed on
the system`. Again nothing to do by hand.

If you're not on Nix, follow the manual sections below instead.

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
  "platform-tools" "platforms;android-36" "build-tools;35.0.0" "ndk;27.1.12297006"
```

⚠️ The `platforms;android-N` and `build-tools;N` versions must match what
`cargo tauri android`'s generated Gradle project requests — see
`app/src-tauri/gen/android/app/build.gradle.kts` (`compileSdk`/`targetSdk`)
and the Android Gradle Plugin version in `gen/android/build.gradle.kts`
(AGP picks a default build-tools). Currently AGP 8.11 → `compileSdk 36`,
`build-tools;35.0.0`. On this writable SDK a mismatch just makes Gradle
download the missing component on first build; **under the Nix flake (§0)
the SDK is read-only, so the flake must pin these exactly** — bump both
places together when Tauri/AGP moves.

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
- Debug APKs are auto-signed and sideloadable; signed release builds are §3.8.
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

### 3.8 Making a signed release

Release builds are split across two roles so signing material never touches
the build machine:

- **Build machine** (full Android toolchain, §3.1–3.6 or the Nix `.#android`
  shell) produces an *unsigned* release APK.
- **Signing machine** (holds the release keystore; needs only `apksigner`)
  signs it, prompting for the keystore password each time — no password is
  stored anywhere.

**One-time: create the release keystore** (any machine with a JDK; it lives
on the signing machine only):

```sh
keytool -genkeypair -v -keystore zink-release.jks \
  -alias zink -keyalg RSA -keysize 2048 -validity 10000
chmod 600 zink-release.jks
```

The name/organization prompts build the certificate's Distinguished Name —
cosmetic, immutable, and embedded (publicly readable) in every signed APK;
a name or "zink" is fine. **Back the keystore up.** Android identifies the
app by this certificate forever: lose it and every phone must
uninstall/reinstall, wiping its device key — a new certificate, even one made
from the same key, counts as a different app.

**Per release:**

1. Bump `version` in `app/src-tauri/tauri.conf.json`. Android derives
   `versionCode` from it (major·1 000 000 + minor·1 000 + patch), and it must
   increase or phones refuse the upgrade.
2. Build machine — build unsigned, then confirm alignment (AGP aligns during
   packaging; the check is belt-and-braces, using build-tools' zipalign):

   ```sh
   (cd app/src-tauri && cargo tauri android build --apk --target aarch64)
   APK=app/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk
   "$ANDROID_HOME"/build-tools/*/zipalign -c -P 16 4 "$APK" && echo aligned
   ```

3. Signing machine — fetch, sign (prompts for the keystore password), verify:

   ```sh
   scp <build-machine>:<path-to>/app-universal-release-unsigned.apk .
   nix shell nixpkgs#apksigner        # or: sdkmanager "build-tools;35.0.0"
   apksigner sign --ks zink-release.jks --ks-key-alias zink \
     --out zink-<version>.apk app-universal-release-unsigned.apk
   apksigner verify --print-certs zink-<version>.apk
   ```

4. Distribute: attach the APK to a GitHub release
   (`gh release create v<version> zink-<version>.apk`); phones running
   [Obtainium](https://github.com/ImranR98/Obtainium) pointed at the repo get
   notified and can install updates. Serving over HTTP as in §3.7 works too —
   either way upgrades are protected by Android's same-signature rule.

Signed releases upgrade in place (app data and device key survive). A
**debug ↔ release switch** is a signature change: uninstall first, which
wipes app data — the phone gets a fresh device identity and must re-pair
(and be re-added to a relay allow-list, §5).

In-Gradle signing also works on a machine that holds the keystore: put a
`keystore.properties` at `app/src-tauri/gen/android/` (the path is
gitignored):

```properties
keyAlias=zink
password=<keystore password>
storeFile=/absolute/path/to/zink-release.jks
```

Without it — the normal case — the release build emits
`app-universal-release-unsigned.apk`.

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
  `<id>@<ip:4400>#http://<ip>:4401` survives restarts and reboots. Since De2
  the relay also answers QUIC address discovery (QAD) on **UDP at the
  `--relay-port` number** (the same-port convention clients derive it by) —
  so 4400/udp, 4401/tcp **and 4401/udp** must all be reachable. QAD failing
  is soft but slow: clients fall back to the pre-De2 behavior (~3 s stall on
  every open, disco-only address discovery).
- New key files (`relay.key`, client `device.key`) are written `0600`. A key
  created before that change keeps its old mode — `chmod 600` it once.
- Abuse caps: 30-day mailbox retention, 1024 items **and 8 MiB** per mailbox,
  30-day blob TTL, 64 MiB max blob (oversized pushes are *evicted on the next
  sweep* — iroh-blobs 0.103 cannot reject a push mid-stream), and a **total
  blob budget** (default 2 GiB).
- **Bounding the data dir (R1/R2).** The relay prints both ceilings and their
  sum on every start, so you never read source to learn the worst case:

  ```
  mailboxes: allow-list /…/allowed-keys · max 128 · ≤ 8 MiB each → ≤ 1024 MiB total
  blobs: ungated pushes, ≤ 64 MiB each · budget 2048 MiB (oldest evicted first)
  → data dir bounded at ≈ 3072 MiB total
  ```

  `--blob-budget <MiB>` moves the blob half; `--max-mailboxes` the other.
  Blob pushes are **deliberately ungated** — a sender pushing an image for
  your friend is legitimate and will not be on any allow-list — so bytes,
  not identity, are what bound them. Over budget, the hourly sweep evicts
  oldest-pushed first. On a box running other services, also gate
  registration to people you know:

  ```sh
  # one hex key per line; '#' comments. `zink-cli pubkey <key-file>` prints one.
  printf '%s   # alice laptop\n' "$ALICE_KEY" >> ~/zink-relay-allowed
  # then add to the unit's ExecStart:
  #   --allow-list %h/zink-relay-allowed [--max-mailboxes 32]
  ```

  The file is **re-read on every registration**, so appending a friend takes
  effect immediately — no restart, no reload signal. A **missing or unreadable
  file permits nobody** (fails closed), so a typo'd path locks the relay down
  rather than silently reopening it. Without `--allow-list` the relay is open
  to any key and bounded only by `--max-mailboxes` (default 128), which is
  first-come-first-served — fine for a throwaway, not for a shared box.
- **What the ceiling does *not* cover:** a full mailbox silently skips new
  deposits, so someone who fills a friend's 8 MiB can crowd out their real
  mail until they drain. Bounding storage is what creates that; the fix is
  SPEC §8's deferred capability grant, not a bigger cap. Reaching it needs
  their device key *and* their relay's dial string — i.e. their QR — so
  there is no internet-scannable surface today.
