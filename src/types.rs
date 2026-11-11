use std::fmt;

use crate::error::DbResult;
use crate::{bail, db_err};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    BigInt,
    /// Maximum length in Unicode characters.
    Varchar(u32),
    /// Fixed dimension of float32 elements.
    Vector(u32),
}

impl DataType {
    pub fn tag(self) -> u8 {
        match self {
            DataType::BigInt => 1,
            DataType::Varchar(_) => 2,
            DataType::Vector(_) => 3,
        }
    }

    pub fn param(self) -> u32 {
        match self {
            DataType::BigInt => 0,
            DataType::Varchar(n) | DataType::Vector(n) => n,
        }
    }

    pub fn from_parts(tag: u8, param: u32) -> Option<DataType> {
        match tag {
            1 => Some(DataType::BigInt),
            2 => Some(DataType::Varchar(param)),
            3 => Some(DataType::Vector(param)),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::BigInt => write!(f, "BIGINT"),
            DataType::Varchar(n) => write!(f, "VARCHAR({n})"),
            DataType::Vector(n) => write!(f, "VECTOR({n})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Str(String),
    Vector(Vec<f32>),
}

/// Length of a VARCHAR value as the schema counts it: Unicode scalar values.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Parses a vector literal such as `[1, 0.5, -2e3]`. Every element must be a
/// plain decimal number that rounds to a finite float32; NaN, infinities and
/// float32 overflow are rejected. Dimension is checked by the caller.
pub fn parse_vector(text: &str) -> DbResult<Vec<f32>> {
    if let Some(v) = parse_vector_fast(text.as_bytes()) {
        return Ok(v);
    }
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| db_err!(InvalidValue, "invalid vector literal '{}'", text))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|e| parse_f32_strict(e.trim(), text))
        .collect()
}

/// Single-pass path for the usual shape `[d.ddd,-d.ddd,...]`. It accepts
/// only ASCII decimals whose digits form a significand of at most 2^24 and
/// whose decimal exponent is at most 10 in magnitude. Both factors are then
/// exact float32 values, so one float32 multiply or divide rounds exactly
/// as `str::parse::<f32>` does (Clinger's fast path). Any other input,
/// malformed ones included, returns None and takes the general path.
fn parse_vector_fast(b: &[u8]) -> Option<Vec<f32>> {
    const POW10: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];
    let skip_ws = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    // Appends the digits at `i` to `m`; returns the new position and count.
    let digits = |mut i: usize, m: &mut u64| {
        let start = i;
        while let Some(d) = b.get(i).map(|c| c.wrapping_sub(b'0')).filter(|&d| d <= 9) {
            *m = m.wrapping_mul(10).wrapping_add(u64::from(d));
            i += 1;
        }
        (i, i - start)
    };
    let mut i = skip_ws(0);
    if b.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut out = Vec::with_capacity(b.len() / 8 + 1);
    loop {
        i = skip_ws(i);
        let neg = b.get(i) == Some(&b'-');
        // The benchmark's shape first: its sign is skipped without a branch
        // and its eight bytes are checked and converted at once.
        let at = i + usize::from(neg);
        if let Some(m) = b.get(at..).and_then(six_decimals) {
            out.push(with_sign(m as f32 / 1e6, neg));
            i = skip_ws(at + 8);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b']') => break,
                _ => return None,
            }
            continue;
        }
        if neg || b.get(i) == Some(&b'+') {
            i += 1;
        }
        let mut m = 0u64;
        let (j, int_digits) = digits(i, &mut m);
        let (mut frac_digits, mut exp) = (0, 0i32);
        i = j;
        if b.get(i) == Some(&b'.') {
            (i, frac_digits) = digits(i + 1, &mut m);
            exp = -(frac_digits as i32);
        }
        // 19 digits cannot wrap a u64, so m is exact whenever it is checked.
        let n = int_digits + frac_digits;
        if n == 0 || n > 19 || m > 1 << 24 {
            return None;
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            let eneg = b.get(i) == Some(&b'-');
            if eneg || b.get(i) == Some(&b'+') {
                i += 1;
            }
            let mut e = 0u64;
            let (j, n) = digits(i, &mut e);
            if n == 0 || n > 3 {
                return None;
            }
            i = j;
            exp += if eneg { -(e as i32) } else { e as i32 };
        }
        let p = *POW10.get(exp.unsigned_abs() as usize)?;
        let mag = if exp >= 0 { m as f32 * p } else { m as f32 / p };
        out.push(with_sign(mag, neg));
        i = skip_ws(i);
        match b.get(i) {
            Some(b',') => i += 1,
            Some(b']') => break,
            _ => return None,
        }
    }
    (skip_ws(i + 1) == b.len()).then_some(out)
}

/// `-mag` when `neg`, for a `mag` that is never negative, by setting the
/// sign bit: `if neg { -mag } else { mag }` compiles to a branch, which
/// random signs mispredict half the time (11.8 against 8.7 ns per element).
#[inline(always)]
fn with_sign(mag: f32, neg: bool) -> f32 {
    f32::from_bits(mag.to_bits() | (u32::from(neg) << 31))
}

/// Largest magnitude a 24-bit vector element holds, in millionths: the 23
/// bits below its sign bit.
pub const MILLIONTHS_MAX: u32 = (1 << 23) - 1;

/// The 24-bit code of `x` (its sign bit, then |x| in millionths) and whether
/// `from_millionths` gives `x` back bit for bit. It does exactly for the
/// float32 nearest m / 10^6 with m <= MILLIONTHS_MAX, which is what every
/// element written `[-]d.dddddd` with d < 8 parses to: rounding |x| * 10^6
/// finds that m, since half an ulp of any |x| < 8.39 is under 0.48
/// millionths, and the check rejects every other value. No branches, so
/// loops over it vectorize.
#[inline(always)]
pub fn millionths(x: f32) -> (u32, bool) {
    // Through i32, which SSE2 converts four at a time; u32 it does not.
    let m = (f64::from(x.abs()) * 1e6 + 0.5) as i32 as u32;
    let code = (m & MILLIONTHS_MAX) | (x.to_bits() >> 31 << 23);
    (code, m <= MILLIONTHS_MAX && from_millionths(code).to_bits() == x.to_bits())
}

/// The element a `millionths` code stands for: the same float32 division
/// the parser does for `d.dddddd`, then the sign bit.
#[inline(always)]
pub fn from_millionths(code: u32) -> f32 {
    with_sign((code & MILLIONTHS_MAX) as i32 as f32 / 1e6, code >> 23 & 1 != 0)
}

/// Bytes of a stored `millionths` code (LE).
pub const MILLIONTHS_BYTES: usize = 3;

/// Stores the codes of `v` in the first `MILLIONTHS_BYTES * v.len()` bytes
/// of `out` and returns whether every element has one; if not, those bytes
/// hold nothing useful.
#[inline]
pub fn put_millionths(v: &[f32], out: &mut [u8]) -> bool {
    let out = &mut out[..v.len() * MILLIONTHS_BYTES];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU supports AVX2, checked just above.
        let (done, ok) = unsafe { put_millionths_avx2(v, out) };
        return put_millionths_scalar(&v[done..], &mut out[done * MILLIONTHS_BYTES..]) & ok;
    }
    put_millionths_scalar(v, out)
}

pub(crate) fn put_millionths_scalar(v: &[f32], out: &mut [u8]) -> bool {
    let mut ok = true;
    for (b, &x) in out.as_chunks_mut::<MILLIONTHS_BYTES>().0.iter_mut().zip(v) {
        let (code, good) = millionths(x);
        ok &= good;
        b.copy_from_slice(&code.to_le_bytes()[..MILLIONTHS_BYTES]);
    }
    ok
}

/// `millionths` eight elements at a time, in the same arithmetic, the codes
/// packed into 24 bytes. A magnitude past i32 (or NaN) converts to
/// 0x80000000, which has bits outside `MILLIONTHS_MAX` and so no code, as
/// the scalar path's saturated value has none. Returns how many elements it
/// stored (all but the last `v.len() % 8`) and whether each had a code.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn put_millionths_avx2(v: &[f32], out: &mut [u8]) -> (usize, bool) {
    use std::arch::x86_64::*;
    debug_assert!(out.len() >= v.len() * MILLIONTHS_BYTES);
    let magnitude = _mm256_set1_epi32(MILLIONTHS_MAX as i32);
    let (million_f64, half) = (_mm256_set1_pd(1e6), _mm256_set1_pd(0.5));
    let million = _mm256_set1_ps(1e6);
    // Each lane's three low bytes to the front of its half, then the halves'
    // twelve bytes next to each other.
    let pack = _mm256_setr_epi8(
        0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1, 0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1,
    );
    let join = _mm256_setr_epi32(0, 1, 2, 4, 5, 6, 3, 7);
    let mut good = _mm256_set1_epi32(-1);
    let mut i = 0;
    while i + 8 <= v.len() {
        let x = _mm256_loadu_ps(v.as_ptr().add(i));
        let bits = _mm256_castps_si256(x);
        let abs = _mm256_and_ps(x, _mm256_castsi256_ps(_mm256_set1_epi32(i32::MAX)));
        let round = |h: __m128| _mm256_cvttpd_epi32(_mm256_add_pd(_mm256_mul_pd(_mm256_cvtps_pd(h), million_f64), half));
        let m = _mm256_set_m128i(round(_mm256_extractf128_ps::<1>(abs)), round(_mm256_castps256_ps128(abs)));
        let mag = _mm256_and_si256(m, magnitude);
        let sign = _mm256_slli_epi32::<31>(_mm256_srli_epi32::<31>(bits));
        let back = _mm256_or_si256(_mm256_castps_si256(_mm256_div_ps(_mm256_cvtepi32_ps(mag), million)), sign);
        good = _mm256_and_si256(good, _mm256_and_si256(_mm256_cmpeq_epi32(m, mag), _mm256_cmpeq_epi32(back, bits)));
        let code = _mm256_or_si256(mag, _mm256_srli_epi32::<8>(sign));
        let packed = _mm256_permutevar8x32_epi32(_mm256_shuffle_epi8(code, pack), join);
        let p = out.as_mut_ptr().add(i * MILLIONTHS_BYTES);
        _mm_storeu_si128(p as *mut __m128i, _mm256_castsi256_si128(packed));
        _mm_storel_epi64(p.add(16) as *mut __m128i, _mm256_extracti128_si256::<1>(packed));
        i += 8;
    }
    (i, _mm256_movemask_epi8(good) == -1)
}

/// Appends the elements whose codes `codes` holds.
#[inline]
pub fn extend_from_millionths(codes: &[u8], out: &mut Vec<f32>) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        out.reserve(codes.len() / MILLIONTHS_BYTES);
        // SAFETY: the CPU supports AVX2, checked just above, and `out` has
        // room for every element of `codes`, of which the call writes the
        // first `done`.
        let done = unsafe {
            let done = from_millionths_avx2(codes, out.as_mut_ptr().add(out.len()));
            out.set_len(out.len() + done);
            done
        };
        return extend_from_millionths_scalar(&codes[done * MILLIONTHS_BYTES..], out);
    }
    extend_from_millionths_scalar(codes, out)
}

pub(crate) fn extend_from_millionths_scalar(codes: &[u8], out: &mut Vec<f32>) {
    let codes = codes.as_chunks::<MILLIONTHS_BYTES>().0;
    out.extend(codes.iter().map(|&[a, b, c]| from_millionths(u32::from_le_bytes([a, b, c, 0]))));
}

/// Eight codes per step: 32 bytes loaded, the first 24 spread into eight
/// u32 lanes, then the scalar path's division and sign bit, lane by lane.
/// Stops while 32 bytes still fit, so the last ten codes or fewer are left
/// to the caller; returns how many it wrote to `dst`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn from_millionths_avx2(codes: &[u8], dst: *mut f32) -> usize {
    use std::arch::x86_64::*;
    // Dwords 0-2 (codes 0-3) to the low lane, dwords 3-5 (codes 4-7) to the
    // high one, then each code's three bytes to a lane of its own.
    let halves = _mm256_setr_epi32(0, 1, 2, 0, 3, 4, 5, 0);
    let spread = _mm256_setr_epi8(
        0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1, 0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, -1,
    );
    let magnitude = _mm256_set1_epi32(MILLIONTHS_MAX as i32);
    let sign = _mm256_set1_epi32(1 << 23);
    let million = _mm256_set1_ps(1e6);
    let mut i = 0;
    while (i + 8) * MILLIONTHS_BYTES + 8 <= codes.len() {
        let raw = _mm256_loadu_si256(codes.as_ptr().add(i * MILLIONTHS_BYTES) as *const __m256i);
        let c = _mm256_shuffle_epi8(_mm256_permutevar8x32_epi32(raw, halves), spread);
        let mag = _mm256_div_ps(_mm256_cvtepi32_ps(_mm256_and_si256(c, magnitude)), million);
        let neg = _mm256_slli_epi32::<8>(_mm256_and_si256(c, sign));
        _mm256_storeu_ps(dst.add(i), _mm256_or_ps(mag, _mm256_castsi256_ps(neg)));
        i += 8;
    }
    i
}

/// For `d.dddddd` followed by a byte that cannot continue a number (the
/// shape `f"{x:.6f}"` gives for |x| < 10), its value times 10^6. That is at
/// most 9,999,999 < 2^24, so dividing it by 1e6 in float32 is Clinger's fast
/// path again. The eight bytes are tested and summed as one u64 (the digit
/// test and the conversion are simdjson's).
#[inline(always)]
fn six_decimals(b: &[u8]) -> Option<u32> {
    let (head, next) = (b.get(..8)?, *b.get(8)?);
    if head[1] != b'.' || next.is_ascii_digit() || matches!(next, b'.' | b'e' | b'E') {
        return None;
    }
    // The '.' made a '0', so a valid shape is eight ASCII digits.
    let x = u64::from_le_bytes(head.try_into().ok()?) & !0xFF00 | 0x3000;
    let hi = 0xF0F0_F0F0_F0F0_F0F0u64;
    if (x & hi) | ((x.wrapping_add(0x0606_0606_0606_0606) & hi) >> 4) != 0x3333_3333_3333_3333 {
        return None;
    }
    // The first byte is the most significant digit: pairs, then fours,
    // then the eight-digit number d * 10^7 + the six decimals.
    let mut v = x - 0x3030_3030_3030_3030;
    v = v.wrapping_mul(10).wrapping_add(v >> 8);
    v = (v & 0x0000_00FF_0000_00FF)
        .wrapping_mul(100 + (1_000_000 << 32))
        .wrapping_add(((v >> 16) & 0x0000_00FF_0000_00FF).wrapping_mul(1 + (10_000 << 32)))
        >> 32;
    Some(v as u32 - u32::from(head[0] - b'0') * 9_000_000)
}

fn parse_f32_strict(elem: &str, whole: &str) -> DbResult<f32> {
    if !is_plain_number(elem) {
        bail!(InvalidValue, "invalid vector element '{}' in '{}'", elem, whole);
    }
    let v: f32 = elem
        .parse()
        .map_err(|_| db_err!(InvalidValue, "invalid vector element '{}' in '{}'", elem, whole))?;
    if !v.is_finite() {
        bail!(InvalidValue, "vector element '{}' is out of float32 range", elem);
    }
    Ok(v)
}

/// `[+-] digits [. digits] [(e|E) [+-] digits]` with at least one mantissa
/// digit. Rejects words like `nan`/`inf` that `str::parse` would accept.
fn is_plain_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn vector_literals() {
        assert_eq!(parse_vector("[1, 0, 0.5, -2]").unwrap(), vec![1.0, 0.0, 0.5, -2.0]);
        assert_eq!(parse_vector(" [ 1e2 ,+.5, 5. ] ").unwrap(), vec![100.0, 0.5, 5.0]);
        assert_eq!(parse_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(parse_vector("[3.4028235e38]").unwrap(), vec![f32::MAX]);
        assert_eq!(parse_vector("[1e-50]").unwrap(), vec![0.0]);
    }

    #[test]
    fn vector_rejects() {
        for bad in [
            "[nan]", "[NaN]", "[inf]", "[-Infinity]", "[1e39]", "[-3.5e38]", "[1,,2]", "[1 2]",
            "1,2", "[1,2", "[0x10]", "[1e]", "[.]", "[1, 'a']", "[1,]",
        ] {
            let e = parse_vector(bad).unwrap_err();
            assert_eq!(e.kind, ErrorKind::InvalidValue, "{bad}");
        }
    }

    #[test]
    fn fast_path_equals_general_parse() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut fast = 0;
        for _ in 0..200_000 {
            let mut e = String::new();
            match next(3) {
                0 => e.push('-'),
                1 if next(4) == 0 => e.push('+'),
                _ => {}
            }
            for _ in 0..next(4) {
                e.push((b'0' + next(10) as u8) as char);
            }
            if next(4) != 0 {
                e.push('.');
                for _ in 0..next(10) {
                    e.push((b'0' + next(10) as u8) as char);
                }
            }
            if next(5) == 0 {
                e.push_str(["e", "E", "e-", "e+"][next(4) as usize]);
                e.push_str(&next(14).to_string());
            }
            let text = format!("[{e}]");
            let general = parse_f32_strict(&e, &text);
            if let Some(v) = parse_vector_fast(text.as_bytes()) {
                fast += 1;
                let g = general.unwrap_or_else(|err| panic!("fast path accepted {text}: {err}"));
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].to_bits(), g.to_bits(), "{text}");
            }
        }
        assert!(fast > 100_000, "fast path taken {fast} times");
    }

    /// The eight-byte path takes `[-]d.dddddd` elements wherever they stand
    /// and leaves the shapes next to it (one more digit, an exponent, a
    /// second integer digit, a '+') to the digit loop: every element of
    /// every vector the fast path accepts gets `str::parse`'s bits, and it
    /// accepts every vector of the eight-byte shape alone.
    #[test]
    fn six_decimals_equals_str_parse() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let is_eight = |e: &str| {
            let b = e.strip_prefix('-').unwrap_or(e).as_bytes();
            b.len() == 8 && b[1] == b'.'
        };
        let (mut eight, mut only_eight) = (0, 0);
        for _ in 0..60_000 {
            let elems: Vec<String> = (0..next(12) + 1)
                .map(|_| {
                    let m = next(10_000_000);
                    let sign = ["", "-", "+"][[0, 0, 0, 1, 1, 1, 2][next(7) as usize]];
                    let int = if next(8) == 0 { 10 + m / 1_000_000 } else { m / 1_000_000 };
                    let tail = ["", "", "", "", "7", "e1", "E-2", "0e+3"][next(8) as usize];
                    format!("{sign}{int}.{:06}{tail}", m % 1_000_000)
                })
                .collect();
            let mut text = String::from([" [", "["][next(2) as usize]);
            for (k, e) in elems.iter().enumerate() {
                if k > 0 {
                    text.push_str([",", ", ", " ,", " , "][next(4) as usize]);
                }
                text.push_str(e);
            }
            text.push_str(["]", " ]", "] "][next(3) as usize]);
            let got = parse_vector_fast(text.as_bytes());
            if elems.iter().all(|e| is_eight(e)) {
                assert!(got.is_some(), "fast path refused {text}");
                only_eight += 1;
            }
            let Some(got) = got else { continue };
            assert_eq!(got.len(), elems.len(), "{text}");
            for (g, e) in got.iter().zip(&elems) {
                let want = parse_f32_strict(e, &text).unwrap();
                assert_eq!(g.to_bits(), want.to_bits(), "{e} in {text}");
                eight += usize::from(is_eight(e));
            }
        }
        assert!(eight > 20_000 && only_eight > 2_000, "eight-byte shape checked {eight} times, alone in {only_eight} vectors");
        for (text, m) in [("0.000000]", 0), ("9.999999,", 9_999_999), ("1.000000 ", 1_000_000), ("0.000001]", 1)] {
            assert_eq!(six_decimals(text.as_bytes()), Some(m), "{text}");
        }
        for text in ["0.0000001", "1.000000e", "1.000000.", "1.00000]", "1,000000]", "a.000000]", "1.00000a]", "1.000000"] {
            assert_eq!(six_decimals(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn fast_path_covers_the_benchmark_format() {
        let text = "[-0.123456,0.999999,-0.000000,0.000001,1.000000, -0.5 ]";
        let v = parse_vector_fast(text.as_bytes()).expect("fast path");
        let want: Vec<f32> = ["-0.123456", "0.999999", "-0.000000", "0.000001", "1.000000", "-0.5"]
            .iter()
            .map(|e| e.parse().unwrap())
            .collect();
        assert_eq!(v.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), want.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
        for slow in ["[]", "[ ]", "[1,]", "[1e]", "[0.12345678901]", "[1e11]", "[1\u{a0}]", "[1]x"] {
            assert!(parse_vector_fast(slow.as_bytes()).is_none(), "{slow}");
        }
    }

    /// Every code, both signs, against the value the parser makes of it;
    /// a sample against `str::parse` of the decimal it stands for.
    #[test]
    fn millionths_round_trip() {
        for m in 0..=MILLIONTHS_MAX {
            for neg in [false, true] {
                let x = with_sign(m as f32 / 1e6, neg);
                let code = m | u32::from(neg) << 23;
                assert_eq!(millionths(x), (code, true), "{m} {neg}");
                assert_eq!(from_millionths(code).to_bits(), x.to_bits(), "{m} {neg}");
            }
        }
        for m in (0..=MILLIONTHS_MAX).step_by(997).chain([MILLIONTHS_MAX]) {
            let text = format!("-{}.{:06}", m / 1_000_000, m % 1_000_000);
            let x: f32 = text.parse().unwrap();
            assert_eq!(millionths(x), (m | 1 << 23, true), "{text}");
            assert_eq!(millionths(x.abs()), (m, true), "{text}");
        }
    }

    #[test]
    fn millionths_rejects() {
        let near = |x: f32, up: bool| f32::from_bits(if up { x.to_bits() + 1 } else { x.to_bits() - 1 });
        for x in [
            8.388608, -8.388608, 9.5, 1e7, f32::MAX, f32::INFINITY, f32::NAN, 1e-7, -4e-7, 0.1234567,
            f32::MIN_POSITIVE, f32::from_bits(1), near(0.1, true), near(-0.1, false), near(8.388607, true),
        ] {
            assert!(!millionths(x).1, "{x:e}");
        }
    }

    /// Codes written eight at a time equal the scalar path's for the
    /// elements of every code, and both refuse the same vectors: floats next
    /// to coded ones, special values, magnitudes past the codes and past
    /// i32, at every position of every length up to 40.
    #[test]
    fn writing_codes() {
        let floats: Vec<f32> = (0..1u32 << 24).map(from_millionths).collect();
        let (mut fast, mut slow) = (vec![0u8; 3 << 16], vec![0u8; 3 << 16]);
        for chunk in floats.chunks(1 << 16) {
            assert!(put_millionths(chunk, &mut fast));
            assert!(put_millionths_scalar(chunk, &mut slow));
            assert_eq!(fast, slow);
        }
        let near = |x: f32, up: bool| f32::from_bits(if up { x.to_bits() + 1 } else { x.to_bits() - 1 });
        let bad = [
            near(0.1, true), near(-0.1, false), near(8.388607, true), 8.388608, 1e-7, f32::MIN_POSITIVE,
            f32::from_bits(1), f32::NAN, -f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 2147.4836, 2147.4837, 3e9, f32::MAX,
        ];
        let mut out = vec![0u8; 40 * MILLIONTHS_BYTES];
        for len in 0..=40usize {
            let v: Vec<f32> = floats[4_000_000..][..len].to_vec();
            assert!(put_millionths(&v, &mut out) && put_millionths_scalar(&v, &mut out), "{len}");
            for at in 0..len {
                for &b in &bad {
                    let mut w = v.clone();
                    w[at] = b;
                    assert!(!put_millionths(&w, &mut out), "{b:e} at {at} of {len}");
                    assert!(!put_millionths_scalar(&w, &mut out), "{b:e} at {at} of {len}");
                }
            }
        }
    }

    /// Every 24-bit code, and every length from 0 to 40 from four starting
    /// bytes, decode to the scalar path's bits; `put_millionths` writes the
    /// decoded elements back as the same bytes.
    #[test]
    fn decoding_codes() {
        let all: Vec<u8> = (0..1u32 << 24).flat_map(|c| c.to_le_bytes().into_iter().take(MILLIONTHS_BYTES)).collect();
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        let (mut fast, mut slow) = (Vec::new(), Vec::new());
        for chunk in all.chunks(3 << 16) {
            fast.clear();
            slow.clear();
            extend_from_millionths(chunk, &mut fast);
            extend_from_millionths_scalar(chunk, &mut slow);
            assert_eq!(bits(&fast), bits(&slow));
        }
        for len in 0..=40 {
            for off in 0..4 {
                let codes = &all[(off + 1_000_003) * MILLIONTHS_BYTES..][..len * MILLIONTHS_BYTES];
                let mut got = vec![7.0];
                extend_from_millionths(codes, &mut got);
                let mut want = vec![7.0];
                extend_from_millionths_scalar(codes, &mut want);
                assert_eq!(bits(&got), bits(&want), "{len} at {off}");
                let mut back = vec![0; codes.len()];
                assert!(put_millionths(&got[1..], &mut back));
                assert_eq!(back, codes, "{len} at {off}");
                if len > 0 {
                    got[len] = 0.1 + 1e-7;
                    assert!(!put_millionths(&got[1..], &mut back), "{len} at {off}");
                }
            }
        }
    }

    #[test]
    fn varchar_counts_chars() {
        assert_eq!(char_len("事务与恢复"), 5);
        assert_eq!(char_len("ab😀"), 3);
    }
}
