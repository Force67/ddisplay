fn main() {
    // Prefer NVENC_HEADER_PATH env var (set by nix flake), then the vendored
    // header (API 13.0 — matches the driver on the dev box; newer headers make
    // OpenEncodeSessionEx fail with NV_ENC_ERR_INVALID_VERSION), then /tmp.
    let vendored = std::path::Path::new("third_party/ffnvcodec");
    let header_path = std::env::var("NVENC_HEADER_PATH").unwrap_or_else(|_| {
        if vendored.join("nvEncodeAPI.h").exists() {
            vendored.to_string_lossy().into_owned()
        } else {
            "/tmp/nv-codec-headers/include/ffnvcodec".to_string()
        }
    });

    println!("cargo:rerun-if-changed=nvenc_wrapper.c");
    println!("cargo:rerun-if-env-changed=NVENC_HEADER_PATH");

    cc::Build::new()
        .file("nvenc_wrapper.c")
        .include(&header_path)
        .warnings(false)
        .compile("nvenc_wrapper");

    println!("cargo:rustc-link-lib=dl");
}
