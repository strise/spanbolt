//! NET.* functions, ported from GoogleSQL's `functions/net.cc` and `base/net`: glibc `inet_pton`
//! parsing, GoogleSQL's IPv6 formatting, host extraction and Public Suffix List lookups against
//! the snapshot the emulator embeds (`public_suffix_list.dat`).
//!
//! Hosts that need IDNA normalisation (upper case or non-ASCII) go through ICU's UTS #46 ToASCII
//! in GoogleSQL. Only its ASCII behaviour is reproduced here; a host with non-ASCII characters, or
//! an `xn--` label whose decoded form can't be validated without the UTS #46 tables, is reported
//! as UNIMPLEMENTED rather than answered differently.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use spanbolt_storage::{Result, Value};

use crate::func::{ScalarFn, Sig, f, strict};
use crate::types::unimplemented;
use crate::value_ops::out_of_range;

pub fn lookup(sig: &Sig) -> Option<ScalarFn> {
    Some(match sig.name {
        "net.ip_from_string" => strict(|a| {
            let s = text(&a[0]);
            match ip_from_string(s) {
                Some(b) => Ok(bytes(b)),
                None => Err(out_of_range(format!(
                    "NET.IP_FROM_STRING() encountered an unparseable IP-address: {s}"
                ))),
            }
        }),
        "net.safe_ip_from_string" => {
            strict(|a| Ok(ip_from_string(text(&a[0])).map_or(Value::Null, bytes)))
        }
        "net.ip_to_string" => strict(|a| {
            let b = blob(&a[0]);
            ip_to_string(b).map(|s| Value::String(Arc::from(s))).ok_or_else(|| {
                out_of_range(format!(
                    "NET.IP_TO_STRING() encountered an invalid IP address. Expected 4 (for IPv4) or 16 (for IPv6) bytes but got {}",
                    b.len()
                ))
            })
        }),
        "net.ip_net_mask" => strict(|a| {
            let (n, bits) = (int(&a[0]), int(&a[1]));
            if n != 4 && n != 16 {
                return Err(out_of_range(format!(
                    "The first argument of NET.IP_NET_MASK() must be either 4 (for IPv4) or 16 (for IPv6); got {n}"
                )));
            }
            if bits < 0 || bits > n * 8 {
                return Err(out_of_range(format!(
                    "The second argument of NET.IP_NET_MASK() must be between 0 and the first argument * 8; got {bits}"
                )));
            }
            let mut out = vec![0u8; n as usize];
            mask_prefix(&mut out, bits, |_| 0xff);
            Ok(bytes(out))
        }),
        "net.ip_trunc" => strict(|a| {
            let (b, bits) = (blob(&a[0]), int(&a[1]));
            if b.len() != 4 && b.len() != 16 {
                return Err(out_of_range(format!(
                    "The first argument of NET.IP_TRUNC() must have length 4 (for IPv4) or 16 (for IPv6); got {}",
                    b.len()
                )));
            }
            if bits < 0 || bits > b.len() as i64 * 8 {
                return Err(out_of_range(format!(
                    "The second argument of NET.IP_TRUNC() must be between 0 and 8 * LENGTH(first argument); got {bits}"
                )));
            }
            let mut out = vec![0u8; b.len()];
            mask_prefix(&mut out, bits, |i| b[i]);
            Ok(bytes(out))
        }),
        "net.ipv4_from_int64" => strict(|a| {
            let v = int(&a[0]);
            if !(-0x8000_0000..=0xFFFF_FFFF).contains(&v) {
                return Err(out_of_range(format!(
                    "NET.IPV4_FROM_INT64() encountered an invalid integer IP. Expected range: [-0x80000000, 0xFFFFFFFF]; got {v}"
                )));
            }
            Ok(bytes((v as u32).to_be_bytes().to_vec()))
        }),
        "net.ipv4_to_int64" => strict(|a| {
            let b = blob(&a[0]);
            let quad: [u8; 4] = b.try_into().map_err(|_| {
                out_of_range(format!(
                    "NET.IPV4_TO_INT64() encountered a non-IPv4 address. Expected 4 bytes but got {}",
                    b.len()
                ))
            })?;
            Ok(Value::Int64(u32::from_be_bytes(quad) as i64))
        }),
        "net.host" => strict(|a| Ok(opt_string(host(text(&a[0]))))),
        "net.public_suffix" => f(|_, a| domain_fn(a, Psl::public_suffix)),
        "net.reg_domain" => f(|_, a| domain_fn(a, Psl::top_private_domain)),
        _ => return None,
    })
}

fn text(v: &Value) -> &str {
    match v {
        Value::String(s) => s,
        _ => "",
    }
}

fn blob(v: &Value) -> &[u8] {
    match v {
        Value::Bytes(b) => b,
        _ => &[],
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        _ => 0,
    }
}

fn bytes(b: Vec<u8>) -> Value {
    Value::Bytes(Arc::from(b))
}

fn opt_string(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(Arc::from(s)))
}

/// Keeps the first `bits` bits of `src(i)` in `out` (zeroing the rest).
fn mask_prefix(out: &mut [u8], bits: i64, src: impl Fn(usize) -> u8) {
    let full = (bits / 8) as usize;
    for (i, o) in out.iter_mut().enumerate().take(full) {
        *o = src(i);
    }
    let rem = bits % 8;
    if rem > 0 {
        out[full] = src(full) & (0xff00u16 >> rem) as u8;
    }
}

// ---------------------------------------------------------------- IP parsing and formatting

/// `inet_pton(AF_INET)` then `inet_pton(AF_INET6)`, as GoogleSQL calls them.
fn ip_from_string(s: &str) -> Option<Vec<u8>> {
    // INET6_ADDRSTRLEN is 46, and the C string must not contain NUL.
    if s.len() >= 46 || s.contains('\0') {
        return None;
    }
    let s = s.as_bytes();
    if let Some(v4) = inet_pton4(s) {
        return Some(v4.to_vec());
    }
    inet_pton6(s).map(|v| v.to_vec())
}

/// glibc `inet_pton4`: exactly four decimal octets, no leading zeros.
fn inet_pton4(s: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut octets = 0usize;
    let mut saw_digit = false;
    let mut cur: u32 = 0;
    for &c in s {
        if c.is_ascii_digit() {
            if saw_digit && cur == 0 {
                return None;
            }
            cur = cur * 10 + (c - b'0') as u32;
            if cur > 255 {
                return None;
            }
            if !saw_digit {
                octets += 1;
                if octets > 4 {
                    return None;
                }
                saw_digit = true;
            }
            out[octets - 1] = cur as u8;
        } else if c == b'.' && saw_digit {
            if octets == 4 {
                return None;
            }
            cur = 0;
            saw_digit = false;
        } else {
            return None;
        }
    }
    (octets == 4).then_some(out)
}

/// glibc `inet_pton6`.
fn inet_pton6(s: &[u8]) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut tp = 0usize;
    let mut colon: Option<usize> = None;
    let mut i = 0usize;
    if s.first() == Some(&b':') {
        if s.get(1) != Some(&b':') {
            return None;
        }
        i = 1;
    }
    let mut cur_tok = i;
    let mut saw_xdigit = false;
    let mut val: u32 = 0;
    let mut ndigits = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if let Some(d) = (c as char).to_digit(16) {
            if ndigits == 4 {
                return None;
            }
            val = (val << 4) | d;
            ndigits += 1;
            saw_xdigit = true;
            continue;
        }
        if c == b':' {
            cur_tok = i;
            if !saw_xdigit {
                if colon.is_some() {
                    return None;
                }
                colon = Some(tp);
                continue;
            } else if i >= s.len() {
                return None;
            }
            if tp + 2 > 16 {
                return None;
            }
            out[tp] = (val >> 8) as u8;
            out[tp + 1] = val as u8;
            tp += 2;
            saw_xdigit = false;
            ndigits = 0;
            val = 0;
            continue;
        }
        if c == b'.' && tp + 4 <= 16 {
            let v4 = inet_pton4(&s[cur_tok..])?;
            out[tp..tp + 4].copy_from_slice(&v4);
            tp += 4;
            saw_xdigit = false;
            break;
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > 16 {
            return None;
        }
        out[tp] = (val >> 8) as u8;
        out[tp + 1] = val as u8;
        tp += 2;
    }
    if let Some(cp) = colon {
        if tp == 16 {
            return None;
        }
        let n = tp - cp;
        out.copy_within(cp..tp, 16 - n);
        out[cp..16 - n].fill(0);
        tp = 16;
    }
    (tp == 16).then_some(out)
}

fn ip_to_string(b: &[u8]) -> Option<String> {
    match b.len() {
        4 => Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])),
        16 => {
            let w: Vec<u16> = b
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(ipv6_to_string(&w, &b[12..16]))
        }
        _ => None,
    }
}

/// GoogleSQL's `AppendIPv6ToString` (ipaddress_oss.cc).
fn ipv6_to_string(w: &[u16], v4: &[u8]) -> String {
    let mut out = String::new();
    if w[..4].iter().all(|&x| x == 0) {
        out.push(':');
        if w[4] == 0 && (w[5] == 0xffff || (w[5] == 0 && w[6] != 0)) {
            if w[5] != 0 {
                out.push_str(":ffff");
            }
            out.push(':');
            out.push_str(&format!("{}.{}.{}.{}", v4[0], v4[1], v4[2], v4[3]));
            return out;
        }
        let mut i = 4;
        while i < 8 && w[i] == 0 {
            i += 1;
        }
        if i < 8 {
            for x in &w[i..] {
                out.push_str(&format!(":{x:x}"));
            }
        } else {
            out.push(':');
        }
        return out;
    }
    // Longest run (>= 2) of zero words, first one on ties.
    let (mut cnt, mut best_len, mut start) = (0, 1, usize::MAX);
    for (i, &x) in w.iter().enumerate() {
        if x == 0 {
            cnt += 1;
            if cnt > best_len {
                best_len = cnt;
                start = i + 1 - cnt;
            }
        } else {
            cnt = 0;
        }
    }
    let mut i = 0;
    while i < 8 {
        if i == start {
            i += 2;
            while i < 8 && w[i] == 0 {
                i += 1;
            }
            out.push(':');
            if i == 8 {
                out.push(':');
                break;
            }
        }
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{:x}", w[i]));
        i += 1;
    }
    out
}

// ---------------------------------------------------------------- hosts

fn is_slash(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

fn is_netloc_end(c: u8) -> bool {
    matches!(c, b'#' | b'/' | b'?' | b'\\')
}

fn needs_normalize(c: u8) -> bool {
    c.is_ascii_uppercase() || c >= 0x7f
}

/// GoogleSQL's `net::Host`: the host of a URL-ish string, `None` for NULL.
fn host(url: &str) -> Option<&str> {
    if url.is_empty() {
        return None;
    }
    let url = url.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    let s = url.as_bytes();
    let mut begin = 0usize;
    let mut end = s.len();
    let mut colon_target = b':';
    if s.len() >= 2 && is_slash(s[0]) && is_slash(s[1]) {
        begin = 2;
        colon_target = b'/';
    }
    let mut p = begin;
    while p < end {
        let c = s[p];
        if is_netloc_end(c) {
            end = p;
            break;
        }
        if c == colon_target {
            if p + 2 < end && is_slash(s[p + 1]) && is_slash(s[p + 2]) {
                p += 2;
                begin = p + 1;
            }
            colon_target = b'/';
        } else if c == b'@' {
            begin = p + 1;
        }
        p += 1;
    }
    // Truncate at the first ':' after the brackets enclosing IPv6.
    let mut p = begin;
    if p < end && s[p] == b'[' {
        p += 1;
        while p < end {
            if s[p] == b'[' || s[p] == b']' {
                p += 1;
                break;
            }
            p += 1;
        }
    }
    if let Some(off) = s[p.min(end)..end].iter().position(|&c| c == b':') {
        end = p + off;
    }
    while begin < end && s[begin] == b'.' {
        begin += 1;
    }
    if end > begin && s[end - 1] == b'.' {
        let mut q = end - 1;
        while s[q - 1] == b'.' {
            q -= 1;
        }
        end = q + 1;
    }
    let h = &url[begin..end];
    (!h.is_empty()).then_some(h)
}

/// NET.PUBLIC_SUFFIX / NET.REG_DOMAIN (`DomainSuffix` in net.cc).
fn domain_fn(a: &[Value], which: fn(&Psl, &str) -> usize) -> Result<Value> {
    let Value::String(url) = &a[0] else {
        return Ok(Value::Null);
    };
    let Some(h) = host(url) else {
        return Ok(Value::Null);
    };
    let normalized: String;
    let name = if h.bytes().any(needs_normalize) {
        match to_ascii(h)? {
            Some(n) => {
                normalized = n;
                normalized.as_str()
            }
            None => return Ok(Value::Null),
        }
    } else {
        h
    };
    let start = which(psl(), name);
    let suffix = &name[start..];
    if suffix.is_empty() {
        return Ok(Value::Null);
    }
    // Keep as many labels of the original host as the suffix has.
    let mut dots = suffix.bytes().filter(|&c| c == b'.').count() as i64;
    let hb = h.as_bytes();
    let mut p = hb.len() as i64;
    loop {
        p -= 1;
        if p < 0 {
            break;
        }
        if hb[p as usize] == b'.' {
            dots -= 1;
            if dots < 0 {
                break;
            }
        }
    }
    let out = &h[(p + 1) as usize..];
    Ok(opt_string((!out.is_empty()).then_some(out)))
}

/// ICU UTS #46 `nameToASCII` (transitional, CheckBidi, no STD3 rules) for the hosts it can
/// answer without the Unicode tables: `Ok(None)` is an IDNA error (the SQL result is NULL).
fn to_ascii(host: &str) -> Result<Option<String>> {
    if !host.is_ascii() {
        return Err(unimplemented(
            "NET.PUBLIC_SUFFIX and NET.REG_DOMAIN on a host with non-ASCII characters",
        ));
    }
    let lower = host.to_ascii_lowercase();
    let labels: Vec<&str> = lower.split('.').collect();
    let trailing_dot = labels.len() > 1 && labels.last() == Some(&"");
    let n = if trailing_dot {
        labels.len() - 1
    } else {
        labels.len()
    };
    let max = if trailing_dot { 254 } else { 253 };
    if lower.len() > max {
        return Ok(None);
    }
    for label in &labels[..n] {
        if label.is_empty() || label.len() > 63 {
            return Ok(None);
        }
        if let Some(code) = label.strip_prefix("xn--") {
            let Some(decoded) = punycode_decode(code) else {
                return Ok(None);
            };
            if decoded.is_ascii() || decoded.chars().any(char::is_control) {
                return Ok(None);
            }
            if !valid_unicode_label(&decoded) {
                return Err(unimplemented(
                    "NET.PUBLIC_SUFFIX and NET.REG_DOMAIN on this xn-- label",
                ));
            }
            continue;
        }
        if label.starts_with('-') || label.ends_with('-') || label.get(2..4) == Some("--") {
            return Ok(None);
        }
    }
    Ok(Some(lower))
}

/// A conservative check that ICU would accept a decoded `xn--` label: lower-case letters, digits
/// and combining marks (not first), in NFC, with the hyphen rules.
fn valid_unicode_label(s: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;
    let mut chars = s.chars();
    if chars.next().is_some_and(is_combining_mark) {
        return false;
    }
    let hyphen_3_4 = s.chars().nth(2) == Some('-') && s.chars().nth(3) == Some('-');
    if s.starts_with('-') || s.ends_with('-') || hyphen_3_4 {
        return false;
    }
    s.nfc().eq(s.chars())
        && s.chars().all(|c| {
            (c.is_alphanumeric() || is_combining_mark(c) || c == '-')
                && c.to_lowercase().eq(std::iter::once(c))
        })
}

/// RFC 3492 punycode decoding of the part after `xn--`.
fn punycode_decode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    let (basic, rest) = match input.rfind('-') {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => ("", input),
    };
    let mut out: Vec<char> = basic.chars().collect();
    if !basic.is_ascii() {
        return None;
    }
    let adapt = |mut delta: u32, num: u32, first: bool| -> u32 {
        delta = if first { delta / 700 } else { delta / 2 };
        delta += delta / num;
        let mut k = 0;
        while delta > ((BASE - TMIN) * TMAX) / 2 {
            delta /= BASE - TMIN;
            k += BASE;
        }
        k + (BASE - TMIN + 1) * delta / (delta + 38)
    };
    let (mut n, mut i, mut bias) = (128u32, 0u32, 72u32);
    let mut it = rest.bytes().peekable();
    while it.peek().is_some() {
        let old_i = i;
        let mut w = 1u32;
        let mut k = BASE;
        loop {
            let c = it.next()?;
            let digit = match c {
                b'a'..=b'z' => c - b'a',
                b'A'..=b'Z' => c - b'A',
                b'0'..=b'9' => c - b'0' + 26,
                _ => return None,
            } as u32;
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let len = out.len() as u32 + 1;
        bias = adapt(i - old_i, len, old_i == 0);
        n = n.checked_add(i / len)?;
        i %= len;
        out.insert(i as usize, char::from_u32(n)?);
        i += 1;
    }
    Some(out.into_iter().collect())
}

// ---------------------------------------------------------------- public suffix list

const KNORMAL: u8 = 1;
const KWILDCARD: u8 = 2;
const KEXCEPTION: u8 = 4;
const KPRIVATE: u8 = 8;

#[derive(Default)]
struct Node {
    map: HashMap<String, usize>,
    rules: u8,
}

/// GoogleSQL's `PublicSuffixRules` trie, keyed by labels from the right.
struct Psl {
    nodes: Vec<Node>,
}

fn psl() -> &'static Psl {
    static PSL: OnceLock<Psl> = OnceLock::new();
    PSL.get_or_init(|| Psl::build(include_str!("public_suffix_list.dat")))
}

/// One label of the name with whether a rule matched it; `start` is its byte offset.
struct Match {
    matched: bool,
    start: usize,
}

impl Psl {
    fn build(data: &str) -> Psl {
        let mut icann: Vec<&str> = Vec::new();
        let mut private: Vec<&str> = Vec::new();
        let mut in_private = false;
        for line in data.lines() {
            if line.contains("BEGIN PRIVATE DOMAINS") {
                in_private = true;
            } else if line.contains("END PRIVATE DOMAINS") {
                in_private = false;
            }
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if in_private {
                private.push(line);
            } else {
                icann.push(line);
            }
        }
        // The builder dedups rules per section (flat_hash_set) before building the trie.
        icann.sort_unstable();
        icann.dedup();
        private.sort_unstable();
        private.dedup();
        let mut psl = Psl {
            nodes: vec![Node {
                map: HashMap::new(),
                rules: KNORMAL | KWILDCARD,
            }],
        };
        for r in icann {
            psl.add(r, false);
        }
        for r in private {
            psl.add(r, true);
        }
        psl
    }

    fn add(&mut self, rule: &str, private: bool) {
        let mut labels: Vec<&str> = rule.rsplit('.').collect();
        let mut flags = KNORMAL;
        if labels.last() == Some(&"*") {
            flags = KWILDCARD;
            labels.pop();
        } else if let Some(l) = labels.last_mut()
            && let Some(rest) = l.strip_prefix('!')
        {
            flags = KEXCEPTION;
            *l = rest;
        }
        if private {
            flags |= KPRIVATE;
        }
        let mut node = 0usize;
        for l in labels {
            node = match self.nodes[node].map.get(l) {
                Some(&n) => n,
                None => {
                    let n = self.nodes.len();
                    self.nodes.push(Node::default());
                    self.nodes[node].map.insert(l.to_string(), n);
                    n
                }
            };
        }
        let stored = &mut self.nodes[node].rules;
        if *stored == 0 {
            *stored = flags;
            return;
        }
        if (*stored & KEXCEPTION) != (flags & KEXCEPTION) {
            *stored &= !KEXCEPTION;
        }
        if (*stored & KPRIVATE) != (flags & KPRIVATE) {
            *stored &= !KPRIVATE;
        }
        *stored |= flags & (KNORMAL | KWILDCARD);
    }

    /// `GetMatchingRules`, labels from the right; the implicit root match comes first.
    fn matches(&self, name: &str) -> Vec<Match> {
        let mut out = vec![Match {
            matched: true,
            start: name.len(),
        }];
        let name = name.strip_suffix('.').unwrap_or(name);
        let b = name.as_bytes();
        // `dot` is the index of the dot left of the next label (-1 at the start of the name).
        let mut dot = name.len() as i64;
        let next = |dot: &mut i64| -> (usize, usize) {
            let end = *dot as usize;
            let mut cur = *dot - 1;
            while cur >= 0 && b[cur as usize] != b'.' {
                cur -= 1;
            }
            *dot = cur;
            ((cur + 1) as usize, end)
        };
        let mut node = 0usize;
        while dot >= 0 {
            let (s, e) = next(&mut dot);
            if s == e {
                return out;
            }
            let label = &name[s..e];
            let prev = node;
            let prev_rules = self.nodes[prev].rules;
            let Some(&found) = self.nodes[node].map.get(label) else {
                if prev_rules & KWILDCARD == 0 {
                    out.push(Match {
                        matched: false,
                        start: s,
                    });
                    return out;
                }
                if prev == 0 {
                    return out;
                }
                out.push(Match {
                    matched: true,
                    start: s,
                });
                if dot < 0 {
                    return out;
                }
                let (s, e) = next(&mut dot);
                if s != e {
                    out.push(Match {
                        matched: false,
                        start: s,
                    });
                }
                return out;
            };
            node = found;
            let rules = self.nodes[node].rules;
            if rules & KPRIVATE != 0 {
                out.push(Match {
                    matched: false,
                    start: s,
                });
                return out;
            }
            if rules & KEXCEPTION != 0 {
                if let Some(last) = out.last_mut() {
                    last.matched = prev_rules & KWILDCARD != 0;
                }
                out.push(Match {
                    matched: false,
                    start: s,
                });
                return out;
            }
            out.push(Match {
                matched: rules & KNORMAL != 0 || prev_rules & KWILDCARD != 0,
                start: s,
            });
        }
        out
    }

    /// Byte offset in `name` where the public suffix starts (`name.len()` for none).
    fn public_suffix(&self, name: &str) -> usize {
        if name.is_empty() {
            return 0;
        }
        let m = self.matches(name);
        m.iter()
            .rev()
            .find(|m| m.matched)
            .map_or(name.len(), |m| m.start)
    }

    /// Byte offset in `name` where the registered domain starts (`name.len()` for none).
    fn top_private_domain(&self, name: &str) -> usize {
        if name.is_empty() {
            return 0;
        }
        let m = self.matches(name);
        let mut last_no_match = None;
        for x in m.iter().rev() {
            if x.matched {
                break;
            }
            last_no_match = Some(x.start);
        }
        last_no_match.unwrap_or(name.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_formatting() {
        let s = |t: &str| ip_to_string(&ip_from_string(t).unwrap()).unwrap();
        assert_eq!(
            s("2001:0db8:0:0:0:ff00:0042:8329"),
            "2001:db8::ff00:42:8329"
        );
        assert_eq!(s("::"), "::");
        assert_eq!(s("::1"), "::1");
        assert_eq!(s("::ffff:1.2.3.4"), "::ffff:1.2.3.4");
        assert_eq!(s("1:0:0:1:0:0:0:1"), "1:0:0:1::1");
        assert_eq!(s("1:2:3:4:5:6:7:0"), "1:2:3:4:5:6:7:0");
    }

    #[test]
    fn pton() {
        assert!(ip_from_string("01.2.3.4").is_none());
        assert!(ip_from_string("1:2:3:4:5:6:7::").is_some());
        assert!(ip_from_string("1:2:3:4:5:6:7:8::").is_none());
        assert!(ip_from_string("::1.2.3.4").is_some());
        assert!(ip_from_string("1::2:").is_none());
    }

    #[test]
    fn suffixes() {
        fn ps(n: &str) -> &str {
            &n[psl().public_suffix(n)..]
        }
        fn rd(n: &str) -> &str {
            &n[psl().top_private_domain(n)..]
        }
        assert_eq!(ps("www.example.co.uk"), "co.uk");
        assert_eq!(rd("www.example.co.uk"), "example.co.uk");
        assert_eq!(ps("foo.ck"), "foo.ck");
        assert_eq!(rd("foo.ck"), "");
        assert_eq!(rd("www.ck"), "www.ck");
        assert_eq!(rd("foo.blogspot.com"), "blogspot.com");
        assert_eq!(ps("foo.unknowntld"), "");
    }

    #[test]
    fn punycode() {
        assert_eq!(punycode_decode("bcher-kva").as_deref(), Some("bücher"));
        assert_eq!(punycode_decode("90a3ac").as_deref(), Some("срб"));
        assert!(punycode_decode("zzzz").is_none());
    }
}
