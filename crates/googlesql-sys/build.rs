//! Links the GoogleSQL shim and generates prost types for the resolved AST.
//!
//! Where the shim comes from, first match wins (docs/building.md):
//! 1. `GOOGLESQL_SHIM_DIR` (a directory holding `libgooglesql_shim.so` and the generated protos
//!    under `external/googlesql+`), or `GOOGLESQL_PREFIX` (a `bin/` + `src/` layout, such as an
//!    unpacked prebuilt archive). `GOOGLESQL_SRC_DIR` overrides the googlesql source protos.
//! 2. A local Bazel build in `native/googlesql` (`bazel-bin`).
//! 3. The prebuilt archive pinned in `native/googlesql-shim.env` (`googlesql-shim.env` here is a
//!    symlink to it, stored as a file in the published crate): from the download cache
//!    (`SPANBOLT_CACHE_DIR`, default `$XDG_CACHE_HOME/spanbolt` or `~/.cache/spanbolt`), else
//!    downloaded from `SPANBOLT_SHIM_URL` (default: the GitHub release; `file://` works) and
//!    checked against its SHA-256.
//!
//! On docs.rs (`DOCS_RS`, no network) nothing is linked and the resolved-AST module is a stub.

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
    /// For a downloaded shim: a directory in the cache holding the library under the name the
    /// linker records (`libgooglesql_shim.dylib` on macOS), so a binary whose build directory is
    /// gone (`cargo install`) still finds it.
    cache_lib: Option<PathBuf>,
}

/// googlesql.rs on docs.rs: the real types are generated from the shim's descriptor set.
const DOCS_RS_STUB: &str = "\
/// The resolved AST of one statement. On docs.rs this is a stub: the real types are generated \
/// from the native shim's `resolved_ast.proto` at build time.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AnyResolvedStatementProto {}
";

/// The targets with a prebuilt shim, as (os, arch, platform name in the archive, target triple).
const PREBUILT: &[(&str, &str, &str, &str)] = &[
    ("macos", "aarch64", "darwin-arm64", "aarch64-apple-darwin"),
    (
        "linux",
        "aarch64",
        "linux-arm64",
        "aarch64-unknown-linux-gnu",
    ),
    (
        "linux",
        "x86_64",
        "linux-x86_64",
        "x86_64-unknown-linux-gnu",
    ),
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let native = manifest.join("../../native/googlesql");
    let pins = manifest.join("googlesql-shim.env");
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    if env::var_os("DOCS_RS").is_some() {
        fs::write(out.join("googlesql.rs"), DOCS_RS_STUB).expect("write stub");
        fs::write(out.join("resolved_ast.fds"), b"").expect("write stub descriptors");
        return;
    }
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
    println!("cargo:rerun-if-changed={}", pins.display());

    let shim = resolve_shim(&native, &pins);
    let bin = &shim.bin;

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
    // And this one, which outlives the build directory (DEP_GOOGLESQL_SHIM_CACHE_LIB_DIR).
    if let Some(dir) = &shim.cache_lib {
        println!("cargo:cache_lib_dir={}", dir.display());
    }

    let fds_prebuilt = bin.join("resolved_ast.fds");
    let bytes = if fds_prebuilt.is_file() {
        println!("cargo:rerun-if-changed={}", fds_prebuilt.display());
        fs::read(&fds_prebuilt).expect("read resolved_ast.fds")
    } else {
        run_protoc(&shim, &native, &out)
    };
    // Kept for walking a serialized resolved AST generically (`RESOLVED_AST_DESCRIPTORS`).
    fs::write(out.join("resolved_ast.fds"), &bytes).expect("write resolved_ast.fds");
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
    let generated = out.join("googlesql.rs");
    let code = fs::read_to_string(&generated).expect("read googlesql.rs");
    let code = DEEP_NODES
        .iter()
        .fold(code, |code, name| decode_without_depth_limit(code, name));
    let code = struct_field_without_depth_limit(code);
    let code = struct_value_without_depth_limit(array_type_with_depth_count(code));
    fs::write(&generated, code).expect("write googlesql.rs");
}

/// `StructFieldProto` as prost-build generates it.
const STRUCT_FIELD: &str = "#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StructFieldProto {
    #[prost(string, optional, tag = \"1\")]
    pub field_name: ::core::option::Option<::prost::alloc::string::String>,
    #[prost(message, optional, tag = \"2\")]
    pub field_type: ::core::option::Option<TypeProto>,
}
";

/// Replaces the derived `prost::Message` of `StructFieldProto` with one that decodes its type with
/// a fresh `DecodeContext` and counts its own depth instead (`deep` in lib.rs). A STRUCT level of a
/// `TypeProto` is three messages, so prost stopped at a type 32 STRUCTs deep, where the emulator
/// answers thousands.
fn struct_field_without_depth_limit(code: String) -> String {
    assert!(
        code.contains(STRUCT_FIELD),
        "generated StructFieldProto changed; update STRUCT_FIELD in build.rs"
    );
    let replacement = "#[derive(Clone, PartialEq, Debug, Default)]
pub struct StructFieldProto {
    pub field_name: ::core::option::Option<::prost::alloc::string::String>,
    pub field_type: ::core::option::Option<TypeProto>,
}
impl ::prost::Message for StructFieldProto {
    fn encode_raw(&self, buf: &mut impl ::prost::bytes::BufMut) {
        if let Some(name) = &self.field_name {
            ::prost::encoding::string::encode(1, name, buf);
        }
        if let Some(ty) = &self.field_type {
            ::prost::encoding::message::encode(2, ty, buf);
        }
    }
    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: ::prost::encoding::wire_type::WireType,
        buf: &mut impl ::prost::bytes::Buf,
        ctx: ::prost::encoding::DecodeContext,
    ) -> ::core::result::Result<(), ::prost::DecodeError> {
        match tag {
            1 => ::prost::encoding::string::merge(
                wire_type,
                self.field_name.get_or_insert_with(Default::default),
                buf,
                ctx,
            )
            .map_err(|mut e| {
                e.push(\"StructFieldProto\", \"field_name\");
                e
            }),
            2 => {
                let _level = crate::deep::Level::enter_type()?;
                ::prost::encoding::message::merge(
                    wire_type,
                    self.field_type.get_or_insert_with(Default::default),
                    buf,
                    ::prost::encoding::DecodeContext::default(),
                )
                .map_err(|mut e| {
                    e.push(\"StructFieldProto\", \"field_type\");
                    e
                })
            }
            _ => ::prost::encoding::skip_field(wire_type, tag, buf, ctx),
        }
    }
    fn encoded_len(&self) -> usize {
        self.field_name
            .as_ref()
            .map_or(0, |name| ::prost::encoding::string::encoded_len(1, name))
            + self
                .field_type
                .as_ref()
                .map_or(0, |ty| ::prost::encoding::message::encoded_len(2, ty))
    }
    fn clear(&mut self) {
        self.field_name = None;
        self.field_type = None;
    }
}
";
    code.replacen(STRUCT_FIELD, replacement, 1)
}

/// `ArrayTypeProto` as prost-build generates it.
const ARRAY_TYPE: &str = "#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ArrayTypeProto {
    #[prost(message, optional, boxed, tag = \"1\")]
    pub element_type: ::core::option::Option<::prost::alloc::boxed::Box<TypeProto>>,
}
";

/// Replaces the derived `prost::Message` of `ArrayTypeProto` with one that counts its element type
/// as a level of type, so that `deep::MAX` bounds how deep a type nests whatever it is made of.
fn array_type_with_depth_count(code: String) -> String {
    assert!(
        code.contains(ARRAY_TYPE),
        "generated ArrayTypeProto changed; update ARRAY_TYPE in build.rs"
    );
    let replacement = "#[derive(Clone, PartialEq, Debug, Default)]
pub struct ArrayTypeProto {
    pub element_type: ::core::option::Option<::prost::alloc::boxed::Box<TypeProto>>,
}
impl ::prost::Message for ArrayTypeProto {
    fn encode_raw(&self, buf: &mut impl ::prost::bytes::BufMut) {
        if let Some(ty) = &self.element_type {
            ::prost::encoding::message::encode(1, &**ty, buf);
        }
    }
    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: ::prost::encoding::wire_type::WireType,
        buf: &mut impl ::prost::bytes::Buf,
        ctx: ::prost::encoding::DecodeContext,
    ) -> ::core::result::Result<(), ::prost::DecodeError> {
        match tag {
            1 => {
                let _level = crate::deep::Level::enter_type()?;
                ::prost::encoding::message::merge(
                    wire_type,
                    &mut **self.element_type.get_or_insert_with(Default::default),
                    buf,
                    ::prost::encoding::DecodeContext::default(),
                )
                .map_err(|mut e| {
                    e.push(\"ArrayTypeProto\", \"element_type\");
                    e
                })
            }
            _ => ::prost::encoding::skip_field(wire_type, tag, buf, ctx),
        }
    }
    fn encoded_len(&self) -> usize {
        self.element_type
            .as_ref()
            .map_or(0, |ty| ::prost::encoding::message::encoded_len(1, &**ty))
    }
    fn clear(&mut self) {
        self.element_type = None;
    }
}
";
    code.replacen(ARRAY_TYPE, replacement, 1)
}

/// `ValueProto.Struct` as prost-build generates it.
const STRUCT_VALUE: &str = "    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Struct {
        #[prost(message, repeated, tag = \"1\")]
        pub field: ::prost::alloc::vec::Vec<super::ValueProto>,
    }
";

/// Replaces the derived `prost::Message` of a STRUCT value (`ValueProto.Struct`) with one that
/// decodes its fields with a fresh `DecodeContext`, counted as a level of type. The analyser folds
/// `STRUCT(STRUCT(1))` into a literal, two messages a level: prost stopped at 50.
fn struct_value_without_depth_limit(code: String) -> String {
    assert!(
        code.contains(STRUCT_VALUE),
        "generated ValueProto.Struct changed; update STRUCT_VALUE in build.rs"
    );
    let replacement = "    #[derive(Clone, PartialEq, Debug, Default)]
    pub struct Struct {
        pub field: ::prost::alloc::vec::Vec<super::ValueProto>,
    }
    impl ::prost::Message for Struct {
        fn encode_raw(&self, buf: &mut impl ::prost::bytes::BufMut) {
            for value in &self.field {
                ::prost::encoding::message::encode(1, value, buf);
            }
        }
        fn merge_field(
            &mut self,
            tag: u32,
            wire_type: ::prost::encoding::wire_type::WireType,
            buf: &mut impl ::prost::bytes::Buf,
            ctx: ::prost::encoding::DecodeContext,
        ) -> ::core::result::Result<(), ::prost::DecodeError> {
            match tag {
                1 => {
                    let _level = crate::deep::Level::enter_type()?;
                    ::prost::encoding::message::merge_repeated(
                        wire_type,
                        &mut self.field,
                        buf,
                        ::prost::encoding::DecodeContext::default(),
                    )
                    .map_err(|mut e| {
                        e.push(\"Struct\", \"field\");
                        e
                    })
                }
                _ => ::prost::encoding::skip_field(wire_type, tag, buf, ctx),
            }
        }
        fn encoded_len(&self) -> usize {
            ::prost::encoding::message::encoded_len_repeated(1, &self.field)
        }
        fn clear(&mut self) {
            self.field.clear();
        }
    }
";
    code.replacen(STRUCT_VALUE, replacement, 1)
}

/// The node wrappers every expression and scan of a resolved AST passes through.
const DEEP_NODES: &[&str] = &["AnyResolvedExprProto", "AnyResolvedScanProto"];

/// Replaces the derived `prost::Message` of the oneof wrapper `name` with one that decodes its node
/// with a fresh `DecodeContext` and counts its own depth instead (`deep` in lib.rs). prost stops at
/// 100 nested messages, and a function call in the resolved AST is four of them, so a statement
/// nested 34 calls deep didn't decode. Requests to the server keep prost's limit.
fn decode_without_depth_limit(code: String, name: &str) -> String {
    let derive = format!("#[derive(Clone, PartialEq, ::prost::Message)]\npub struct {name} {{\n");
    let start = code
        .find(&derive)
        .unwrap_or_else(|| panic!("generated {name} not found"));
    let body = start + derive.len();
    let end = body + code[body..].find("\n}\n").expect("end of struct") + 3;
    let fields = &code[body..end];
    let tags = fields
        .split("tags = \"")
        .nth(1)
        .and_then(|t| t.split('"').next())
        .unwrap_or_else(|| panic!("{name} is not a single oneof"));
    let node = fields
        .split("oneof = \"")
        .nth(1)
        .and_then(|t| t.split('"').next())
        .expect("oneof type");
    let tags = tags.replace(", ", " | ");
    let replacement = format!(
        "#[derive(Clone, PartialEq, Debug, Default)]
pub struct {name} {{
    pub node: ::core::option::Option<{node}>,
}}
impl ::prost::Message for {name} {{
    fn encode_raw(&self, buf: &mut impl ::prost::bytes::BufMut) {{
        if let Some(node) = &self.node {{
            node.encode(buf);
        }}
    }}
    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: ::prost::encoding::wire_type::WireType,
        buf: &mut impl ::prost::bytes::Buf,
        ctx: ::prost::encoding::DecodeContext,
    ) -> ::core::result::Result<(), ::prost::DecodeError> {{
        match tag {{
            {tags} => {{
                let _level = crate::deep::Level::enter()?;
                {node}::merge(
                    &mut self.node,
                    tag,
                    wire_type,
                    buf,
                    ::prost::encoding::DecodeContext::default(),
                )
                .map_err(|mut e| {{
                    e.push(\"{name}\", \"node\");
                    e
                }})
            }}
            _ => ::prost::encoding::skip_field(wire_type, tag, buf, ctx),
        }}
    }}
    fn encoded_len(&self) -> usize {{
        self.node.as_ref().map_or(0, {node}::encoded_len)
    }}
    fn clear(&mut self) {{
        self.node = None;
    }}
}}
"
    );
    format!("{}{replacement}{}", &code[..start], &code[end..])
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

fn resolve_shim(native: &Path, pins: &Path) -> Shim {
    let canon = |p: PathBuf, what: &str| {
        fs::canonicalize(&p).unwrap_or_else(|e| panic!("{what}={}: {e}", p.display()))
    };
    let src_dir = env::var_os("GOOGLESQL_SRC_DIR").map(PathBuf::from);
    if let Some(dir) = env::var_os("GOOGLESQL_SHIM_DIR") {
        return Shim {
            bin: canon(dir.into(), "GOOGLESQL_SHIM_DIR"),
            src: src_dir
                .or_else(|| env::var_os("GOOGLESQL_PREFIX").map(|p| PathBuf::from(p).join("src"))),
            cache_lib: None,
        };
    }
    if let Some(prefix) = env::var_os("GOOGLESQL_PREFIX") {
        let prefix = canon(prefix.into(), "GOOGLESQL_PREFIX");
        return Shim {
            bin: prefix.join("bin"),
            src: Some(src_dir.unwrap_or_else(|| prefix.join("src"))),
            cache_lib: None,
        };
    }
    let bazel_bin = native.join("bazel-bin");
    if bazel_bin.join("libgooglesql_shim.so").is_file() {
        return Shim {
            bin: canon(bazel_bin, "bazel-bin"),
            src: src_dir,
            cache_lib: None,
        };
    }
    if !native.join("shim").is_dir() {
        // A published crate: no Bazel sources, so the prebuilt archive or a given prefix.
        let prefix = prebuilt(native, pins).unwrap_or_else(|e| {
            panic!(
                "\n\nNo GoogleSQL shim: {e}\n\n\
                 spanbolt's SQL engine links a native library, prebuilt for {} (Linux: glibc 2.38+).\n\
                 Options:\n\
                 - on another target, run the container image (ghcr.io/strise/spanbolt), or build the\n\
                 \x20 shim from a git checkout with Bazel and set GOOGLESQL_PREFIX to it\n\
                 \x20 (https://github.com/strise/spanbolt/blob/main/docs/building.md);\n\
                 - behind a proxy or offline, point SPANBOLT_SHIM_URL at a directory or URL holding the\n\
                 \x20 pinned archive (file:// works);\n\
                 - `cargo install spanbolt --no-default-features` builds the server without SQL.\n",
                PREBUILT
                    .iter()
                    .map(|t| t.3)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        return prebuilt_shim(prefix);
    }
    let prefix = prebuilt(native, pins).unwrap_or_else(|e| {
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
    prebuilt_shim(prefix)
}

/// An unpacked archive in the cache, plus `lib/` holding the library under its install name.
fn prebuilt_shim(prefix: PathBuf) -> Shim {
    let name = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        "libgooglesql_shim.dylib"
    } else {
        "libgooglesql_shim.so"
    };
    let lib = prefix.join("lib");
    let link = lib.join(name);
    if !link.is_file() {
        // A relative symlink (no second 40 MB copy); racing builds create the same one.
        let _ = fs::create_dir_all(&lib);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink("../bin/libgooglesql_shim.so", &link);
        #[cfg(not(unix))]
        let _ = fs::copy(prefix.join("bin/libgooglesql_shim.so"), &link);
    }
    Shim {
        bin: prefix.join("bin"),
        src: Some(prefix.join("src")),
        cache_lib: link.is_file().then_some(lib),
    }
}

/// The pinned prebuilt archive, unpacked in the download cache (downloading it if needed).
fn prebuilt(native: &Path, pins: &Path) -> Result<PathBuf, String> {
    let pins = read_pins(pins)?;
    let pin = |k: &str| pins.get(k).filter(|v| !v.is_empty()).cloned();
    let gsql = pin("GOOGLESQL_VERSION").ok_or("googlesql-shim.env has no GOOGLESQL_VERSION")?;
    let shim_version = pin("SHIM_VERSION").ok_or("googlesql-shim.env has no SHIM_VERSION")?;
    let platform = platform()?;
    let sha = pin(&format!("SHA256_{}", platform.replace('-', "_"))).ok_or_else(|| {
        format!("no prebuilt shim pinned for {platform} in native/googlesql-shim.env")
    })?;

    // A prebuilt archive is only right for the sources it was built from. A published crate has
    // no sources to compare: the release checks the pin first (scripts/package-shim.sh --check).
    let want_src = pin("SHIM_SOURCE_SHA256").unwrap_or_default();
    let have_src = if native.join("shim").is_dir() {
        source_hash(native)?
    } else {
        want_src.clone()
    };
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
    PREBUILT
        .iter()
        .find(|t| t.0 == os && t.1 == arch)
        .map(|t| t.2.to_string())
        .ok_or_else(|| {
            format!(
                "no prebuilt shim for {}",
                env::var("TARGET").unwrap_or_default()
            )
        })
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
