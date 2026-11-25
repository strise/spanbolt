//! `proto:` blocks: `.proto` source compiled into the serialized `FileDescriptorSet` that
//! `CreateDatabase` and `UpdateDatabaseDdl` take as `proto_descriptors`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;

/// The descriptors a `proto:` body stands for. The body is `.proto` source, compiled with the
/// vendored `protoc` (imports of the well-known types work), or `bytes: <base64>` for raw bytes
/// that need not be a valid descriptor set (`bytes:` alone is empty). Compiled once per process
/// per distinct body.
pub fn descriptors(body: &[String]) -> Result<Arc<Vec<u8>>, String> {
    let text = body.join("\n");
    if let Some(b64) = text.trim().strip_prefix("bytes:") {
        return base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map(Arc::new)
            .map_err(|e| format!("proto bytes: {e}"));
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(d) = cache.lock().map_err(|e| e.to_string())?.get(&text) {
        return Ok(d.clone());
    }
    let out = Arc::new(compile(&text)?);
    cache
        .lock()
        .map_err(|e| e.to_string())?
        .insert(text, out.clone());
    Ok(out)
}

fn compile(source: &str) -> Result<Vec<u8>, String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    let dir = std::env::temp_dir().join(format!(
        "spanbolt-conformance-proto-{}-{:016x}",
        std::process::id(),
        h.finish()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let result = run_protoc(&dir, source);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn run_protoc(dir: &std::path::Path, source: &str) -> Result<Vec<u8>, String> {
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(|e| format!("protoc: {e}"))?;
    let include = protoc_bin_vendored::include_path().map_err(|e| format!("protoc: {e}"))?;
    std::fs::write(dir.join("case.proto"), source).map_err(|e| e.to_string())?;
    let out_file = dir.join("case.pb");
    let out = std::process::Command::new(protoc)
        .arg("--include_imports")
        .arg(format!("--proto_path={}", dir.display()))
        .arg(format!("--proto_path={}", include.display()))
        .arg(format!("--descriptor_set_out={}", out_file.display()))
        .arg("case.proto")
        .output()
        .map_err(|e| format!("protoc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "protoc: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::read(&out_file).map_err(|e| format!("{}: {e}", out_file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_source_and_takes_raw_bytes() {
        let src = [
            "syntax = \"proto2\";",
            "package a;",
            "message M { optional int64 x = 1; }",
        ]
        .map(String::from);
        let d = descriptors(&src).expect("compiles");
        let set = <prost_types::FileDescriptorSet as prost::Message>::decode(d.as_slice())
            .expect("a FileDescriptorSet");
        assert_eq!(set.file.len(), 1);
        assert_eq!(set.file[0].package.as_deref(), Some("a"));
        assert_eq!(
            *descriptors(&["bytes: AQI=".into()]).expect("raw"),
            vec![1u8, 2]
        );
        assert!(descriptors(&["bytes:".into()]).expect("empty").is_empty());
        assert!(descriptors(&["message {".into()]).is_err());
    }
}
