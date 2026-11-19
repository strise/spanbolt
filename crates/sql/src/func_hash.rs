//! hash functions.

use std::sync::Arc;

use spanbolt_storage::Value;

use crate::func::{ScalarFn, Sig, strict};

const K0: u64 = 0xc3a5_c85c_97cb_3127;
const K1: u64 = 0xb492_b66f_be98_f273;
const K2: u64 = 0x9ae1_6a3b_2f90_404f;

fn fetch64(s: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    if let Some(x) = s.get(i..i + 8) {
        b.copy_from_slice(x);
    }
    u64::from_le_bytes(b)
}

fn fetch32(s: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 4];
    if let Some(x) = s.get(i..i + 4) {
        b.copy_from_slice(x);
    }
    u32::from_le_bytes(b) as u64
}

fn rot(v: u64, s: u32) -> u64 {
    v.rotate_right(s)
}

fn shift_mix(v: u64) -> u64 {
    v ^ (v >> 47)
}

fn hash_len16(u: u64, v: u64, mul: u64) -> u64 {
    let mut a = (u ^ v).wrapping_mul(mul);
    a ^= a >> 47;
    let mut b = (v ^ a).wrapping_mul(mul);
    b ^= b >> 47;
    b.wrapping_mul(mul)
}

fn len_0_to_16(s: &[u8]) -> u64 {
    let len = s.len();
    if len >= 8 {
        let mul = K2.wrapping_add(len as u64 * 2);
        let a = fetch64(s, 0).wrapping_add(K2);
        let b = fetch64(s, len - 8);
        let c = rot(b, 37).wrapping_mul(mul).wrapping_add(a);
        let d = rot(a, 25).wrapping_add(b).wrapping_mul(mul);
        return hash_len16(c, d, mul);
    }
    if len >= 4 {
        let mul = K2.wrapping_add(len as u64 * 2);
        let a = fetch32(s, 0);
        return hash_len16((len as u64).wrapping_add(a << 3), fetch32(s, len - 4), mul);
    }
    if len > 0 {
        let (a, b, c) = (s[0] as u32, s[len >> 1] as u32, s[len - 1] as u32);
        let y = a.wrapping_add(b << 8);
        let z = (len as u32).wrapping_add(c << 2);
        return shift_mix((y as u64).wrapping_mul(K2) ^ (z as u64).wrapping_mul(K0))
            .wrapping_mul(K2);
    }
    K2
}

fn len_17_to_32(s: &[u8]) -> u64 {
    let len = s.len();
    let mul = K2.wrapping_add(len as u64 * 2);
    let a = fetch64(s, 0).wrapping_mul(K1);
    let b = fetch64(s, 8);
    let c = fetch64(s, len - 8).wrapping_mul(mul);
    let d = fetch64(s, len - 16).wrapping_mul(K2);
    hash_len16(
        rot(a.wrapping_add(b), 43)
            .wrapping_add(rot(c, 30))
            .wrapping_add(d),
        a.wrapping_add(rot(b.wrapping_add(K2), 18)).wrapping_add(c),
        mul,
    )
}

fn len_33_to_64(s: &[u8]) -> u64 {
    let len = s.len();
    let mul = K2.wrapping_add(len as u64 * 2);
    let a = fetch64(s, 0).wrapping_mul(K2);
    let b = fetch64(s, 8);
    let c = fetch64(s, len - 8).wrapping_mul(mul);
    let d = fetch64(s, len - 16).wrapping_mul(K2);
    let y = rot(a.wrapping_add(b), 43)
        .wrapping_add(rot(c, 30))
        .wrapping_add(d);
    let z = hash_len16(
        y,
        a.wrapping_add(rot(b.wrapping_add(K2), 18)).wrapping_add(c),
        mul,
    );
    let e = fetch64(s, 16).wrapping_mul(mul);
    let f = fetch64(s, 24);
    let g = y.wrapping_add(fetch64(s, len - 32)).wrapping_mul(mul);
    let h = z.wrapping_add(fetch64(s, len - 24)).wrapping_mul(mul);
    hash_len16(
        rot(e.wrapping_add(f), 43)
            .wrapping_add(rot(g, 30))
            .wrapping_add(h),
        e.wrapping_add(rot(f.wrapping_add(a), 18)).wrapping_add(g),
        mul,
    )
}

fn weak_hash_32(s: &[u8], i: usize, a: u64, b: u64) -> (u64, u64) {
    let (w, x, y, z) = (
        fetch64(s, i),
        fetch64(s, i + 8),
        fetch64(s, i + 16),
        fetch64(s, i + 24),
    );
    let mut a = a.wrapping_add(w);
    let mut b = rot(b.wrapping_add(a).wrapping_add(z), 21);
    let c = a;
    a = a.wrapping_add(x).wrapping_add(y);
    b = b.wrapping_add(rot(a, 44));
    (a.wrapping_add(z), b.wrapping_add(c))
}

/// Google's `Fingerprint64` (farmhashna::Hash64).
pub fn fingerprint64(s: &[u8]) -> u64 {
    let len = s.len();
    if len <= 16 {
        return len_0_to_16(s);
    }
    if len <= 32 {
        return len_17_to_32(s);
    }
    if len <= 64 {
        return len_33_to_64(s);
    }
    let seed: u64 = 81;
    let mut x = seed;
    let mut y = seed.wrapping_mul(K1).wrapping_add(113);
    let mut z = shift_mix(y.wrapping_mul(K2).wrapping_add(113)).wrapping_mul(K2);
    let mut v = (0u64, 0u64);
    let mut w = (0u64, 0u64);
    x = x.wrapping_mul(K2).wrapping_add(fetch64(s, 0));
    let end = ((len - 1) / 64) * 64;
    let last64 = end + ((len - 1) & 63) - 63;
    let mut p = 0;
    while p != end {
        x = rot(
            x.wrapping_add(y)
                .wrapping_add(v.0)
                .wrapping_add(fetch64(s, p + 8)),
            37,
        )
        .wrapping_mul(K1);
        y = rot(y.wrapping_add(v.1).wrapping_add(fetch64(s, p + 48)), 42).wrapping_mul(K1);
        x ^= w.1;
        y = y.wrapping_add(v.0).wrapping_add(fetch64(s, p + 40));
        z = rot(z.wrapping_add(w.0), 33).wrapping_mul(K1);
        v = weak_hash_32(s, p, v.1.wrapping_mul(K1), x.wrapping_add(w.0));
        w = weak_hash_32(
            s,
            p + 32,
            z.wrapping_add(w.1),
            y.wrapping_add(fetch64(s, p + 16)),
        );
        std::mem::swap(&mut z, &mut x);
        p += 64;
    }
    let mul = K1.wrapping_add((z & 0xff) << 1);
    let p = last64;
    w.0 = w.0.wrapping_add(((len - 1) & 63) as u64);
    v.0 = v.0.wrapping_add(w.0);
    w.0 = w.0.wrapping_add(v.0);
    x = rot(
        x.wrapping_add(y)
            .wrapping_add(v.0)
            .wrapping_add(fetch64(s, p + 8)),
        37,
    )
    .wrapping_mul(mul);
    y = rot(y.wrapping_add(v.1).wrapping_add(fetch64(s, p + 48)), 42).wrapping_mul(mul);
    x ^= w.1.wrapping_mul(9);
    y = y
        .wrapping_add(v.0.wrapping_mul(9))
        .wrapping_add(fetch64(s, p + 40));
    z = rot(z.wrapping_add(w.0), 33).wrapping_mul(mul);
    v = weak_hash_32(s, p, v.1.wrapping_mul(mul), x.wrapping_add(w.0));
    w = weak_hash_32(
        s,
        p + 32,
        z.wrapping_add(w.1),
        y.wrapping_add(fetch64(s, p + 16)),
    );
    std::mem::swap(&mut z, &mut x);
    hash_len16(
        hash_len16(v.0, w.0, mul)
            .wrapping_add(shift_mix(y).wrapping_mul(K0))
            .wrapping_add(z),
        hash_len16(v.1, w.1, mul).wrapping_add(x),
        mul,
    )
}

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
