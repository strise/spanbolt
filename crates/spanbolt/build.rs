//! With the `googlesql` feature, puts the GoogleSQL shim on the binary's rpath, in this order: the
//! build directory (development), next to the executable (distribution: the image, a tarball) and
//! the download cache it came from (`cargo install`, whose build directory is deleted).

fn main() {
    let origin = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "@executable_path"
    } else {
        "$ORIGIN"
    };
    if let Ok(dir) = std::env::var("DEP_SPANBOLT_SQL_LIB_DIR") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{origin}");
    }
    if let Ok(dir) = std::env::var("DEP_SPANBOLT_SQL_CACHE_LIB_DIR") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
    }
}
