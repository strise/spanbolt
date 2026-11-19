//! With the `googlesql` feature, puts the GoogleSQL shim on the binary's rpath: next to the
//! executable (distribution) and the build directory (development).

fn main() {
    if let Ok(dir) = std::env::var("DEP_SPANBOLT_SQL_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        let origin = if cfg!(target_os = "macos") {
            "@executable_path"
        } else {
            "$ORIGIN"
        };
        println!("cargo:rustc-link-arg=-Wl,-rpath,{origin}");
    }
}
