// Link against JuiceFS's libjfs only when the `mount` feature is enabled, so
// the core crate (validator, `trove check`) builds with no native dependency.
//
// Platform matrix (matches upstream JuiceFS naming in
// `libjfs/build/juicefs/sdk/java/libjfs/Makefile`):
//
//   linux  / x86_64   →  libjfs-amd64.so      link name: jfs-amd64
//   linux  / aarch64  →  libjfs-arm64.so      link name: jfs-arm64
//   macos  / x86_64   →  libjfs-amd64.dylib   link name: jfs-amd64
//   macos  / aarch64  →  libjfs-arm64.dylib   link name: jfs-arm64
//
// The rustc link name is shared across OSes — the system linker resolves the
// extension (`.so` vs `.dylib`). The file on disk does change per OS, which is
// why the existence check below uses the full filename.
//
// LIBJFS_DIR overrides the search directory. Default is `libjfs/build/`
// (resolved against CARGO_MANIFEST_DIR), which is where `libjfs/build.sh`
// deposits the built artefact. On a fresh checkout the script hasn't been run
// yet, so the resolved file won't exist and we fail loud with a pointer to
// `libjfs/build.sh`. See `docs/packaging.md` for the release-engineering matrix.
fn main() {
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_none() {
        return;
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let (link_name, file_name) = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => ("jfs-amd64", "libjfs-amd64.so"),
        ("linux", "aarch64") => ("jfs-arm64", "libjfs-arm64.so"),
        ("macos", "x86_64") => ("jfs-amd64", "libjfs-amd64.dylib"),
        ("macos", "aarch64") => ("jfs-arm64", "libjfs-arm64.dylib"),
        _ => panic!(
            "trove: the `mount` feature is not supported on target {target_os}/{target_arch}. \
             Supported: linux/x86_64, linux/aarch64, macos/x86_64, macos/aarch64. \
             See docs/packaging.md."
        ),
    };

    let dir = std::env::var("LIBJFS_DIR").unwrap_or_else(|_| {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        std::path::Path::new(&manifest)
            .join("libjfs")
            .join("build")
            .to_string_lossy()
            .into_owned()
    });
    let lib_path = std::path::Path::new(&dir).join(file_name);
    if !lib_path.exists() {
        panic!(
            "trove: libjfs not found at {} — run `libjfs/build.sh` first \
             (or set LIBJFS_DIR to point at a pre-built libjfs directory). \
             See docs/packaging.md for build instructions for this platform.",
            lib_path.display()
        );
    }

    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=dylib={link_name}");
    // Runtime rpath: look beside the binary first (relocatable distribution).
    // On Linux that's $ORIGIN; on macOS it's @loader_path. The shipped tarball
    // places `trove` and `libjfs-<arch>.{so,dylib}` in the same directory, so
    // the loader resolves the dylib without LD_LIBRARY_PATH/DYLD_LIBRARY_PATH.
    let target_os_runtime = target_os.as_str();
    match target_os_runtime {
        "linux" => {
            // $ORIGIN is the dir containing the binary at runtime.
            // The `$` must reach the linker literally — Cargo doesn't expand it.
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        }
        "macos" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        }
        _ => {}
    }
    // Also emit the absolute build-time rpath as a fallback so `cargo test`
    // and other local builds keep working when the binary stays in target/.
    // The loader tries each -rpath in order; the relative one wins for the
    // shipped tarball, this one wins for developer builds.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    println!("cargo:rerun-if-env-changed=LIBJFS_DIR");
    println!("cargo:rerun-if-changed={}", lib_path.display());
}
