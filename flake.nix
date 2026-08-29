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

          # Android SDK/NDK for the native phone client. The platform +
          # build-tools versions MUST match what `cargo tauri android`'s
          # generated Gradle project asks for (app/build.gradle.kts:
          # compileSdk/targetSdk, and AGP's default build-tools) — under Nix
          # the SDK lives read-only in /nix/store, so Gradle can't silently
          # auto-download a missing component the way it does on a writable
          # ~/android/sdk. Currently AGP 8.11 → compileSdk 36 / build-tools
          # 35.0.0. Bump these together when Tauri/AGP moves.
          ndkVersion = "27.1.12297006";
          buildToolsVersion = "35.0.0";
          androidComposition = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "36" ];
            buildToolsVersions = [ buildToolsVersion ];
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
            # GSettings schemas the GTK stack reads at runtime: gtk3 brings
            # org.gtk.Settings.FileChooser (the file picker), this one brings
            # org.gnome.desktop.interface (theme / dark mode). Listed
            # explicitly rather than relying on gtk3 propagating it — see the
            # XDG_DATA_DIRS note in the desktop shellHook.
            pkgs.gsettings-desktop-schemas
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
              # Non-NixOS hosts: Nix's glvnd/Mesa/gbm look for GPU drivers under
              # /run/opengl-driver, a NixOS-only path. Without these, WebKitGTK
              # aborts at startup ("Could not create default EGL display:
              # EGL_BAD_PARAMETER"). Point EGL + GBM at the nixpkgs Mesa instead,
              # which matches the rest of the shell's libraries. On NixOS the
              # path exists and the system drivers (incl. NVIDIA) take over.
              if [ ! -e /run/opengl-driver ]; then
                export __EGL_VENDOR_LIBRARY_FILENAMES="${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json"
                export LIBGL_DRIVERS_PATH="${pkgs.mesa}/lib/dri"
                export GBM_BACKENDS_PATH="${pkgs.mesa}/lib/gbm"
              fi

              # GIO finds GSettings schemas only under XDG_DATA_DIRS. nixpkgs'
              # glib setup hook collects them into GSETTINGS_SCHEMAS_PATH
              # instead, and normally wrapGAppsHook translates that into
              # XDG_DATA_DIRS when wrapping an installed app — but a mkShell
              # has no wrapper, so a bare `cargo tauri dev` binary sees none.
              # WebKitGTK's file picker (<input type="file">) then aborts the
              # whole process with "No GSettings schemas are installed on the
              # system". Do the translation ourselves. Also needed on NixOS:
              # the system profile carries no schemas for unwrapped binaries.
              if [ -n "''${GSETTINGS_SCHEMAS_PATH:-}" ]; then
                export XDG_DATA_DIRS="$GSETTINGS_SCHEMAS_PATH''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              fi

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

              # NixOS: the Android Gradle Plugin downloads its own aapt2 from
              # Maven — a dynamically-linked ELF that can't start here (no FHS
              # loader). Force AGP to use the SDK's autoPatchelf'd aapt2 instead.
              #
              # We pass the override as a Gradle project property via a JVM
              # system property (org.gradle.project.<name>) in GRADLE_OPTS. This
              # keeps the dependency cache in the default, shared ~/.gradle (like
              # ~/.cargo) while writing NO file: not into the git-tracked
              # gen/android/gradle.properties (the override is a machine-specific
              # /nix/store path that must never be committed), and not into a
              # global ~/.gradle config (which would leak into your other Gradle
              # projects). The setting lives only in this shell's environment.
              #
              # (A Gradle init script does NOT work for this: AGP reads the
              # property via providers.gradleProperty(), whose value is snapshot
              # from files/CLI/env before init scripts run, so mutating
              # startParameter.projectProperties there has no effect.)
              export GRADLE_OPTS="-Dorg.gradle.project.android.aapt2FromMavenOverride=${androidSdkRoot}/build-tools/${buildToolsVersion}/aapt2 ''${GRADLE_OPTS:-}"

              echo "zink android shell — sdk 36 / build-tools ${buildToolsVersion} / ndk ${ndkVersion} / jdk 21"
              echo "  smoke test:  cargo build -p zink-relay --lib --target aarch64-linux-android"
              echo "  app:         (cd app/src-tauri && cargo tauri android init && cargo tauri android build --debug --target aarch64)"
            '';
          };
        });

      # `nix build .#zink-relay` — the deployable relay binary, consumed by
      # server configs as a flake input. Built with the same pinned toolchain
      # as the dev shell so both agree on the compiler.
      packages = forAllSystems (pkgs:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        rec {
          zink-relay = rustPlatform.buildRustPackage {
            pname = "zink-relay";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildAndTestSubdir = "crates/zink-relay";
            # Tests run via nextest in dev/CI; deploy builds stay lean.
            # (build.rs finds no .git in the Nix sandbox, so --version
            # reports "unknown" — the documented tarball fallback.)
            doCheck = false;
            meta.mainProgram = "zink-relay";
          };
          default = zink-relay;
        });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
