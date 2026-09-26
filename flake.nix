{
  description = "ddisplay – GPU-accelerated remote display (server + native client)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          # The Android SDK is unfree and its license must be accepted; both
          # are needed to compose the toolchain for the `android` dev shell.
          config = {
            allowUnfree = true;
            android_sdk.accept_license = true;
          };
        };
        inherit (pkgs) lib;

        # ---- Shared build inputs ----
        sharedNativeBuildInputs = with pkgs; [
          pkg-config
          rustc
          cargo
          clippy
          rustfmt
        ];

        # ---- Server dependencies ----
        serverBuildInputs = with pkgs; [
          # X11 / XCB (screen capture, input injection)
          libx11
          libxcb
          libxrandr
          libxfixes
          libxtst
          libxext
          libxi

          # NVENC wrapper compilation
          nv-codec-headers-12

          # OpenSSL (needed by some deps)
          openssl

          # C compiler for nvenc_wrapper.c (cc crate)
          gcc

          # Wayland backend: PipeWire capture (pipewire-rs)
          pipewire

          # bindgen (used by pipewire-sys) needs libclang
          llvmPackages.libclang
        ];

        # ---- Client-native dependencies ----
        clientBuildInputs = with pkgs; [
          # OpenSSL (for native-tls in tokio-tungstenite / reqwest)
          openssl

          # Windowing (winit) — X11 + Wayland
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
          wayland
          wayland-protocols
          libxkbcommon

          # GPU rendering (wgpu) — Vulkan + GLES
          vulkan-loader
          vulkan-headers
          libGL

          # Native file dialogs (rfd) — GTK3 backend on Linux
          gtk3
          glib
        ];

        # Runtime library path for dynamically loaded libs.
        # NVIDIA driver libs (libcuda, libnvidia-encode) live in /run/opengl-driver/lib
        # on NixOS and are loaded via dlopen — no build-time dependency needed.
        runtimeLibPath = pkgs.lib.makeLibraryPath (
          with pkgs;
          [
            vulkan-loader
            libGL
            wayland
            libxkbcommon
            libx11
            libxcb
            libxcursor
            libxrandr
            libxi
          ]
        );

        # Native Linux client, built straight from this workspace. The
        # workspace also holds the server (NVENC/PipeWire), so we build only
        # the `ddisplay-client` crate.
        ddisplay-client = pkgs.rustPlatform.buildRustPackage {
          pname = "ddisplay-client";
          version = "0.1.0";

          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          cargoBuildFlags = [
            "-p"
            "ddisplay-client"
          ];
          doCheck = false;

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
            nasm
          ];
          buildInputs = clientBuildInputs;

          # Link the system OpenSSL (native-tls) instead of vendoring.
          OPENSSL_NO_VENDOR = 1;

          # wgpu / winit / rfd dlopen their libs at runtime; the GPU driver
          # lives in /run/opengl-driver/lib on NixOS.
          postInstall = ''
            wrapProgram $out/bin/ddisplay-client \
              --prefix LD_LIBRARY_PATH : "${runtimeLibPath}:/run/opengl-driver/lib"
          '';

          meta = {
            description = "Native Linux client for ddisplay, a GPU-accelerated remote display";
            homepage = "https://github.com/Force67/ddisplay";
            license = lib.licenses.free;
            mainProgram = "ddisplay-client";
            platforms = lib.platforms.linux;
          };
        };

        # ---- Android client toolchain ----
        # Compose the Android SDK so aapt2 and the build-tools are autopatchelf'd
        # for NixOS (Google's own binaries are FHS-linked and fail otherwise).
        # Versions match client-android: compileSdk 36, AGP 9.2.1 -> build-tools
        # 36.0.0. buildToolsVersions must be a superset of what AGP requests.
        androidComposition = pkgs.androidenv.composeAndroidPackages {
          platformVersions = [ "36" ];
          buildToolsVersions = [ "36.0.0" ];
          platformToolsVersion = "35.0.2";
          includeEmulator = false;
          includeSystemImages = false;
          includeNDK = false;
        };
        androidSdk = androidComposition.androidsdk;
        androidHome = "${androidSdk}/libexec/android-sdk";

      in
      {
        # Toolchain for building and installing the Kotlin/Compose Android client.
        # Usage: `nix develop .#android` then, from client-android/,
        #   ./gradlew :app:assembleDebug
        # ANDROID_HOME and the aapt2 override are exported below, and adb is on PATH.
        devShells.android = pkgs.mkShell {
          buildInputs = [
            pkgs.jdk17
            androidSdk
          ];

          ANDROID_HOME = androidHome;
          ANDROID_SDK_ROOT = androidHome;
          JAVA_HOME = "${pkgs.jdk17}";

          shellHook = ''
            # Force AGP to use the patched aapt2 from the composed SDK instead of
            # downloading its own FHS-linked one (which cannot run on NixOS).
            export GRADLE_OPTS="-Dandroid.aapt2FromMavenOverride=${androidHome}/build-tools/36.0.0/aapt2 $GRADLE_OPTS"
            export PATH="${androidHome}/platform-tools:$PATH"
          '';
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = sharedNativeBuildInputs;
          buildInputs = serverBuildInputs ++ clientBuildInputs;

          shellHook = ''
            # Let wgpu / winit / rfd find dynamically loaded libs
            export LD_LIBRARY_PATH="${runtimeLibPath}:/run/opengl-driver/lib:$LD_LIBRARY_PATH"

            # Tell the server's build.rs where NVENC headers live
            export NVENC_HEADER_PATH="${pkgs.nv-codec-headers-12}/include/ffnvcodec"

            # bindgen (pipewire-sys) needs libclang
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
          '';
        };
      }
      // lib.optionalAttrs pkgs.stdenv.isLinux {
        packages.ddisplay-client = ddisplay-client;
        packages.default = ddisplay-client;
      }
    );
}
