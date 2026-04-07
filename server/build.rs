fn main() {
    let header_path = "/tmp/nv-codec-headers/include/ffnvcodec";

    println!("cargo:rerun-if-changed=nvenc_wrapper.c");

    cc::Build::new()
        .file("nvenc_wrapper.c")
        .include(header_path)
        .warnings(false)
        .compile("nvenc_wrapper");

    println!("cargo:rustc-link-lib=dl");
}
