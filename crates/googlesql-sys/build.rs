//! Links the GoogleSQL shim and generates prost types for the resolved AST.
//!
//! Where the shim comes from, first match wins (docs/building.md):
//! 1. `GOOGLESQL_SHIM_DIR` (a directory holding `libgooglesql_shim.so` and the generated protos
//!    under `external/googlesql+`), or `GOOGLESQL_PREFIX` (a `bin/` + `src/` layout, such as an
//!    unpacked prebuilt archive). `GOOGLESQL_SRC_DIR` overrides the googlesql source protos.
//! 2. A local Bazel build in `native/googlesql` (`bazel-bin`).
//! 3. The prebuilt archive pinned in `native/googlesql-shim.env`: from the download cache
//!    (`SPANBOLT_CACHE_DIR`, default `$XDG_CACHE_HOME/spanbolt` or `~/.cache/spanbolt`), else
//!    downloaded from `SPANBOLT_SHIM_URL` (default: the GitHub release; `file://` works) and
//!    checked against its SHA-256.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// Inputs of the shim build hashed into `SHIM_SOURCE_SHA256` (same list as scripts/package-shim.sh).
const SHIM_SOURCES: &[&str] = &[
    ".bazelrc",
    ".bazelversion",
    "BUILD.bazel",
    "MODULE.bazel",
    "shim",
    "tools",
];

struct Shim {
    /// Holds `libgooglesql_shim.so`, the generated protos under `external/googlesql+` and, in a
    /// prebuilt archive, `resolved_ast.fds` (so no protoc is needed).
    bin: PathBuf,
    /// googlesql's source tree (hand-written protos). Unused when `bin` has `resolved_ast.fds`.
    src: Option<PathBuf>,
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let native = manifest.join("../../native/googlesql");
    for var in [
        "GOOGLESQL_SHIM_DIR",
        "GOOGLESQL_SRC_DIR",
        "GOOGLESQL_PREFIX",
        "SPANBOLT_SHIM_URL",
        "SPANBOLT_CACHE_DIR",
        "SPANBOLT_SHIM_ALLOW_STALE",
        "PROTOC",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!(
        "cargo:rerun-if-changed={}",
        native.join("../googlesql-shim.env").display()
    );

    let shim = resolve_shim(&native);
    let bin = &shim.bin;
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir"));

    let lib_src = bin.join("libgooglesql_shim.so");
    let lib_name = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "libgooglesql_shim.dylib"
    } else {
        "libgooglesql_shim.so"
    };
    let lib_dst = out.join(lib_name);
    let _ = fs::remove_file(&lib_dst);
    fs::copy(&lib_src, &lib_dst).unwrap_or_else(|e| panic!("copy {}: {e}", lib_src.display()));
    println!("cargo:rerun-if-changed={}", lib_src.display());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=googlesql_shim");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", out.display());
    // Dependents add this directory to their binaries' rpath (DEP_GOOGLESQL_SHIM_LIB_DIR).
    println!("cargo:lib_dir={}", out.display());

    let fds_prebuilt = bin.join("resolved_ast.fds");
    let bytes = if fds_prebuilt.is_file() {
        println!("cargo:rerun-if-changed={}", fds_prebuilt.display());
        fs::read(&fds_prebuilt).expect("read resolved_ast.fds")
    } else {
        run_protoc(&shim, &native, &out)
    };
    let mut fds = <prost_types::FileDescriptorSet as prost::Message>::decode(bytes.as_slice())
        .expect("decode fds");
    // prost-build 0.14 rejects `edition = "2023"` files (protobuf's own descriptor.proto and a few
    // googlesql vector protos). Drop google/protobuf/* (prost-types covers them) and present the
    // edition files as proto2, whose explicit-presence default matches 2023.
    fds.file
        .retain(|f| !f.name().starts_with("google/protobuf/"));
    for f in &mut fds.file {
        if f.syntax() == "editions" {
            f.syntax = Some("proto2".into());
        }
    }
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("prost codegen for resolved_ast.proto");
}

/// Descriptor set of resolved_ast.proto and its imports. Generated protos (resolved_ast.proto,
/// resolved_node_kind.proto) live in the shim's bin directory, hand-written ones in the source tree
/// of the googlesql external repo.
fn run_protoc(shim: &Shim, native: &Path, out: &Path) -> Vec<u8> {
    let gen_root = shim.bin.join("external/googlesql+");
    let src_root = shim.src.clone().unwrap_or_else(|| {
        let exec_root = fs::canonicalize(native.join("bazel-googlesql")).expect(
            "bazel exec root symlink native/googlesql/bazel-googlesql; set GOOGLESQL_SRC_DIR",
        );
        exec_root.join("external/googlesql+")
    });
    let fds_path = out.join("googlesql.fds");
    let protoc = env::var("PROTOC").unwrap_or_else(|_| "protoc".into());
    let status = Command::new(&protoc)
        .arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", fds_path.display()))
        .arg(format!("-I{}", gen_root.display()))
        .arg(format!("-I{}", src_root.display()))
        .arg(gen_root.join("googlesql/resolved_ast/resolved_ast.proto"))
        .status()
        .unwrap_or_else(|e| {
            panic!("run {protoc} (protoc 27+ is needed without a prebuilt resolved_ast.fds): {e}")
        });
    assert!(status.success(), "protoc failed");
    fs::read(&fds_path).expect("read descriptor set")
}

fn resolve_shim(native: &Path) -> Shim {
    let canon = |p: PathBuf, what: &str| {
        fs::canonicalize(&p).unwrap_or_else(|e| panic!("{what}={}: {e}", p.display()))
    };
    let src_dir = env::var_os("GOOGLESQL_SRC_DIR").map(PathBuf::from);
    if let Some(dir) = env::var_os("GOOGLESQL_SHIM_DIR") {
        return Shim {
            bin: canon(dir.into(), "GOOGLESQL_SHIM_DIR"),
            src: src_dir
                .or_else(|| env::var_os("GOOGLESQL_PREFIX").map(|p| PathBuf::from(p).join("src"))),
        };
    }
    if let Some(prefix) = env::var_os("GOOGLESQL_PREFIX") {
        let prefix = canon(prefix.into(), "GOOGLESQL_PREFIX");
        return Shim {
            bin: prefix.join("bin"),
            src: Some(src_dir.unwrap_or_else(|| prefix.join("src"))),
        };
    }
    let bazel_bin = native.join("bazel-bin");
    if bazel_bin.join("libgooglesql_shim.so").is_file() {
        return Shim {
            bin: canon(bazel_bin, "bazel-bin"),
            src: src_dir,
        };
    }
    let prefix = prebuilt(native).unwrap_or_else(|e| {
        panic!(
            "\n\nNo GoogleSQL shim: {e}\n\n\
             Options (docs/building.md):\n\
             - point SPANBOLT_SHIM_URL at a directory or URL holding the pinned archive (file:// works);\n\
             - build the shim with Bazel (~25 min cold, needs bazelisk and clang):\n\
             \x20   cd native/googlesql && bazelisk build //:libgooglesql_shim.so\n\
             \x20 cargo then picks up native/googlesql/bazel-bin by itself;\n\
             - or set GOOGLESQL_PREFIX (bin/ + src/) or GOOGLESQL_SHIM_DIR to an existing build.\n"
        )
    });
    Shim {
        bin: prefix.join("bin"),
        src: Some(prefix.join("src")),
    }
}

/// The pinned prebuilt archive, unpacked in the download cache (downloading it if needed).
fn prebuilt(native: &Path) -> Result<PathBuf, String> {
    let pins = read_pins(&native.join("../googlesql-shim.env"))?;
    let pin = |k: &str| pins.get(k).filter(|v| !v.is_empty()).cloned();
    let gsql = pin("GOOGLESQL_VERSION").ok_or("googlesql-shim.env has no GOOGLESQL_VERSION")?;
    let shim_version = pin("SHIM_VERSION").ok_or("googlesql-shim.env has no SHIM_VERSION")?;
    let platform = platform()?;
    let sha = pin(&format!("SHA256_{}", platform.replace('-', "_"))).ok_or_else(|| {
        format!("no prebuilt shim pinned for {platform} in native/googlesql-shim.env")
    })?;

    // A prebuilt archive is only right for the sources it was built from.
    let want_src = pin("SHIM_SOURCE_SHA256").unwrap_or_default();
    let have_src = source_hash(native)?;
    if want_src != have_src && env::var_os("SPANBOLT_SHIM_ALLOW_STALE").is_none() {
        return Err(format!(
            "the shim sources in native/googlesql changed since the pinned archive was built \
             (SHIM_SOURCE_SHA256 {want_src:?}, tree {have_src}); package a new one \
             (scripts/package-shim.sh) or set SPANBOLT_SHIM_ALLOW_STALE=1 to use it anyway"
        ));
    }

    let stem = format!("googlesql-shim-{gsql}-s{shim_version}-{platform}");
    let archive_name = format!("{stem}.tar.gz");
    let cache = cache_dir()?.join("prebuilt");
    let dir = cache.join(format!("{stem}-{}", &sha[..12.min(sha.len())]));
    if dir.join("bin/libgooglesql_shim.so").is_file() {
        return Ok(dir);
    }
    fs::create_dir_all(&cache).map_err(|e| format!("create {}: {e}", cache.display()))?;

    let base = env::var("SPANBOLT_SHIM_URL").ok().unwrap_or_else(|| {
        let release = pin("SHIM_RELEASE_URL").unwrap_or_default();
        format!("{release}/shim-{gsql}-s{shim_version}")
    });
    let url = format!("{}/{archive_name}", base.trim_end_matches('/'));
    let tmp = cache.join(format!(".{archive_name}.{}", std::process::id()));
    let fetched = fetch(&url, &tmp).and_then(|()| {
        let got = sha256_file(&tmp)?;
        if got != sha {
            return Err(format!(
                "{url}: SHA-256 {got}, expected {sha} (googlesql-shim.env)"
            ));
        }
        unpack(&tmp, &cache, &dir)
    });
    let _ = fs::remove_file(&tmp);
    fetched?;
    println!("cargo:warning=using prebuilt GoogleSQL shim {stem} from {url}");
    Ok(dir)
}

fn read_pins(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect())
}

fn platform() -> Result<String, String> {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => Ok("darwin-arm64".into()),
        ("linux", "aarch64") => Ok("linux-arm64".into()),
        ("linux", "x86_64") => Ok("linux-x86_64".into()),
        _ => Err(format!("no prebuilt shim for {os}/{arch}")),
    }
}

fn cache_dir() -> Result<PathBuf, String> {
    if let Some(d) = env::var_os("SPANBOLT_CACHE_DIR") {
        return Ok(d.into());
    }
    if let Some(d) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(d).join("spanbolt"));
    }
    env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache/spanbolt"))
        .ok_or_else(|| "no cache directory: set SPANBOLT_CACHE_DIR".into())
}

/// `file://` and plain paths are copied; anything else goes through curl.
fn fetch(url: &str, dst: &Path) -> Result<(), String> {
    if let Some(path) = url
        .strip_prefix("file://")
        .or_else(|| (!url.contains("://")).then_some(url))
    {
        return fs::copy(path, dst)
            .map(drop)
            .map_err(|e| format!("copy {path}: {e}"));
    }
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(dst)
        .arg(url)
        .status()
        .map_err(|e| format!("run curl to fetch {url}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("download {url} failed ({status})"))
    }
}

/// Unpacks into a private directory, then renames it into place, so a build interrupted halfway
/// (or a concurrent one) never sees half an archive.
fn unpack(archive: &Path, cache: &Path, dir: &Path) -> Result<(), String> {
    let staging = cache.join(format!(".unpack.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("create {}: {e}", staging.display()))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&staging)
        .status()
        .map_err(|e| format!("run tar: {e}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("tar -xzf {} failed ({status})", archive.display()));
    }
    if !staging.join("bin/libgooglesql_shim.so").is_file() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "{} has no bin/libgooglesql_shim.so",
            archive.display()
        ));
    }
    if fs::rename(&staging, dir).is_err() {
        // Another build got there first.
        let _ = fs::remove_dir_all(&staging);
        if !dir.join("bin/libgooglesql_shim.so").is_file() {
            return Err(format!(
                "could not move the unpacked shim to {}",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 over `sha256sum`-style lines (`<hex>  <path>\n`) of every regular file in
/// `SHIM_SOURCES`, sorted by path: `scripts/package-shim.sh --source-hash` computes the same.
fn source_hash(native: &Path) -> Result<String, String> {
    fn walk(root: &Path, rel: &str, files: &mut Vec<String>) -> Result<(), String> {
        let path = root.join(rel);
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))? {
                let name = entry.map_err(|e| e.to_string())?.file_name();
                walk(root, &format!("{rel}/{}", name.to_string_lossy()), files)?;
            }
        } else if meta.is_file() {
            files.push(rel.to_string());
        }
        Ok(())
    }
    let mut files = Vec::new();
    for top in SHIM_SOURCES {
        walk(native, top, &mut files)?;
    }
    files.sort();
    let mut h = Sha256::new();
    for rel in &files {
        let path = native.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        h.update(format!("{}  {rel}\n", sha256_file(&path)?));
    }
    Ok(hex(&h.finalize()))
}
