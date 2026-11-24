//! Puts the GoogleSQL shim on the rpath of this crate's tests and re-exports where it lives
//! (`DEP_SPANBOLT_SQL_LIB_DIR`, and `DEP_SPANBOLT_SQL_CACHE_LIB_DIR` for a downloaded shim).

fn main() {
    if let Ok(dir) = std::env::var("DEP_GOOGLESQL_SHIM_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        println!("cargo:lib_dir={dir}");
    }
    if let Ok(dir) = std::env::var("DEP_GOOGLESQL_SHIM_CACHE_LIB_DIR") {
        println!("cargo:cache_lib_dir={dir}");
    }
}
