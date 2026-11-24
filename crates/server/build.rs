//! With the `googlesql` feature, puts the GoogleSQL shim on the rpath of this crate's tests and
//! examples. The `spanbolt` binary (crates/spanbolt) does the same for itself.

fn main() {
    if let Ok(dir) = std::env::var("DEP_SPANBOLT_SQL_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
