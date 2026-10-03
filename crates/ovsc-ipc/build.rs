//! Compiles the XPC shim (c/ovshim.c) for macOS targets. Other targets have
//! no XPC and need no C code.

fn main() {
    println!("cargo:rerun-if-changed=c/ovshim.c");
    println!("cargo:rerun-if-changed=c/ovshim.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("c/ovshim.c")
            .flag("-fblocks")
            .flag("-Wall")
            .flag("-Wextra")
            .flag("-Werror")
            .compile("ovshim");
    }
}
