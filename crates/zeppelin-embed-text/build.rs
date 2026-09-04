fn main() {
    println!("cargo:rerun-if-changed=objc/ze_coreml.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("objc/ze_coreml.m")
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .compile("ze_coreml");
    println!("cargo:rustc-link-lib=framework=CoreML");
    println!("cargo:rustc-link-lib=framework=Foundation");
}
