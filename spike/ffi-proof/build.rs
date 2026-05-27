// Link against the libjfs-amd64.so built from the JuiceFS source tree, and
// bake an rpath so the binary finds it at runtime without LD_LIBRARY_PATH.
use std::path::PathBuf;

fn main() {
    let lib_dir = PathBuf::from(env_or(
        "LIBJFS_DIR",
        "/home/cp/code/trove/spike/juicefs/sdk/java/libjfs",
    ));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=jfs-amd64");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
