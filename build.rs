// Link against JuiceFS's libjfs only when the `mount` feature is enabled, so
// the core crate (validator, `trove check`) builds with no native dependency.
//
// LIBJFS_DIR points at the directory holding libjfs-amd64.so (built from the
// pinned JuiceFS source). Default is the spike location; override in CI.
fn main() {
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_none() {
        return;
    }
    let dir = std::env::var("LIBJFS_DIR")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/sdk/java/libjfs".to_string());
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=dylib=jfs-amd64");
    // rpath so binaries/tests find the .so at runtime without LD_LIBRARY_PATH.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    println!("cargo:rerun-if-env-changed=LIBJFS_DIR");
}
