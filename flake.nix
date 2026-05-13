{
  description = "ddisplay – GPU-accelerated remote display (server + native client)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

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
        runtimeLibPath = pkgs.lib.makeLibraryPath (with pkgs; [
          vulkan-loader
          libGL
          wayland
          libxkbcommon
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
        ]);

      in {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = sharedNativeBuildInputs;
          buildInputs = serverBuildInputs ++ clientBuildInputs;

          shellHook = ''
            # Let wgpu / winit / rfd find dynamically loaded libs
            export LD_LIBRARY_PATH="${runtimeLibPath}:/run/opengl-driver/lib:$LD_LIBRARY_PATH"

            # Tell the server's build.rs where NVENC headers live
            export NVENC_HEADER_PATH="${pkgs.nv-codec-headers-12}/include/ffnvcodec"
          '';
        };
      }
    );
}
