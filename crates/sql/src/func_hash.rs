//! hash functions.

use std::sync::Arc;

use spanbolt_storage::Value;

use crate::func::{ScalarFn, Sig, strict};

pub use spanbolt_ddl::names::fingerprint64;

fn input(v: &Value) -> &[u8] {
    match v {
        Value::String(s) => s.as_bytes(),
        Value::Bytes(b) => b,
        _ => &[],
    }
}

fn digest<D: md5::Digest>() -> ScalarFn {
    strict(|a| {
        let mut h = D::new();
        h.update(input(&a[0]));
        Ok(Value::Bytes(Arc::from(h.finalize().to_vec())))
    })
}

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    Some(match sig.name {
        "farm_fingerprint" => strict(|a| Ok(Value::Int64(fingerprint64(input(&a[0])) as i64))),
        "md5" => digest::<md5::Md5>(),
        "sha1" => digest::<sha1::Sha1>(),
        "sha256" => digest::<sha2::Sha256>(),
        "sha512" => digest::<sha2::Sha512>(),
        _ => return None,
    })
}
