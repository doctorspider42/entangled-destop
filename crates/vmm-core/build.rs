fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64")
    {
        println!("cargo:rerun-if-changed=src/hvf/simd.c");
        cc::Build::new().file("src/hvf/simd.c").compile("hvf_simd");
    }
}
