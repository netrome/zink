{
  description = "zink — a small, p2p-first chat protocol and its apps";

  # Dependency discipline (see CLAUDE.md): only two inputs. rust-overlay gives
  # us a pinned stable toolchain plus the wasm32 / android cross targets in one
  # place; everything else comes from nixpkgs. No flake-utils — the tiny
  # forAllSystems helper below is all we need.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, rust-overlay }:
    let
      # Dev happens on x86_64-linux; relays also run on aarch64 boxes.
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (pkgsFor system));

      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config = {
          # The Android SDK/NDK components are unfree and license-gated.
          allowUnfree = true;
          android_sdk.accept_license = true;
        };
      };
    in
    {
      devShells = forAllSystems (pkgs:
        let
          # The toolchain from rust-toolchain.toml, so the flake and a plain
          # rustup checkout agree on channel + targets + components.
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # wasm-bindgen CLI must EXACTLY match the wasm-bindgen crate in
          # Cargo.lock (DEV-SETUP §2 — currently 0.2.126). nixpkgs keeps
          # per-version attrs, so we pin the exact one instead of tracking the
          # floating `wasm-bindgen-cli`. Bump this alongside the Cargo.lock
          # crate (and DEV-SETUP §2).
          wasmBindgenCli = pkgs.wasm-bindgen-cli_0_2_126;

          # C toolchain bits that native crypto/networking crates need:
          # ring and blake3 compile C, and the relay's build.rs shells out to
          # git for `git describe`. Native C is the stdenv gcc (see wasmEnv for
          # the wasm cross-compiler).
          coreBuildInputs = [
            rustToolchain
            wasmBindgenCli
            pkgs.cargo-nextest
            pkgs.pkg-config
            pkgs.git
          ];

          # ring (pulled in via iroh's tls-ring on wasm) compiles C, and gcc
          # can't emit wasm objects. Point the wasm target's C compiler/archiver
          # at clang/llvm-ar so the WASM bundles link. Native builds keep using
          # the stdenv gcc — these are target-scoped env vars the `cc` crate
          # reads only for wasm32.
          wasmEnv = {
            CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
            AR_wasm32_unknown_unknown = "${pkgs.llvmPackages.bintools-unwrapped}/bin/llvm-ar";
          };

          # Android SDK/NDK for the native phone client. Versions pinned to
          # DEV-SETUP §3.2/§3.4: platform 34, build-tools 34.0.0, NDK 27.1.
          ndkVersion = "27.1.12297006";
          androidComposition = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "34" ];
            buildToolsVersions = [ "34.0.0" ];
            includeNDK = true;
            ndkVersions = [ ndkVersion ];
            abiVersions = [ "arm64-v8a" ]; # aarch64 phones (DEV-SETUP §3.4)
            includeEmulator = false;
            includeSystemImages = false;
          };
          androidSdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";
          ndkRoot = "${androidSdkRoot}/ndk/${ndkVersion}";
          # Google ships the NDK toolchain prebuilt for linux-x86_64 only.
          ndkBin = "${ndkRoot}/toolchains/llvm/prebuilt/linux-x86_64/bin";

          # Tauri desktop (Linux) system libraries — see
          # https://v2.tauri.app/start/prerequisites/
          desktopInputs = [
            pkgs.webkitgtk_4_1
            pkgs.libsoup_3
            pkgs.gtk3
            pkgs.cairo
            pkgs.pango
            pkgs.gdk-pixbuf
            pkgs.glib
            pkgs.atk
            pkgs.librsvg
            pkgs.openssl
          ];
        in
        {
          # `nix develop` — core crates, tests, clippy/fmt, and the WASM
          # bundles (web/spike + app/ui). The common case.
          default = pkgs.mkShell {
            packages = coreBuildInputs;
            env = wasmEnv // {
              RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            };
            shellHook = ''
              echo "zink dev shell — rust $(rustc --version | cut -d' ' -f2), wasm-bindgen $(wasm-bindgen --version | cut -d' ' -f2)"
              echo "  cargo nextest run / clippy / fmt   — core crates"
              echo "  ./web/spike/build.sh, ./app/ui/build.sh — WASM bundles"
              echo "  nix develop .#desktop | .#android  — app builds"
            '';
          };

          # `nix develop .#desktop` — build/run the Tauri desktop app.
          desktop = pkgs.mkShell {
            packages = coreBuildInputs ++ desktopInputs ++ [ pkgs.cargo-tauri ];
            # app/ui compiles to wasm too, so it needs the same clang bridge.
            env = wasmEnv;
            shellHook = ''
              echo "zink desktop shell — webkit2gtk + tauri-cli ready"
              echo "  (cd app/src-tauri && cargo tauri dev)"
            '';
          };
        }
        # Android cross-builds need the linux-x86_64 NDK toolchain, so this
        # shell only exists on x86_64-linux. First entry downloads the SDK
        # (~1.5 GB); `cargo tauri android` then pulls Gradle deps over the net.
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          android = pkgs.mkShell {
            packages = coreBuildInputs ++ [ pkgs.jdk21 pkgs.cargo-tauri ];
            env = wasmEnv // {
              JAVA_HOME = "${pkgs.jdk21}";
              ANDROID_HOME = androidSdkRoot;
              ANDROID_SDK_ROOT = androidSdkRoot;
              NDK_HOME = ndkRoot;
              ANDROID_NDK_ROOT = ndkRoot;
              # Raw `cargo build --target aarch64-linux-android` (DEV-SETUP
              # §3.5); `cargo tauri android` sets these itself but they're
              # harmless and make the smoke-test build work too.
              CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "${ndkBin}/aarch64-linux-android24-clang";
              CC_aarch64_linux_android = "${ndkBin}/aarch64-linux-android24-clang";
              AR_aarch64_linux_android = "${ndkBin}/llvm-ar";
            };
            shellHook = ''
              export PATH="${androidSdkRoot}/platform-tools:$PATH"
              echo "zink android shell — sdk 34 / ndk ${ndkVersion} / jdk 21"
              echo "  smoke test:  cargo build -p zink-relay --lib --target aarch64-linux-android"
              echo "  app:         (cd app/src-tauri && cargo tauri android init && cargo tauri android build --debug --target aarch64)"
            '';
          };
        });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
