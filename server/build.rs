fn main() {
    // Prefer NVENC_HEADER_PATH env var (set by nix flake), fall back to /tmp checkout.
    let header_path = std::env::var("NVENC_HEADER_PATH")
        .unwrap_or_else(|_| "/tmp/nv-codec-headers/include/ffnvcodec".to_string());

    println!("cargo:rerun-if-changed=nvenc_wrapper.c");
    println!("cargo:rerun-if-env-changed=NVENC_HEADER_PATH");

    cc::Build::new()
        .file("nvenc_wrapper.c")
        .include(&header_path)
        .warnings(false)
        .compile("nvenc_wrapper");

    println!("cargo:rustc-link-lib=dl");
}
