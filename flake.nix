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
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
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

      in
      {
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
