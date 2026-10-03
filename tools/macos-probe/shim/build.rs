fn main() {
    println!("cargo:rerun-if-changed=c/ovshim.c");
    println!("cargo:rerun-if-changed=c/ovshim.h");
    cc::Build::new()
        .file("c/ovshim.c")
        .flag("-fblocks")
        .flag("-Wall")
        .flag("-Wextra")
        .flag("-Werror")
        .compile("ovshim");
}
