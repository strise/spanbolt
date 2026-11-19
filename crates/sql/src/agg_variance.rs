//! STDDEV / STDDEV_SAMP / VARIANCE / VAR_SAMP, matching GoogleSQL's reference implementation bit
//! for bit.
//!
//! FLOAT64 input runs GoogleSQL's stable one-pass update (`UpdateMeanAndVariance` in
//! reference_impl/function.cc) in `long double`. The emulator ships as a Linux aarch64 binary,
//! where `long double` is IEEE binary128, so the arithmetic here is a small software binary128
//! (113-bit significand, round to nearest even) rather than f64.
//!
//! NUMERIC input follows `NumericValue::VarianceAggregator`: exact sums of the scaled values and
//! of their squares, then one correctly rounded division in f64.

use std::cmp::Ordering;

use spanbolt_storage::Value;

/// Which statistic to finish with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stat {
    StddevSamp,
    VarSamp,
}

pub fn finish<'v>(stat: Stat, vals: impl Iterator<Item = &'v Value>) -> Value {
    let vals: Vec<&Value> = vals.collect();
    if vals.iter().any(|v| matches!(v, Value::Numeric(_))) {
        return numeric(stat, &vals);
    }
    let mut count: u64 = 0;
    let mut mean = Quad::ZERO;
    let mut var = Quad::ZERO;
    let mut nan = false;
    for v in vals {
        let x = match v {
            Value::Float64(f) => *f,
            Value::Float32(f) => *f as f64,
            Value::Int64(i) => *i as f64,
            _ => continue,
        };
        count += 1;
        if nan {
            continue;
        }
        if !x.is_finite() {
            nan = true;
            continue;
        }
        // frac = 1 / count; delta = x - mean; mean += delta * frac;
        // variance += frac * ((1 - frac) * delta * delta - variance)
        let frac = Quad::ONE.div(Quad::from_u64(count));
        let delta = Quad::from_f64(x).sub(mean);
        mean = mean.add(delta.mul(frac));
        let t = Quad::ONE.sub(frac).mul(delta).mul(delta).sub(var);
        var = var.add(frac.mul(t));
    }
    if count <= 1 {
        return Value::Null;
    }
    if nan {
        return Value::Float64(f64::NAN);
    }
    let c = Quad::from_u64(count).div(Quad::from_u64(count - 1));
    let var = var.mul(c);
    Value::Float64(match stat {
        Stat::VarSamp => var.to_f64(),
        Stat::StddevSamp => var.sqrt().to_f64(),
    })
}

fn numeric(stat: Stat, vals: &[&Value]) -> Value {
    let mut sum = Wide::ZERO;
    let mut sum_sq = Wide::ZERO;
    let mut count: u64 = 0;
    for v in vals {
        if let Value::Numeric(n) = v {
            count += 1;
            let x = Wide::from_i128(n.0);
            sum = sum.add(&x);
            sum_sq = sum_sq.add(&x.mul(&x));
        }
    }
    if count <= 1 {
        return Value::Null;
    }
    // (sum_sq * count - sum^2) / (scale^2 * count * (count - 1))
    let num = sum_sq.mul(&Wide::from_u64(count)).sub(&sum.mul(&sum));
    let den = Wide::from_u64(1_000_000_000_000_000_000)
        .mul(&Wide::from_u64(count))
        .mul(&Wide::from_u64(count - 1));
    let var = num.to_f64() / den.to_f64();
    Value::Float64(match stat {
        Stat::VarSamp => var,
        Stat::StddevSamp => var.sqrt(),
    })
}

// ---------------------------------------------------------------- 512-bit integers

/// A two's complement 512-bit integer (little-endian limbs), wide enough for NUMERIC's sums.
#[derive(Clone, Copy)]
struct Wide([u64; 8]);

impl Wide {
    const ZERO: Wide = Wide([0; 8]);

    fn from_i128(v: i128) -> Wide {
        let fill = if v < 0 { u64::MAX } else { 0 };
        let mut w = [fill; 8];
        w[0] = v as u64;
        w[1] = (v >> 64) as u64;
        Wide(w)
    }

    fn from_u64(v: u64) -> Wide {
        let mut w = [0; 8];
        w[0] = v;
        Wide(w)
    }

    fn is_negative(&self) -> bool {
        self.0[7] >> 63 == 1
    }

    fn add(&self, o: &Wide) -> Wide {
        let mut out = [0u64; 8];
        let mut carry = false;
        for (i, slot) in out.iter_mut().enumerate() {
            let (s1, c1) = self.0[i].overflowing_add(o.0[i]);
            let (s2, c2) = s1.overflowing_add(carry as u64);
            *slot = s2;
            carry = c1 || c2;
        }
        Wide(out)
    }

    fn neg(&self) -> Wide {
        let mut inv = [0u64; 8];
        for (i, slot) in inv.iter_mut().enumerate() {
            *slot = !self.0[i];
        }
        Wide(inv).add(&Wide::from_u64(1))
    }

    fn sub(&self, o: &Wide) -> Wide {
        self.add(&o.neg())
    }

    fn abs(&self) -> Wide {
        if self.is_negative() {
            self.neg()
        } else {
            *self
        }
    }

    /// Product truncated to 512 bits (operands here never overflow it).
    fn mul(&self, o: &Wide) -> Wide {
        let neg = self.is_negative() != o.is_negative();
        let (a, b) = (self.abs(), o.abs());
        let mut out = [0u64; 8];
        for i in 0..8 {
            let mut carry: u128 = 0;
            for j in 0..8 - i {
                let cur = out[i + j] as u128 + a.0[i] as u128 * b.0[j] as u128 + carry;
                out[i + j] = cur as u64;
                carry = cur >> 64;
            }
        }
        let r = Wide(out);
        if neg { r.neg() } else { r }
    }

    /// Correctly rounded (half to even) conversion, as GoogleSQL's `FixedUint::operator double`.
    fn to_f64(self) -> f64 {
        let neg = self.is_negative();
        let m = self.abs();
        let r = limbs_to_f64(&m.0);
        if neg { -r } else { r }
    }
}

fn limbs_to_f64(limbs: &[u64]) -> f64 {
    let Some(top) = limbs.iter().rposition(|&w| w != 0) else {
        return 0.0;
    };
    let msb = 63 - limbs[top].leading_zeros() as usize;
    let bit_len = top * 64 + msb + 1;
    let bit = |i: usize| (limbs[i / 64] >> (i % 64)) & 1;
    if bit_len <= 53 {
        return limbs[0] as f64;
    }
    // Take the top 53 bits, then round on the rest.
    let shift = bit_len - 53;
    let mut mant: u64 = 0;
    for i in (shift..bit_len).rev() {
        mant = (mant << 1) | bit(i);
    }
    let half = bit(shift - 1) == 1;
    let below = (0..shift - 1).any(|i| bit(i) == 1);
    if half && (below || mant & 1 == 1) {
        mant += 1;
    }
    // mant * 2^shift; mant <= 2^53 so the product is exact in f64 unless it overflows.
    mant as f64 * 2f64.powi(shift as i32)
}

// ---------------------------------------------------------------- software binary128

/// A 256-bit unsigned integer as (high, low).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct U256 {
    hi: u128,
    lo: u128,
}

impl U256 {
    const ZERO: U256 = U256 { hi: 0, lo: 0 };

    fn from(v: u128) -> U256 {
        U256 { hi: 0, lo: v }
    }

    fn is_zero(&self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    fn bits(&self) -> u32 {
        if self.hi != 0 {
            256 - self.hi.leading_zeros()
        } else {
            128 - self.lo.leading_zeros()
        }
    }

    fn shl(&self, n: u32) -> U256 {
        match n {
            0 => *self,
            1..=127 => U256 {
                hi: (self.hi << n) | (self.lo >> (128 - n)),
                lo: self.lo << n,
            },
            128..=255 => U256 {
                hi: self.lo << (n - 128),
                lo: 0,
            },
            _ => U256::ZERO,
        }
    }

    fn shr(&self, n: u32) -> U256 {
        match n {
            0 => *self,
            1..=127 => U256 {
                hi: self.hi >> n,
                lo: (self.lo >> n) | (self.hi << (128 - n)),
            },
            128..=255 => U256 {
                hi: 0,
                lo: self.hi >> (n - 128),
            },
            _ => U256::ZERO,
        }
    }

    /// Whether any of the low `n` bits is set.
    fn low_bits_set(&self, n: u32) -> bool {
        if n == 0 {
            return false;
        }
        if n >= 256 {
            return !self.is_zero();
        }
        !self.sub(&self.shr(n).shl(n)).is_zero()
    }

    fn add(&self, o: &U256) -> U256 {
        let (lo, c) = self.lo.overflowing_add(o.lo);
        U256 {
            hi: self.hi.wrapping_add(o.hi).wrapping_add(c as u128),
            lo,
        }
    }

    fn sub(&self, o: &U256) -> U256 {
        let (lo, b) = self.lo.overflowing_sub(o.lo);
        U256 {
            hi: self.hi.wrapping_sub(o.hi).wrapping_sub(b as u128),
            lo,
        }
    }

    fn mul(a: u128, b: u128) -> U256 {
        let m = u64::MAX as u128;
        let (a1, a0, b1, b0) = (a >> 64, a & m, b >> 64, b & m);
        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;
        let mid = (p00 >> 64) + (p01 & m) + (p10 & m);
        U256 {
            lo: (p00 & m) | (mid << 64),
            hi: p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64),
        }
    }

    fn cmp(&self, o: &U256) -> Ordering {
        (self.hi, self.lo).cmp(&(o.hi, o.lo))
    }

    /// Quotient and whether the remainder is non-zero.
    fn div(&self, d: u128) -> (U256, bool) {
        let d = U256::from(d);
        let mut q = U256::ZERO;
        let mut r = U256::ZERO;
        for i in (0..self.bits()).rev() {
            r = r.shl(1);
            if self.shr(i).lo & 1 == 1 {
                r.lo |= 1;
            }
            if r.cmp(&d) != Ordering::Less {
                r = r.sub(&d);
                q = q.add(&U256::from(1).shl(i));
            }
        }
        (q, !r.is_zero())
    }

    /// Integer square root and whether the remainder is non-zero.
    fn isqrt(&self) -> (U256, bool) {
        let mut rem = *self;
        let mut res = U256::ZERO;
        let mut bit = U256::from(1).shl((self.bits().max(1) - 1) & !1);
        while !bit.is_zero() {
            let t = res.add(&bit);
            if rem.cmp(&t) != Ordering::Less {
                rem = rem.sub(&t);
                res = res.shr(1).add(&bit);
            } else {
                res = res.shr(1);
            }
            bit = bit.shr(2);
        }
        (res, !rem.is_zero())
    }
}

/// A finite binary128 value: (-1)^neg * mant * 2^exp, `mant` in [2^112, 2^113) or zero. The
/// exponent range is unbounded; the values here never approach binary128's limits.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Quad {
    neg: bool,
    mant: u128,
    exp: i32,
}

const P: u32 = 113;

impl Quad {
    const ZERO: Quad = Quad {
        neg: false,
        mant: 0,
        exp: 0,
    };
    const ONE: Quad = Quad {
        neg: false,
        mant: 1 << (P - 1),
        exp: -(P as i32 - 1),
    };

    fn is_zero(&self) -> bool {
        self.mant == 0
    }

    /// Rounds `mag * 2^exp` (exact) to 113 bits, half to even.
    fn round(neg: bool, mag: U256, exp: i32) -> Quad {
        if mag.is_zero() {
            return Quad {
                neg,
                mant: 0,
                exp: 0,
            };
        }
        let bits = mag.bits();
        if bits <= P {
            return Quad {
                neg,
                mant: mag.shl(P - bits).lo,
                exp: exp - (P - bits) as i32,
            };
        }
        let sh = bits - P;
        let mut m = mag.shr(sh).lo;
        let half = mag.shr(sh - 1).lo & 1 == 1;
        let below = mag.low_bits_set(sh - 1);
        let mut e = exp + sh as i32;
        if half && (below || m & 1 == 1) {
            m += 1;
            if m == 1 << P {
                m >>= 1;
                e += 1;
            }
        }
        Quad {
            neg,
            mant: m,
            exp: e,
        }
    }

    fn from_u64(v: u64) -> Quad {
        Quad::round(false, U256::from(v as u128), 0)
    }

    fn from_f64(x: f64) -> Quad {
        let bits = x.to_bits();
        let neg = bits >> 63 == 1;
        let be = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & ((1 << 52) - 1);
        let (m, e) = if be == 0 {
            (frac, -1074)
        } else {
            (frac | (1 << 52), be - 1075)
        };
        let mut q = Quad::round(neg, U256::from(m as u128), e);
        q.neg = neg;
        q
    }

    fn neg(self) -> Quad {
        Quad {
            neg: !self.neg,
            ..self
        }
    }

    fn add(self, o: Quad) -> Quad {
        if self.is_zero() && o.is_zero() {
            return Quad {
                neg: self.neg && o.neg,
                ..Quad::ZERO
            };
        }
        if self.is_zero() {
            return o;
        }
        if o.is_zero() {
            return self;
        }
        let (a, b) = if (self.exp, self.mant) >= (o.exp, o.mant) {
            (self, o)
        } else {
            (o, self)
        };
        // Both significands sit 128 bits up; the smaller one is shifted down by the exponent
        // gap with any lost bits jammed into its lowest bit.
        let base = a.exp - 128;
        let am = U256::from(a.mant).shl(128);
        let diff = (a.exp - b.exp) as u32;
        let bm_full = U256::from(b.mant).shl(128);
        let mut bm = bm_full.shr(diff);
        if bm_full.low_bits_set(diff.min(256)) {
            bm.lo |= 1;
        }
        if a.neg == b.neg {
            Quad::round(a.neg, am.add(&bm), base)
        } else {
            match am.cmp(&bm) {
                Ordering::Equal => Quad::ZERO,
                Ordering::Greater => Quad::round(a.neg, am.sub(&bm), base),
                Ordering::Less => Quad::round(b.neg, bm.sub(&am), base),
            }
        }
    }

    fn sub(self, o: Quad) -> Quad {
        self.add(o.neg())
    }

    fn mul(self, o: Quad) -> Quad {
        let neg = self.neg != o.neg;
        if self.is_zero() || o.is_zero() {
            return Quad { neg, ..Quad::ZERO };
        }
        Quad::round(neg, U256::mul(self.mant, o.mant), self.exp + o.exp)
    }

    fn div(self, o: Quad) -> Quad {
        let neg = self.neg != o.neg;
        if self.is_zero() {
            return Quad { neg, ..Quad::ZERO };
        }
        // (a << 116) / b has at least 116 significant bits; the remainder is jammed.
        let (mut q, rem) = U256::from(self.mant).shl(116).div(o.mant);
        if rem {
            q.lo |= 1;
        }
        Quad::round(neg, q, self.exp - o.exp - 116)
    }

    fn sqrt(self) -> Quad {
        if self.is_zero() {
            return self;
        }
        if self.neg {
            return Quad {
                neg: false,
                mant: 0,
                exp: i32::MIN,
            };
        }
        // Scale to an even exponent with ~232 bits so the root has ~116 bits.
        let mut k = 118u32;
        if (self.exp - k as i32) % 2 != 0 {
            k += 1;
        }
        let (mut r, rem) = U256::from(self.mant).shl(k).isqrt();
        if rem {
            r.lo |= 1;
        }
        Quad::round(false, r, (self.exp - k as i32) / 2)
    }

    fn to_f64(self) -> f64 {
        if self.exp == i32::MIN {
            return f64::NAN;
        }
        if self.is_zero() {
            return if self.neg { -0.0 } else { 0.0 };
        }
        let sign = if self.neg { -1.0 } else { 1.0 };
        let mag = U256::from(self.mant);
        // Unbiased exponent of the leading bit.
        let lead = self.exp + (P as i32 - 1);
        let bits: u64 = if lead >= -1022 {
            let sh = P - 53;
            let mut m = (self.mant >> sh) as u64;
            let half = (self.mant >> (sh - 1)) & 1 == 1;
            let below = mag.low_bits_set(sh - 1);
            let mut e = lead;
            if half && (below || m & 1 == 1) {
                m += 1;
                if m == 1 << 53 {
                    m >>= 1;
                    e += 1;
                }
            }
            if e > 1023 {
                return sign * f64::INFINITY;
            }
            (((e + 1023) as u64) << 52) | (m & ((1 << 52) - 1))
        } else {
            // Subnormal: count units of 2^-1074.
            let sh = (-1074 - self.exp) as u32;
            if sh > P + 1 {
                0
            } else {
                let mut m = mag.shr(sh).lo as u64;
                let half = sh > 0 && mag.shr(sh - 1).lo & 1 == 1;
                let below = sh > 1 && mag.low_bits_set(sh - 1);
                if half && (below || m & 1 == 1) {
                    m += 1;
                }
                m
            }
        };
        sign * f64::from_bits(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_round_trips() {
        for x in [1.0, 0.1, -2.5, 1e300, 1e-300, 5e-324, f64::MAX, 123456.789] {
            assert_eq!(Quad::from_f64(x).to_f64(), x);
        }
    }

    #[test]
    fn quad_arithmetic() {
        let q = Quad::from_f64;
        assert_eq!(q(0.1).add(q(0.2)).to_f64(), 0.30000000000000004);
        assert_eq!(q(1.0).div(q(3.0)).mul(q(3.0)).to_f64(), 1.0);
        assert_eq!(q(2.0).sqrt().to_f64(), 2f64.sqrt());
        assert_eq!(q(9.0).sqrt().to_f64(), 3.0);
        assert_eq!(q(1.5).sub(q(1.5)).to_f64(), 0.0);
        assert_eq!(q(1e308).mul(q(10.0)).to_f64(), f64::INFINITY);
    }

    #[test]
    fn wide_to_f64() {
        assert_eq!(Wide::from_i128(-5).to_f64(), -5.0);
        let big = Wide::from_i128(i128::MAX).mul(&Wide::from_i128(i128::MAX));
        assert_eq!(big.to_f64(), (i128::MAX as f64) * (i128::MAX as f64));
    }
}
