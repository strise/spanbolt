//! Generated schema object names, ported from the emulator's `GlobalSchemaNames`: unnamed foreign
//! keys and CHECK constraints, and the managed indexes behind foreign keys.

/// Longest schema identifier; generated names are truncated to fit.
pub const MAX_IDENTIFIER: usize = 128;

/// Production's rule for a schema object name (the emulator's `ValidateSchemaName`): the part
/// after any `schema.` prefix starts with an ASCII letter, has only ASCII letters, digits and
/// underscores, and is at most 128 characters.
pub fn valid_name(name: &str) -> bool {
    let part = name.rsplit_once('.').map_or(name, |(_, n)| n);
    part.len() <= MAX_IDENTIFIER
        && part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Production's rule for a column name: at most 128 characters, not starting with `_`.
pub fn valid_column_name(name: &str) -> bool {
    name.len() <= MAX_IDENTIFIER && !name.starts_with('_')
}

/// Schema names reserved by Spanner (the emulator's `ReservedSchemaNames`), compared ignoring case;
/// `spanner_` and `pg_` prefixes are reserved too.
pub fn reserved_schema_name(name: &str) -> bool {
    const RESERVED: [&str; 17] = [
        "INFORMATION_SCHEMA",
        "safe",
        "aead",
        "kll_quantiles",
        "keys",
        "hll_count",
        "net",
        "ml",
        "SPANNER_PLACEMENT",
        "SPANNER_SYS",
        "pg_information_schema",
        "pg_catalog",
        "pg",
        "public",
        "DEFINITION_SCHEMA",
        "default",
        "spanner",
    ];
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name))
        || name.starts_with("spanner_")
        || name.starts_with("pg_")
}

/// The emulator's `MakeFingerprint`: Fingerprint64 of a seeded signature, as 16 hex digits.
pub fn make_fingerprint(signature: &str) -> String {
    let text = format!("2695C62E33338E4B_{signature}");
    format!("{:016X}", fingerprint64(text.as_bytes()))
}

/// The emulator's `MakeName`: `base_suffix`, with `base` cut so the whole fits in 128 bytes.
pub fn make_name(base: &str, suffix: &str) -> String {
    let len = base.len() + 1 + suffix.len();
    let mut base = base;
    if len > MAX_IDENTIFIER {
        base = base
            .get(..base.len().saturating_sub(len - MAX_IDENTIFIER))
            .unwrap_or("");
        base = base.strip_suffix('_').unwrap_or(base);
    }
    format!("{base}_{suffix}")
}

/// The emulator's `MakeBaseName`: `prefix_name1_name2...`, with the in-schema part of each name and
/// the first name's schema (if any) in front: `sch.FK_T_R`.
pub fn make_base_name(prefix: &str, names: &[&str]) -> String {
    let mut out = match names.first().and_then(|n| n.rsplit_once('.')) {
        Some((sch, _)) => format!("{sch}.{prefix}"),
        None => prefix.to_string(),
    };
    for n in names {
        out.push('_');
        out.push_str(n.rsplit_once('.').map_or(n, |(_, n)| n));
    }
    out
}

/// The emulator's `GenerateSequencedName`: `base_<fingerprint>_<n>` for the first `n` from 1 whose
/// name `taken` says is free.
pub fn sequenced_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let fp = make_fingerprint(base);
    (1u64..)
        .map(|n| make_name(base, &format!("{fp}_{n}")))
        .find(|name| !taken(name))
        .expect("an unused sequence number")
}

/// Name of an unnamed foreign key from `table` to `ref_table` (canonical table names).
pub fn foreign_key_name(table: &str, ref_table: &str, taken: impl Fn(&str) -> bool) -> String {
    sequenced_name(&make_base_name("FK", &[table, ref_table]), taken)
}

/// Name of an unnamed CHECK constraint on `table` (canonical table name).
pub fn check_name(table: &str, taken: impl Fn(&str) -> bool) -> String {
    sequenced_name(&make_base_name("CK", &[table]), taken)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    // Expected names recorded from the official emulator (conformance/cases/ddl/generated_names.case).
    #[test]
    fn names_match_the_emulator() {
        let free = |_: &str| false;
        assert_eq!(
            foreign_key_name("Others", "Parents", free),
            "FK_Others_Parents_77D2EFE8327295FC_1"
        );
        assert_eq!(
            foreign_key_name("sch.Items", "Parents", free),
            "sch.FK_Items_Parents_7EC0040F058CA24C_1"
        );
        assert_eq!(check_name("Others", free), "CK_Others_B1162681556A2BED_1");
        assert_eq!(
            check_name("sch.Items", free),
            "sch.CK_Items_D34EFD3F812532C3_1"
        );
    }

    #[test]
    fn sequence_counts_up_to_the_first_free_name() {
        let taken = |n: &str| n.ends_with("_1") || n.ends_with("_2");
        assert_eq!(check_name("Others", taken), "CK_Others_B1162681556A2BED_3");
    }

    #[test]
    fn long_names_are_cut_to_128_bytes() {
        let t = "A_Table_Name_That_Is_Long_Enough_That_The_Generated_Constraint_Names_Must_Be_Cut_To_Fit_The_128_Byte_Identifier_Limit";
        let name = check_name(t, |_| false);
        assert_eq!(name.len(), MAX_IDENTIFIER);
        assert_eq!(
            name,
            "CK_A_Table_Name_That_Is_Long_Enough_That_The_Generated_Constraint_Names_Must_Be_Cut_To_Fit_The_128_Byte_Ident_F8F5568198A9F57B_1"
        );
    }
}
