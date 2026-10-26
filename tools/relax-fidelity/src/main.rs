//! Float kernels over adversarial inputs, every result printed as its bits.
//!
//! Built twice, once by rustc (`cargo run --release`) and once by makepad's
//! relax JIT (`relax run --quiet relax-fidelity`), the two outputs must be
//! byte-equal for relax to compute what the shipped binary computes. Each line
//! is `kernel inputs => result`, all in hex, so a diff names the operation and
//! the operands. `README.md` has the commands and what was found.
//!
//! Three ways of reaching an operation are kept apart, because a JIT can get
//! one right and another wrong: through a function pointer on `black_box`ed
//! inputs (the runtime instruction), on literals the optimiser may fold, and
//! in `const` items (compile-time evaluation). The whole battery runs `REPS`
//! times and each repetition is compared with the first, so a function that
//! changes tier between calls and computes differently there is seen too.

use std::fmt::Write as _;
use std::hint::black_box as bb;
use std::io::Write as _;


struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// any f32 bit pattern
    fn any32(&mut self) -> f32 {
        f32::from_bits(self.next() as u32)
    }
    /// a finite f32 of moderate exponent, either sign: the ordinary arithmetic
    fn mid32(&mut self) -> f32 {
        let r = self.next();
        let exp = 127 - 20 + (r >> 32) % 41; // 2^-20 .. 2^20
        f32::from_bits(((r & 1) << 31 | exp << 23 | (r >> 8) & 0x7f_ffff) as u32)
    }
    fn any64(&mut self) -> f64 {
        f64::from_bits(self.next())
    }
    fn mid64(&mut self) -> f64 {
        let r = self.next();
        let r2 = self.next();
        let exp = 1023 - 40 + (r2 >> 32) % 81;
        f64::from_bits((r & 1) << 63 | exp << 52 | (r >> 12) & 0xf_ffff_ffff_ffff)
    }
}

fn specials32() -> Vec<f32> {
    let bits: &[u32] = &[
        0x0000_0000, 0x8000_0000, // +-0
        0x0000_0001, 0x8000_0001, 0x007f_ffff, 0x0040_0000, 0x0000_0003, // subnormals
        0x0080_0000, 0x8080_0000, 0x0080_0001, // smallest normals
        0x3f80_0000, 0xbf80_0000, 0x3f00_0000, 0x3fc0_0000, 0x4020_0000, 0xc020_0000, // 1 -1 .5 1.5 2.5 -2.5
        0x3dcc_cccd, 0x3eaa_aaab, 0x3f7f_ffff, 0x3f80_0001, // .1 1/3 1-ulp 1+ulp
        0x7f7f_ffff, 0xff7f_ffff, 0x7f80_0000, 0xff80_0000, // max, inf
        0x7fc0_0000, 0xffc0_0000, 0x7fc1_2345, 0xffd5_4321, // quiet NaNs with payloads
        0x7f80_0001, 0xff81_2345, 0x7fa0_0000, // signaling NaNs
        0x4b80_0000, 0x4b80_0001, 0x4b00_0000, 0x4aff_ffff, 0x4b7f_ffff, // 2^24, 2^24+2, 2^23, 2^23-.5, 2^24-1
        0x4f00_0000, 0x4eff_ffff, 0xcf00_0000, 0xcf00_0001, 0x4f80_0000, 0x4f7f_ffff, // 2^31, 2^32 edges
        0x5f00_0000, 0x5eff_ffff, 0xdf00_0000, 0x5f80_0000, // 2^63, 2^64
        0x437f_8000, 0x4380_0000, 0xc301_0000, 0x4300_0000, 0xc300_0000, // 255.5 256 -129 128 -128
        0x477f_ff00, 0x4780_0000, 0xc700_0000, // 65535 65536 -32768
        0x501502f9, 0x2edb_e6ff, 0x3f00_0001, 0x3eff_ffff, 0x3fbf_ffff, // 1e10 1e-10 .5+ulp .5-ulp 1.5-ulp
        0x40490fdb, 0xc0490fdb, 0x3fc90fdb, 0x4f800000, 0x7e967699, // pi -pi pi/2 2^32 1e38
        0x42c8_0000, 0x4120_0000, 0x3a83_126f, // 100 10 .001
    ];
    bits.iter().map(|&b| f32::from_bits(b)).collect()
}

fn specials64() -> Vec<f64> {
    let bits: &[u64] = &[
        0, 0x8000_0000_0000_0000, 1, 0x8000_0000_0000_0001, 0x000f_ffff_ffff_ffff, 0x0008_0000_0000_0000,
        0x0010_0000_0000_0000, 0x8010_0000_0000_0000,
        0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000, 0x3fe0_0000_0000_0000, 0x3ff8_0000_0000_0000,
        0x4004_0000_0000_0000, 0xc004_0000_0000_0000, 0x3fb9_9999_9999_999a, 0x3fd5_5555_5555_5555,
        0x3fef_ffff_ffff_ffff, 0x3ff0_0000_0000_0001,
        0x7fef_ffff_ffff_ffff, 0xffef_ffff_ffff_ffff, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0000, 0xfff8_0000_0000_0000, 0x7ff8_0000_1234_5678, 0xfffa_bcde_0000_0001,
        0x7ff0_0000_0000_0001, 0xfff1_2345_6789_abcd, 0x7ff4_0000_0000_0000,
        0x4340_0000_0000_0000, 0x4340_0000_0000_0001, 0x4330_0000_0000_0000, 0x432f_ffff_ffff_ffff,
        0x41e0_0000_0000_0000, 0x41df_ffff_ffff_ffff, 0xc1e0_0000_0000_0000, 0xc1e0_0000_0020_0000,
        0x41f0_0000_0000_0000, 0x41ef_ffff_ffff_ffff, 0x43e0_0000_0000_0000, 0x43df_ffff_ffff_ffff,
        0xc3e0_0000_0000_0000, 0x43f0_0000_0000_0000, 0x43ef_ffff_ffff_ffff,
        // f64 -> f32 rounding: f32::MAX + half ulp (overflow tie), a tie, subnormal f32 results
        0x47ef_ffff_f000_0000, 0x47ef_ffff_efff_ffff, 0x3ff0_0000_1000_0000, 0x3ff0_0000_3000_0000,
        0x36a0_0000_0000_0000, 0x3690_0000_0000_0000, 0x3690_0000_0000_0001, 0x380f_ffff_f000_0000,
        0x400921fb54442d18, 0x4202_a05f_2000_0000, 0x3ddb_7cdf_d9d7_bdbb,
    ];
    bits.iter().map(|&b| f64::from_bits(b)).collect()
}

fn ints() -> Vec<u64> {
    let mut v: Vec<u64> = vec![
        0, 1, 2, 3, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff,
        0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000, u64::MAX, u64::MAX - 1,
        (1 << 24) + 1, (1 << 24) + 3, (1 << 53) + 1, (1 << 53) + 3, 0x0020_0000_0000_0001,
    ];
    // ties and near-ties for every magnitude: 2^k + half an f32 ulp (+-1), and the
    // double-rounding traps (a u64 -> f32 through f64 rounds twice)
    for k in 24..64u32 {
        let base = 1u64 << k;
        let half = 1u64 << (k - 24);
        for d in [0i64, 1, -1] {
            v.push(base.wrapping_add(half).wrapping_add(d as u64));
            v.push(base.wrapping_add(3 * half).wrapping_add(d as u64));
        }
        if k >= 53 {
            v.push(base | half | 1);
            v.push(base | half | (1u64 << (k - 53)));
        }
    }
    let n = v.len();
    for i in 0..n {
        v.push(v[i].wrapping_neg());
    }
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..200 {
        v.push(r.next());
        v.push(r.next() >> (r.next() % 64));
    }
    v
}

type U32 = (&'static str, fn(f32) -> f32);
type B32 = (&'static str, fn(f32, f32) -> f32);
type T32 = (&'static str, fn(f32, f32, f32) -> f32);
type U64 = (&'static str, fn(f64) -> f64);
type B64 = (&'static str, fn(f64, f64) -> f64);
type T64 = (&'static str, fn(f64, f64, f64) -> f64);

fn unary32() -> Vec<U32> {
    vec![
        ("neg", |x| -x), ("abs", |x| x.abs()), ("sqrt", |x| x.sqrt()), ("floor", |x| x.floor()),
        ("ceil", |x| x.ceil()), ("round", |x| x.round()), ("round_ties_even", |x| x.round_ties_even()),
        ("trunc", |x| x.trunc()), ("fract", |x| x.fract()), ("signum", |x| x.signum()),
        ("recip", |x| x.recip()), ("to_radians", |x| x.to_radians()), ("to_degrees", |x| x.to_degrees()),
        ("clamp01", |x| x.clamp(0.0, 1.0)), ("clamp_m1_1", |x| x.clamp(-1.0, 1.0)),
        ("min0", |x| x.min(0.0)), ("max0", |x| x.max(0.0)), ("0min", |x| 0.0f32.min(x)),
        ("0max", |x| 0.0f32.max(x)), ("min_m0", |x| x.min(-0.0)), ("max_m0", |x| x.max(-0.0)),
        ("m0min", |x| (-0.0f32).min(x)), ("m0max", |x| (-0.0f32).max(x)), ("1min", |x| 1.0f32.min(x)),
        ("max_m1", |x| x.max(-1.0)), ("clamp_m0_1", |x| x.clamp(-0.0, 1.0)),
        ("sq", |x| x * x), ("half", |x| x * 0.5), ("div3", |x| x / 3.0), ("plus1", |x| x + 1.0),
        ("via_f64", |x| (x as f64) as f32), ("f64_sq", |x| ((x as f64) * (x as f64)) as f32),
        ("powi_2", |x| x.powi(2)), ("powi_3", |x| x.powi(3)), ("powi_4", |x| x.powi(4)),
        ("powi_5", |x| x.powi(5)), ("powi_7", |x| x.powi(7)), ("powi_m1", |x| x.powi(-1)),
        ("powi_m2", |x| x.powi(-2)), ("powi_m3", |x| x.powi(-3)), ("powi_31", |x| x.powi(31)),
        ("powi_0", |x| x.powi(0)), ("powi_bb", |x| x.powi(bb(6))),
        ("sin", |x| x.sin()), ("cos", |x| x.cos()), ("tan", |x| x.tan()), ("asin", |x| x.asin()),
        ("acos", |x| x.acos()), ("atan", |x| x.atan()), ("sinh", |x| x.sinh()), ("cosh", |x| x.cosh()),
        ("tanh", |x| x.tanh()), ("exp", |x| x.exp()), ("exp2", |x| x.exp2()), ("exp_m1", |x| x.exp_m1()),
        ("ln", |x| x.ln()), ("log2", |x| x.log2()), ("log10", |x| x.log10()), ("ln_1p", |x| x.ln_1p()),
        ("cbrt", |x| x.cbrt()), ("pow2.2", |x| x.powf(2.2)), ("sin_cos.0", |x| x.sin_cos().0),
        ("sin_cos.1", |x| x.sin_cos().1), ("bits_rt", |x| f32::from_bits(bb(x.to_bits()))),
        ("rem_euclid3", |x| x.rem_euclid(3.0)), ("div_euclid3", |x| x.div_euclid(3.0)),
    ]
}

fn binary32() -> Vec<B32> {
    vec![
        ("add", |a, b| a + b), ("sub", |a, b| a - b), ("mul", |a, b| a * b), ("div", |a, b| a / b),
        ("rem", |a, b| a % b), ("min", |a, b| a.min(b)), ("max", |a, b| a.max(b)),
        ("lt_sel", |a, b| if a < b { a } else { b }), ("gt_sel", |a, b| if a > b { a } else { b }),
        ("copysign", |a, b| a.copysign(b)), ("powf", |a, b| a.powf(b)), ("atan2", |a, b| a.atan2(b)),
        ("hypot", |a, b| a.hypot(b)), ("log", |a, b| a.log(b)), ("rem_euclid", |a, b| a.rem_euclid(b)),
        ("div_euclid", |a, b| a.div_euclid(b)),
        ("cmp", |a, b| (a.partial_cmp(&b).map_or(9, |o| o as i8 + 1)) as f32),
        ("total_cmp", |a, b| a.total_cmp(&b) as i8 as f32),
        ("eqs", |a, b| ((a == b) as u8 | ((a < b) as u8) << 1 | ((a <= b) as u8) << 2 | ((a != b) as u8) << 3) as f32),
        ("sum_f64", |a, b| ((a as f64) + (b as f64)) as f32),
        ("mul_f64", |a, b| ((a as f64) * (b as f64)) as f32),
        ("lerp", |a, b| a + (b - a) * 0.25),
        ("addmul", |a, b| (a + b) * (a - b)),
    ]
}

fn ternary32() -> Vec<T32> {
    vec![
        ("mul_then_add", |a, b, c| a * b + c), ("mul_then_sub", |a, b, c| a * b - c),
        ("c_plus_mul", |a, b, c| c + a * b), ("c_minus_mul", |a, b, c| c - a * b),
        ("mul_add", |a, b, c| a.mul_add(b, c)), ("neg_mul_add", |a, b, c| (-a).mul_add(b, c)),
        ("mul_add_negc", |a, b, c| a.mul_add(b, -c)), ("clamp", |a, b, c| if b <= c { a.clamp(b, c) } else { 7.0 }),
        ("sum3", |a, b, c| a + b + c), ("sum3r", |a, b, c| a + (b + c)), ("dot2", |a, b, c| a * b + b * c),
    ]
}

fn unary64() -> Vec<U64> {
    vec![
        ("neg", |x| -x), ("abs", |x| x.abs()), ("sqrt", |x| x.sqrt()), ("floor", |x| x.floor()),
        ("ceil", |x| x.ceil()), ("round", |x| x.round()), ("round_ties_even", |x| x.round_ties_even()),
        ("trunc", |x| x.trunc()), ("fract", |x| x.fract()), ("signum", |x| x.signum()),
        ("recip", |x| x.recip()), ("min0", |x| x.min(0.0)), ("max0", |x| x.max(0.0)),
        ("0min", |x| 0.0f64.min(x)), ("0max", |x| 0.0f64.max(x)), ("min_m0", |x| x.min(-0.0)),
        ("max_m0", |x| x.max(-0.0)), ("clamp01", |x| x.clamp(0.0, 1.0)),
        ("to_f32", |x| x as f32 as f64), ("to_f32_bits", |x| (x as f32).to_bits() as f64),
        ("powi_2", |x| x.powi(2)), ("powi_3", |x| x.powi(3)), ("powi_m1", |x| x.powi(-1)), ("powi_9", |x| x.powi(9)),
        ("sin", |x| x.sin()), ("cos", |x| x.cos()), ("tan", |x| x.tan()), ("asin", |x| x.asin()),
        ("acos", |x| x.acos()), ("atan", |x| x.atan()), ("tanh", |x| x.tanh()), ("exp", |x| x.exp()),
        ("exp2", |x| x.exp2()), ("exp_m1", |x| x.exp_m1()), ("ln", |x| x.ln()), ("log2", |x| x.log2()),
        ("log10", |x| x.log10()), ("ln_1p", |x| x.ln_1p()), ("cbrt", |x| x.cbrt()),
        ("pow2.2", |x| x.powf(2.2)), ("bits_rt", |x| f64::from_bits(bb(x.to_bits()))),
        ("rem_euclid3", |x| x.rem_euclid(3.0)),
    ]
}

fn binary64() -> Vec<B64> {
    vec![
        ("add", |a, b| a + b), ("sub", |a, b| a - b), ("mul", |a, b| a * b), ("div", |a, b| a / b),
        ("rem", |a, b| a % b), ("min", |a, b| a.min(b)), ("max", |a, b| a.max(b)),
        ("copysign", |a, b| a.copysign(b)), ("powf", |a, b| a.powf(b)), ("atan2", |a, b| a.atan2(b)),
        ("hypot", |a, b| a.hypot(b)), ("rem_euclid", |a, b| a.rem_euclid(b)),
        ("total_cmp", |a, b| a.total_cmp(&b) as i8 as f64),
        ("sum_to_f32", |a, b| (a + b) as f32 as f64),
    ]
}

fn ternary64() -> Vec<T64> {
    vec![
        ("mul_then_add", |a, b, c| a * b + c), ("c_minus_mul", |a, b, c| c - a * b),
        ("mul_add", |a, b, c| a.mul_add(b, c)), ("neg_mul_add", |a, b, c| (-a).mul_add(b, c)),
        ("sum3", |a, b, c| a + b + c),
    ]
}

fn f2i32() -> Vec<(&'static str, fn(f32) -> i128)> {
    vec![
        ("i8", |x| x as i8 as i128), ("u8", |x| x as u8 as i128), ("i16", |x| x as i16 as i128),
        ("u16", |x| x as u16 as i128), ("i32", |x| x as i32 as i128), ("u32", |x| x as u32 as i128),
        ("i64", |x| x as i64 as i128), ("u64", |x| x as u64 as i128), ("i128", |x| x as i128),
        ("u128", |x| x as u128 as i128), ("usize", |x| x as usize as i128), ("isize", |x| x as isize as i128),
        ("floor_u32", |x| x.floor() as u32 as i128), ("round_i32", |x| x.round() as i32 as i128),
    ]
}

fn f2i64() -> Vec<(&'static str, fn(f64) -> i128)> {
    vec![
        ("i8", |x| x as i8 as i128), ("u8", |x| x as u8 as i128), ("i32", |x| x as i32 as i128),
        ("u32", |x| x as u32 as i128), ("i64", |x| x as i64 as i128), ("u64", |x| x as u64 as i128),
        ("i128", |x| x as i128), ("u128", |x| x as u128 as i128), ("usize", |x| x as usize as i128),
    ]
}

fn i2f() -> Vec<(&'static str, fn(u64) -> u64)> {
    vec![
        ("i8_f32", |x| (x as i8 as f32).to_bits() as u64), ("u8_f32", |x| (x as u8 as f32).to_bits() as u64),
        ("i32_f32", |x| (x as i32 as f32).to_bits() as u64), ("u32_f32", |x| (x as u32 as f32).to_bits() as u64),
        ("i64_f32", |x| (x as i64 as f32).to_bits() as u64), ("u64_f32", |x| (x as f32).to_bits() as u64),
        ("i32_f64", |x| (x as i32 as f64).to_bits()), ("u32_f64", |x| (x as u32 as f64).to_bits()),
        ("i64_f64", |x| (x as i64 as f64).to_bits()), ("u64_f64", |x| (x as f64).to_bits()),
        ("usize_f32", |x| (x as usize as f32).to_bits() as u64),
        ("u128_f32", |x| (((x as u128) << 40 | 0xff_ffff) as f32).to_bits() as u64),
        ("i128_f64", |x| ((((x as i128) << 50) | 0x3_ffff) as f64).to_bits()),
        ("u128_f64", |x| ((((x as u128) << 64) | x as u128) as f64).to_bits()),
        ("u64_f64_f32", |x| (x as f64 as f32).to_bits() as u64),
    ]
}

fn h32(x: f32) -> u32 {
    x.to_bits()
}

fn battery(o: &mut String) {
    let s32 = specials32();
    let s64 = specials64();
    let mut r = Rng(0x1234_5678_9abc_def1);
    let mut in32 = s32.clone();
    for _ in 0..150 {
        in32.push(r.any32());
        in32.push(r.mid32());
    }
    let mut in64 = s64.clone();
    for _ in 0..150 {
        in64.push(r.any64());
        in64.push(r.mid64());
    }
    // pairs: specials x specials, then random pairs
    let mut p32: Vec<(f32, f32)> = Vec::new();
    for &a in &s32 {
        for &b in &s32 {
            p32.push((a, b));
        }
    }
    for _ in 0..1500 {
        p32.push((r.mid32(), r.mid32()));
        p32.push((r.any32(), r.mid32()));
    }
    let mut p64: Vec<(f64, f64)> = Vec::new();
    for &a in &s64 {
        for &b in &s64 {
            p64.push((a, b));
        }
    }
    for _ in 0..1500 {
        p64.push((r.mid64(), r.mid64()));
        p64.push((r.any64(), r.mid64()));
    }
    // triples: a subset of specials cubed, then random triples, then ones where a*b+c
    // and the fused form must differ (a*b exact needs more than 24 bits, c cancels it)
    let sub32: Vec<f32> = s32.iter().copied().step_by(3).collect();
    let mut t32: Vec<(f32, f32, f32)> = Vec::new();
    for &a in &sub32 {
        for &b in &sub32 {
            for &c in &sub32 {
                t32.push((a, b, c));
            }
        }
    }
    for _ in 0..3000 {
        let (a, b) = (r.mid32(), r.mid32());
        t32.push((a, b, r.mid32()));
        t32.push((a, b, -(a * b)));
    }
    let sub64: Vec<f64> = s64.iter().copied().step_by(3).collect();
    let mut t64: Vec<(f64, f64, f64)> = Vec::new();
    for &a in &sub64 {
        for &b in &sub64 {
            for &c in &sub64 {
                t64.push((a, b, c));
            }
        }
    }
    for _ in 0..3000 {
        let (a, b) = (r.mid64(), r.mid64());
        t64.push((a, b, r.mid64()));
        t64.push((a, b, -(a * b)));
    }

    for (name, f) in unary32() {
        let f = bb(f);
        for &x in &in32 {
            let _ = writeln!(o, "f32.{name} {:08x} => {:08x}", h32(x), h32(f(bb(x))));
        }
    }
    for (name, f) in binary32() {
        let f = bb(f);
        for &(a, b) in &p32 {
            let _ = writeln!(o, "f32.{name} {:08x} {:08x} => {:08x}", h32(a), h32(b), h32(f(bb(a), bb(b))));
        }
    }
    for (name, f) in ternary32() {
        let f = bb(f);
        for &(a, b, c) in &t32 {
            let _ = writeln!(o, "f32.{name} {:08x} {:08x} {:08x} => {:08x}", h32(a), h32(b), h32(c), h32(f(bb(a), bb(b), bb(c))));
        }
    }
    for (name, f) in unary64() {
        let f = bb(f);
        for &x in &in64 {
            let _ = writeln!(o, "f64.{name} {:016x} => {:016x}", x.to_bits(), f(bb(x)).to_bits());
        }
    }
    for (name, f) in binary64() {
        let f = bb(f);
        for &(a, b) in &p64 {
            let _ = writeln!(o, "f64.{name} {:016x} {:016x} => {:016x}", a.to_bits(), b.to_bits(), f(bb(a), bb(b)).to_bits());
        }
    }
    for (name, f) in ternary64() {
        let f = bb(f);
        for &(a, b, c) in &t64 {
            let _ = writeln!(o, "f64.{name} {:016x} {:016x} {:016x} => {:016x}", a.to_bits(), b.to_bits(), c.to_bits(), f(bb(a), bb(b), bb(c)).to_bits());
        }
    }
    for (name, f) in f2i32() {
        let f = bb(f);
        for &x in &in32 {
            let _ = writeln!(o, "cast.f32_{name} {:08x} => {:x}", h32(x), f(bb(x)));
        }
    }
    for (name, f) in f2i64() {
        let f = bb(f);
        for &x in &in64 {
            let _ = writeln!(o, "cast.f64_{name} {:016x} => {:x}", x.to_bits(), f(bb(x)));
        }
    }
    let is = ints();
    for (name, f) in i2f() {
        let f = bb(f);
        for &x in &is {
            let _ = writeln!(o, "cast.{name} {x:016x} => {:016x}", f(bb(x)));
        }
    }
    // integer arithmetic the float code leans on (fold unit columns, page strides)
    for &x in &is {
        let y = bb(x);
        let _ = writeln!(o, "int.mix {y:016x} => {:016x} {:016x} {:x} {:x}", y.wrapping_mul(0x9e37_79b9) >> 7, (y as u32 % 7) as u64 ^ (y as i64 >> 3) as u64, y.leading_zeros(), (y as u128 * 0xdead_beef_u128) >> 33);
    }

    formatting(o, &in32, &in64);
    reductions(o, &mut r, true);
    reductions(o, &mut r, false);
    sweep(o);
    luts(o);
    folded(o);
    consts(o);
}

fn formatting(o: &mut String, in32: &[f32], in64: &[f64]) {
    for &x in in32 {
        let x = bb(x);
        let a = format!("{x}|{x:?}|{x:e}|{x:.3}|{x:.0}|{x:10.4}|{x:+}");
        let back = format!("{x}").parse::<f32>().map(|v| v.to_bits()).unwrap_or(0xdead);
        let _ = writeln!(o, "fmt.f32 {:08x} => {a} {back:08x}", x.to_bits());
    }
    for &x in in64 {
        let x = bb(x);
        let a = format!("{x}|{x:?}|{x:e}|{x:.3}|{x:.17}");
        let back = format!("{x:?}").parse::<f64>().map(|v| v.to_bits()).unwrap_or(0xdead);
        let _ = writeln!(o, "fmt.f64 {:016x} => {a} {back:016x}", x.to_bits());
    }
    let strs = [
        "0", "-0", "1.5", "0.1", "1e-45", "1e-46", "7e-46", "3.4028235e38", "3.4028236e38", "3.40282357e38",
        "1.00000005960464477539062500000", "1.000000059604644775390625000001", "1.0000000596046447753906249999",
        "16777217", "16777219", "2.5e-324", "4.9e-324", "1.7976931348623157e308", "1.7976931348623159e308",
        "inf", "-infinity", "NaN", "nan", "1e400", "123456789012345678901234567890", ".5", "5.", "+3", "1_0",
        "0x10", "", " 1", "9007199254740993", "2.2250738585072011e-308", "0.30000000000000004",
    ];
    for s in strs {
        let s = bb(s);
        let a = s.parse::<f32>().map(|v| format!("{:08x}", v.to_bits())).unwrap_or_else(|e| format!("err:{e}"));
        let b = s.parse::<f64>().map(|v| format!("{:016x}", v.to_bits())).unwrap_or_else(|e| format!("err:{e}"));
        let _ = writeln!(o, "parse {s:?} => {a} {b}");
    }
}

/// Loops a vectoriser may reach for: sums, lane accumulators, element maps.
fn reductions(o: &mut String, r: &mut Rng, nans: bool) {
    let n = 4099;
    let mut a: Vec<f32> = (0..n).map(|_| r.mid32()).collect();
    let b: Vec<f32> = (0..n).map(|_| r.mid32()).collect();
    for (i, s) in specials32().into_iter().filter(|s| nans || s.is_finite()).enumerate() {
        a[i * 37 % n] = s;
    }
    let tag = if nans { "nan" } else { "finite" };
    let a = bb(a);
    let b = bb(b);
    let k = bb(1.0f32 / 3.0);
    let digest = |v: &[f32]| v.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ x.to_bits() as u64).wrapping_mul(0x100_0000_01b3));

    let sum: f32 = a.iter().sum();
    let mut s2 = 0.0f32;
    for i in 0..n {
        s2 += a[i];
    }
    let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
    let mut acc = [0.0f32; 4];
    for c in 0..n / 4 {
        for l in 0..4 {
            acc[l] += a[c * 4 + l] * b[c * 4 + l];
        }
    }
    let mut acc8 = [0.0f32; 8];
    for c in 0..n / 8 {
        for l in 0..8 {
            acc8[l] += a[c * 8 + l] - b[c * 8 + l] * k;
        }
    }
    let s64: f64 = a.iter().map(|&x| x as f64).sum();
    let _ = writeln!(o, "red.sum.{tag} => {:08x} {:08x} {:08x} {:016x}", sum.to_bits(), s2.to_bits(), dot.to_bits(), s64.to_bits());
    let _ = writeln!(o, "red.acc4.{tag} => {:08x} {:08x} {:08x} {:08x}", acc[0].to_bits(), acc[1].to_bits(), acc[2].to_bits(), acc[3].to_bits());
    let _ = writeln!(o, "red.acc8.{tag} => {:x?}", acc8.map(f32::to_bits));

    let maps: Vec<(&str, Vec<f32>)> = vec![
        ("axpy", a.iter().zip(&b).map(|(x, y)| x * k + y).collect()),
        ("ypax", a.iter().zip(&b).map(|(x, y)| y + k * x).collect()),
        ("min", a.iter().zip(&b).map(|(x, y)| x.min(*y)).collect()),
        ("max", a.iter().zip(&b).map(|(x, y)| x.max(*y)).collect()),
        ("lt_sel", a.iter().zip(&b).map(|(x, y)| if x < y { *x } else { *y }).collect()),
        ("div", a.iter().zip(&b).map(|(x, y)| x / y).collect()),
        ("sqrt", a.iter().map(|x| x.sqrt()).collect()),
        ("abs_neg", a.iter().map(|x| -x.abs()).collect()),
        ("floor", a.iter().map(|x| (x * 7.0).floor()).collect()),
        ("scan", a.iter().scan(0.0f32, |s, x| { *s += x; Some(*s) }).collect()),
        ("u32_to_f32", (0..n as u32).map(|i| (bb(i).wrapping_mul(2_654_435_761)) as f32 * k).collect()),
    ];
    for (name, v) in &maps {
        let _ = writeln!(o, "map.{name}.{tag} => {:016x}", digest(v));
    }
    // in-place element maps through indices (the shape layout loops have)
    let mut out = vec![0.0f32; n];
    for i in 0..n {
        out[i] = a[i] * b[i] + k;
    }
    let _ = writeln!(o, "map.idx_muladd.{tag} => {:016x}", digest(&out));
    let mut cells = vec![0u32; n];
    for i in 0..n {
        cells[i] = (a[i].abs() / 0.6).floor() as u32;
    }
    let _ = writeln!(o, "map.cells.{tag} => {:016x}", cells.iter().fold(0u64, |h, &c| h.wrapping_mul(31).wrapping_add(c as u64)));
    // a running f32 x over advances, the fold's shape
    let mut x = 0.0f32;
    let mut xs = Vec::with_capacity(n);
    for i in 0..n {
        x += if a[i] > 0.0 { 0.6 } else { 1.2 };
        xs.push(x);
    }
    let _ = writeln!(o, "map.running_x.{tag} => {:016x} {:08x}", digest(&xs), x.to_bits());
}

/// Many inputs through the functions glyph3d uses (and their neighbours), digested per
/// chunk of 1024 so the output stays small; `sweep <kernel> <chunk>` as the program's
/// arguments prints one chunk line by line.
fn sweep(o: &mut String) {
    let args: Vec<String> = std::env::args().collect();
    let pick = if args.len() >= 4 && args[1] == "sweep" { Some((args[2].clone(), args[3].parse::<usize>().unwrap_or(0))) } else { None };
    let mut r = Rng(0xfeed_f00d_dead_beef);
    let n = 1 << 17;
    let x32: Vec<f32> = (0..n).map(|i| if i % 2 == 0 { r.mid32() } else { r.any32() }).collect();
    let y32: Vec<f32> = (0..n).map(|_| r.mid32()).collect();
    let x64: Vec<f64> = (0..n).map(|i| if i % 2 == 0 { r.mid64() } else { r.any64() }).collect();
    let y64: Vec<f64> = (0..n).map(|_| r.mid64()).collect();
    let k32: Vec<(&str, fn(f32, f32) -> f32)> = vec![
        ("sin", |x, _| x.sin()), ("cos", |x, _| x.cos()), ("tan", |x, _| x.tan()), ("exp", |x, _| x.exp()),
        ("exp_neg", |x, _| (-x.abs()).exp()), ("ln", |x, _| x.ln()), ("sqrt", |x, _| x.sqrt()),
        ("powf", |x, y| x.abs().powf(y.rem_euclid(8.0))), ("pow2.2", |x, _| x.abs().powf(2.2)),
        ("atan2", |x, y| x.atan2(y)), ("sin_cos", |x, _| { let (s, c) = x.sin_cos(); s + c }),
        ("to_radians", |x, _| x.to_radians()), ("powi_bb", |x, y| x.powi(bb(y as i32 % 9))),
        ("mul_add", |x, y| x.mul_add(y, 0.1)), ("rem_euclid", |x, y| x.rem_euclid(y)),
        ("tanh", |x, _| x.tanh()), ("cbrt", |x, _| x.cbrt()), ("exp2", |x, _| x.exp2()), ("log2", |x, _| x.log2()),
    ];
    let k64: Vec<(&str, fn(f64, f64) -> f64)> = vec![
        ("sin", |x, _| x.sin()), ("cos", |x, _| x.cos()), ("tan", |x, _| x.tan()), ("exp", |x, _| x.exp()),
        ("exp_neg", |x, _| (-x.abs()).exp()), ("ln", |x, _| x.ln()), ("sqrt", |x, _| x.sqrt()),
        ("powf", |x, y| x.abs().powf(y.rem_euclid(8.0))), ("pow2.2", |x, _| x.abs().powf(2.2)),
        ("atan2", |x, y| x.atan2(y)), ("sin_cos", |x, _| { let (s, c) = x.sin_cos(); s + c }),
        ("mul_add", |x, y| x.mul_add(y, 0.1)), ("tanh", |x, _| x.tanh()), ("cbrt", |x, _| x.cbrt()),
        ("exp2", |x, _| x.exp2()), ("log2", |x, _| x.log2()), ("asin", |x, _| (x % 1.0).asin()),
    ];
    for (name, f) in k32 {
        let f = bb(f);
        for (c, (xs, ys)) in x32.chunks(1024).zip(y32.chunks(1024)).enumerate() {
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            let mut finite_only = 0xcbf2_9ce4_8422_2325u64;
            for (&x, &y) in xs.iter().zip(ys) {
                let v = f(bb(x), bb(y));
                h = (h ^ v.to_bits() as u64).wrapping_mul(0x100_0000_01b3);
                let q = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
                finite_only = (finite_only ^ q as u64).wrapping_mul(0x100_0000_01b3);
                if pick.as_ref().is_some_and(|(k, pc)| k == &format!("f32.{name}") && *pc == c) {
                    let _ = writeln!(o, "sweep.f32.{name} {:08x} {:08x} => {:08x}", x.to_bits(), y.to_bits(), v.to_bits());
                }
            }
            let _ = writeln!(o, "sweep.f32.{name} {c} => {finite_only:016x} {h:016x}");
        }
    }
    for (name, f) in k64 {
        let f = bb(f);
        for (c, (xs, ys)) in x64.chunks(1024).zip(y64.chunks(1024)).enumerate() {
            let mut h = 0xcbf2_9ce4_8422_2325u64;
            let mut finite_only = 0xcbf2_9ce4_8422_2325u64;
            for (&x, &y) in xs.iter().zip(ys) {
                let v = f(bb(x), bb(y));
                h = (h ^ v.to_bits()).wrapping_mul(0x100_0000_01b3);
                let q = if v.is_nan() { 0x7ff8_0000_0000_0000 } else { v.to_bits() };
                finite_only = (finite_only ^ q).wrapping_mul(0x100_0000_01b3);
                if pick.as_ref().is_some_and(|(k, pc)| k == &format!("f64.{name}") && *pc == c) {
                    let _ = writeln!(o, "sweep.f64.{name} {:016x} {:016x} => {:016x}", x.to_bits(), y.to_bits(), v.to_bits());
                }
            }
            let _ = writeln!(o, "sweep.f64.{name} {c} => {finite_only:016x} {h:016x}");
        }
    }
}

/// glyph3d's own tables, whole: the sRGB-ish LUTs (atlas.rs, tint.rs), the camera's
/// trig at the angles a drag reaches, the easing curves.
fn luts(o: &mut String) {
    let lut32: Vec<u32> = (0..256).map(|b| (bb(b) as f32 / 255.0).powf(2.2).to_bits()).collect();
    let lut64: Vec<u64> = (0..256).map(|b| (bb(b) as f64 / 255.0).powf(2.2).to_bits()).collect();
    let _ = writeln!(o, "lut.pow32 => {lut32:x?}");
    let _ = writeln!(o, "lut.pow64 => {lut64:x?}");
    let mut cam = Vec::new();
    for i in -720..=720 {
        let a = (bb(i) as f32 * 0.25).to_radians();
        let (s, c) = a.sin_cos();
        cam.push(s.to_bits() ^ c.to_bits().rotate_left(7));
    }
    let _ = writeln!(o, "lut.cam => {:016x}", cam.iter().fold(0u64, |h, &v| h.wrapping_mul(0x100_0000_01b3) ^ v as u64));
    let ease: Vec<u32> = (0..240).map(|i| (1.0 - (-14.0 * (bb(i) as f32 / 960.0)).exp()).clamp(0.0, 1.0).to_bits()).collect();
    let speed: Vec<u32> = (-40..40).map(|l| 1.1f32.powf(bb(l) as f32).to_bits()).collect();
    let _ = writeln!(o, "lut.ease => {ease:x?}");
    let _ = writeln!(o, "lut.speed => {speed:x?}");
}

/// The same operations on literals: whatever the optimiser folds, it must fold to the
/// bits the instruction would compute.
fn folded(o: &mut String) {
    macro_rules! u32s {
        ($($x:expr),*) => {$({
            let x: f32 = $x;
            let v = [-x, x.abs(), x.sqrt(), x.floor(), x.ceil(), x.round(), x.trunc(), x * x, x / 3.0,
                x + 0.1, x.min(1.0), x.max(-1.0), x.clamp(0.0, 1.0), (x as f64) as f32, x.mul_add(3.0, 0.1),
                x * 3.0 + 0.1, x.powi(3), x.powi(-2), x.signum(), x.rem_euclid(3.0), x.fract(), x.recip(),
                x.sin(), x.exp(), x.powf(2.2)];
            let c = [x as i8 as i64, x as u8 as i64, x as i32 as i64, x as u32 as i64, x as i64, x as u64 as i64];
            let _ = writeln!(o, "fold.f32 {:08x} => {:x?} {:x?}", x.to_bits(), v.map(f32::to_bits), c);
        })*};
    }
    u32s!(0.0, -0.0, 1.0e-45, 1.1754942e-38, 0.5, 1.5, 2.5, -2.5, 0.1, 1.0 / 3.0, 16777217.0, 2147483648.0,
        4294967296.0, -2147483904.0, 1.8446744e19, 3.4028235e38, f32::INFINITY, f32::NEG_INFINITY, f32::NAN,
        -f32::NAN, 255.5, -129.0, 65535.9, 1.0e10, 8388607.5, 0.49999997, f32::from_bits(0x7fc12345),
        f32::from_bits(0x7f800001), f32::EPSILON, 1.0 + f32::EPSILON, 2.2);
    macro_rules! b32s {
        ($(($a:expr, $b:expr)),*) => {$({
            let (a, b): (f32, f32) = ($a, $b);
            let v = [a + b, a - b, a * b, a / b, a % b, a.min(b), a.max(b), a.copysign(b), a * b + 0.5,
                a.mul_add(b, 0.5), a.powf(b), a.atan2(b), a.hypot(b), ((a as f64) * (b as f64)) as f32];
            let _ = writeln!(o, "fold.f32x2 {:08x} {:08x} => {:x?}", a.to_bits(), b.to_bits(), v.map(f32::to_bits));
        })*};
    }
    b32s!((0.1, 0.2), (1.0, 3.0), (f32::MAX, 2.0), (f32::MAX, f32::MAX), (1.0e-45, 0.5), (-0.0, 0.0),
        (0.0, -0.0), (f32::NAN, 1.0), (1.0, f32::NAN), (f32::from_bits(0x7fc12345), f32::from_bits(0xffd54321)),
        (f32::from_bits(0x7f800001), 1.0), (f32::INFINITY, f32::NEG_INFINITY), (5.5, -2.0), (-5.5, 2.0),
        (1.0000001, 0.99999994), (16777216.0, 1.0), (1.17549435e-38, 0.5), (3.0, 1.0e-40), (2.0, 0.5),
        (1.1, 1.0e-8), (0.0, 0.0), (1.0, 0.0), (-1.0, 0.0));
    macro_rules! u64s {
        ($($x:expr),*) => {$({
            let x: f64 = $x;
            let v = [-x, x.sqrt(), x.floor(), x.round(), x * x, x / 3.0, x + 0.1, x.min(1.0), x.mul_add(3.0, 0.1),
                x * 3.0 + 0.1, x.powi(3), (x as f32) as f64, x.sin(), x.exp(), x.powf(2.2), x.ln()];
            let c = [x as i32 as i64, x as u32 as i64, x as i64, x as u64 as i64, (x as f32).to_bits() as i64];
            let _ = writeln!(o, "fold.f64 {:016x} => {:x?} {:x?}", x.to_bits(), v.map(f64::to_bits), c);
        })*};
    }
    u64s!(0.0, -0.0, 5e-324, 2.2250738585072014e-308, 0.5, 2.5, 0.1, 1.0 / 3.0, 9007199254740993.0,
        9.223372036854776e18, 1.8446744073709552e19, 3.4028235677973366e38, 3.402823669209385e38, 1.401298464324817e-45,
        7.006492321624085e-46, 7.006492321624087e-46, f64::NAN, f64::INFINITY, f64::MAX, 1.0000000596046448,
        1.0000001788139343, 255.0 / 255.0, 0.30000000000000004, f64::from_bits(0x7ff8_0000_1234_5678));
    macro_rules! i2fs {
        ($($x:expr),*) => {$({
            let x: u64 = $x;
            let v = [(x as f32).to_bits() as u64, (x as i64 as f32).to_bits() as u64, (x as u32 as f32).to_bits() as u64,
                (x as i32 as f32).to_bits() as u64, (x as f64).to_bits(), (x as i64 as f64).to_bits(), (x as f64 as f32).to_bits() as u64];
            let _ = writeln!(o, "fold.i2f {x:016x} => {v:x?}");
        })*};
    }
    i2fs!(0, 1, 16777217, 16777219, 0x8000_0000, 0xffff_ffff, 0x8000_0080_0000_0001, 0x8000_0080_0000_0000,
        0x8000_0180_0000_0000, u64::MAX, 0x7fff_ffff_ffff_ffff, 0x0020_0000_0000_0001, 0x0020_0000_2000_0001,
        0xffff_ff7f_ffff_ffff, 0xffff_ff80_0000_0000);
}

const C32: [f32; 24] = [
    0.1 + 0.2, 1.0 / 3.0, 16777217.0, f32::MAX * 2.0, -0.0 + 0.0, 0.1f64 as f32, 16777217u32 as f32,
    u64::MAX as f32, 1.0e-45 * 0.5, f32::MIN_POSITIVE / 3.0, 0x8000_0080_0000_0001u64 as f32,
    (1.0 / 3.0) * 3.0, 3.4028235e38 + 1.0e31, 0.1 * 3.0, 1.0 - 0.9, 2.5f32.min(f32::NAN), f32::NAN.max(-1.0),
    (-2.5f32).abs(), 1.5f32.copysign(-0.0), 7.0 % 2.5, -7.5 % 2.0, 1.0e10 * 1.0e10 / 1.0e10, 0.6 + 0.6 + 0.6,
    (0.5f64 + f32::EPSILON as f64 / 4.0) as f32,
];
const C64: [f64; 10] = [
    0.1 + 0.2, 1.0 / 3.0, 9007199254740993.0, u64::MAX as f64, 5e-324 * 0.5, f64::MAX + 1.0e292,
    0.1f32 as f64, 7.0 % 2.5, (1.0f64).min(f64::NAN), 2.2250738585072014e-308 / 3.0,
];
const CI: [i64; 12] = [
    3.9f32 as i32 as i64, -1.0f32 as u8 as i64, f32::NAN as i32 as i64, 1.0e10f32 as i32 as i64,
    -1.0e10f32 as i32 as i64, 300.7f32 as u8 as i64, f32::INFINITY as u64 as i64, -0.99f32 as u32 as i64,
    1.0e20f64 as i64, f64::NAN as u64 as i64, 2147483647.5f64 as i32 as i64, -2147483648.9f64 as i32 as i64,
];
const CB: [u32; 4] = [f32::NAN.to_bits(), (-f32::NAN).to_bits(), f32::from_bits(0x7fa0_0001).to_bits(), (1.0f32 / 0.0).to_bits()];

fn consts(o: &mut String) {
    let _ = writeln!(o, "const.f32 => {:x?}", bb(C32).map(f32::to_bits));
    let _ = writeln!(o, "const.f64 => {:x?}", bb(C64).map(f64::to_bits));
    let _ = writeln!(o, "const.int => {:x?}", bb(CI));
    let _ = writeln!(o, "const.bits => {:x?}", bb(CB));
    // the same expressions evaluated at run time, for comparison inside one build
    let rt = [bb(0.1f32) + bb(0.2), bb(1.0f32) / bb(3.0), bb(f32::MAX) * bb(2.0), (bb(0.5f64) + f32::EPSILON as f64 / 4.0) as f32];
    let _ = writeln!(o, "const.rt => {:x?}", rt.map(f32::to_bits));
}

fn main() {
    // RELAX_FIDELITY_REPS: more repetitions give relax's background tiers (O2, O3) time
    // to install their builds while the battery is still running
    let reps: usize = std::env::var("RELAX_FIDELITY_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let mut first = String::new();
    let mut same = Vec::new();
    for rep in 0..reps {
        let mut o = String::new();
        battery(&mut o);
        if rep == 0 {
            first = o;
        } else {
            let diff: Vec<(&str, &str)> = first.lines().zip(o.lines()).filter(|(a, b)| a != b).collect();
            same.push(diff.len() + first.lines().count().abs_diff(o.lines().count()));
            for (a, b) in diff.iter().take(20) {
                eprintln!("rep {rep} differs from rep 0:\n  rep 0: {a}\n  rep {rep}: {b}");
            }
        }
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(first.as_bytes());
    let digest = first.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    let _ = writeln!(out, "reps: {reps}, lines differing from rep 0: {same:?}");
    let _ = writeln!(out, "lines: {}, digest: {digest:016x}", first.lines().count());
}
