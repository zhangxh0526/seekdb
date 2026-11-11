//! 8-bit codes of a cached vector column, for exact TopK scans. Each
//! component x is stored as c = round(x / step) clamped to [-127, 127], with
//! one step for the whole column, and each row keeps an upper bound on
//! |x - step * c|, its distance to its own reconstruction. The query is coded
//! the same way, and by the triangle inequality
//!
//!   |q - x| >= step * |cq - cx| - |q - step * cq| - |x - step * cx|,
//!
//! where |cq - cx| is computed exactly in integers. A row whose bound exceeds
//! the K-th distance found so far cannot place, so a scan reads one byte per
//! component for most rows instead of four and still returns exactly what a
//! full scan returns. Rows outside the range the step was chosen for get
//! clamped codes and larger residuals: their bound is looser, never wrong.

use std::ops::Range;

use crate::vector::ssd_i8_scalar;
#[cfg(target_arch = "x86_64")]
use crate::vector::{ssd_i8_avx2, ssd_i8_short_avx2, SHORT_CODES};

/// Relative padding on every bound, as in the k-means index: far above the
/// rounding error of the f64 arithmetic involved, far below any gap that
/// matters.
const PAD: f64 = 1e-9;

pub struct Sq8 {
    dim: usize,
    step: f64,
    /// `dim` codes per slot.
    codes: Vec<i8>,
    /// At least |x - step * c| for each slot's vector.
    resid: Vec<f32>,
    /// At least every entry of `resid`.
    max_resid: f32,
}

impl Sq8 {
    /// Codes for every slot of a vector column (`data` holds `dim` floats per
    /// slot), with the step fitted to the live rows.
    pub fn build(dim: usize, data: &[f32], live: &[bool]) -> Sq8 {
        let max = (0..live.len())
            .filter(|&s| live[s])
            .flat_map(|s| &data[s * dim..][..dim])
            .fold(0f32, |m, &x| m.max(x.abs()));
        let step = if max > 0.0 { max as f64 / 127.0 } else { 1.0 };
        let mut sq = Sq8 {
            dim,
            step,
            codes: Vec::with_capacity(live.len() * dim),
            resid: Vec::with_capacity(live.len()),
            max_resid: 0.0,
        };
        for s in 0..live.len() {
            sq.set(s as u32, &data[s * dim..][..dim]);
        }
        sq
    }

    /// Codes the vector now in `slot`.
    pub fn set(&mut self, slot: u32, v: &[f32]) {
        let s = slot as usize;
        if self.resid.len() <= s {
            self.resid.resize(s + 1, 0.0);
            self.codes.resize((s + 1) * self.dim, 0);
        }
        let r = encode(self.step, v, &mut self.codes[s * self.dim..][..self.dim]);
        self.resid[s] = r;
        self.max_resid = self.max_resid.max(r);
    }

    pub fn query(&self, q: &[f32]) -> Sq8Query<'_> {
        let mut codes = vec![0i16; self.dim];
        let resid = encode(self.step, q, &mut codes) as f64;
        Sq8Query { sq: self, codes, resid, scale: 1.0 / (self.step * (1.0 - PAD)), quick: 0, reach: 0.0, active: false }
    }

    pub fn bytes(&self) -> usize {
        self.codes.capacity() + self.resid.capacity() * 4
    }
}

/// Codes `v` into `out` and returns an upper bound on |v - step * code|,
/// rounded up to f32 (+inf when it does not fit).
fn encode<T: From<i8>>(step: f64, v: &[f32], out: &mut [T]) -> f32 {
    let mut sum = 0f64;
    let mut big = 127.0 * step;
    for (o, &x) in out.iter_mut().zip(v) {
        let x = x as f64;
        let c = (x / step).round().clamp(-127.0, 127.0);
        *o = T::from(c as i8);
        let d = x - step * c;
        sum += d * d;
        big = big.max(x.abs());
    }
    // Each difference is off by at most 2^-51 * big in absolute terms; the
    // sum of squares and the square root add at most (len + 2) * 2^-53
    // relative.
    let len = v.len() as f64;
    let bound = sum.sqrt() * (1.0 + PAD + (len + 2.0) * 2f64.powi(-52)) + len * big * 2f64.powi(-50);
    let r = bound as f32;
    if (r as f64) < bound {
        r.next_up()
    } else {
        r
    }
}

/// One query against the codes. Pruning starts once `set_kth` is called.
pub struct Sq8Query<'a> {
    sq: &'a Sq8,
    codes: Vec<i16>,
    /// At least |q - step * cq|.
    resid: f64,
    /// Turns a distance into code units, shrunk by PAD.
    scale: f64,
    /// Rows whose code distance squared exceeds this cannot place, whatever
    /// their residual.
    quick: u64,
    /// K-th distance plus the query's residual.
    reach: f64,
    active: bool,
}

impl Sq8Query<'_> {
    /// Called with the K-th distance whenever it changes.
    pub fn set_kth(&mut self, kth: f64) {
        self.reach = kth + self.resid;
        let t = (self.reach + self.sq.max_resid as f64) * self.scale;
        // Rounding the threshold down only prunes less.
        self.quick = if t * t < 1.8e19 { (t * t) as u64 } else { u64::MAX };
        self.active = true;
    }

    /// Appends to `out` the slots of `slots` the bound cannot rule out:
    /// rows that may be as near as the K-th distance, ties included. Before
    /// `set_kth` that is every slot. `scattered` slots (a selection rather
    /// than a run of neighbours) have their codes fetched a few rows ahead.
    pub fn keep(&self, slots: &[u32], scattered: bool, out: &mut Vec<u32>) {
        if !self.active {
            out.extend_from_slice(slots);
            return;
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU supports AVX2, checked just above.
            return unsafe {
                if scattered {
                    self.keep_scattered_avx2(slots, out)
                } else {
                    self.keep_iter_avx2(slots.iter().map(|&s| s as usize), out)
                }
            };
        }
        self.keep_scalar(slots.iter().map(|&s| s as usize), out);
    }

    /// `keep` for the live slots of `range`.
    pub fn keep_live(&self, range: Range<usize>, live: &[bool], out: &mut Vec<u32>) {
        let slots = range.filter(|&s| live[s]);
        if !self.active {
            out.extend(slots.map(|s| s as u32));
            return;
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU supports AVX2, checked just above.
            return unsafe { self.keep_iter_avx2(slots, out) };
        }
        self.keep_scalar(slots, out);
    }

    /// The loops compiled for AVX2, so the kernel is inlined into them.
    /// Scattered slots have their codes fetched four rows ahead.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn keep_scattered_avx2(&self, slots: &[u32], out: &mut Vec<u32>) {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        let dim = self.sq.dim;
        for (i, &s) in slots.iter().enumerate() {
            if let Some(&next) = slots.get(i + 4) {
                let row = &self.sq.codes[next as usize * dim..][..dim];
                for off in (0..dim.min(256)).step_by(64).chain([dim - 1]) {
                    _mm_prefetch::<_MM_HINT_T0>(row.as_ptr().add(off));
                }
            }
            let s = s as usize;
            let ssd = self.ssd_avx2(s);
            if self.may_place(s, ssd) {
                out.push(s as u32);
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn keep_iter_avx2(&self, slots: impl Iterator<Item = usize>, out: &mut Vec<u32>) {
        for s in slots {
            let ssd = self.ssd_avx2(s);
            if self.may_place(s, ssd) {
                out.push(s as u32);
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn ssd_avx2(&self, slot: usize) -> u64 {
        let dim = self.sq.dim;
        let row = &self.sq.codes[slot * dim..][..dim];
        if dim < SHORT_CODES {
            ssd_i8_short_avx2(&self.codes, row)
        } else {
            ssd_i8_avx2(&self.codes, row)
        }
    }

    fn keep_scalar(&self, slots: impl Iterator<Item = usize>, out: &mut Vec<u32>) {
        let dim = self.sq.dim;
        for s in slots {
            if self.may_place(s, ssd_i8_scalar(&self.codes, &self.sq.codes[s * dim..][..dim])) {
                out.push(s as u32);
            }
        }
    }

    /// False when the row in `slot`, at code distance squared `ssd`, is
    /// provably farther than the K-th distance, so it cannot place, not even
    /// on a tie.
    #[inline(always)]
    fn may_place(&self, slot: usize, ssd: u64) -> bool {
        if ssd > self.quick {
            return false;
        }
        let t = (self.reach + self.sq.resid[slot] as f64) * self.scale;
        ssd as f64 <= t * t
    }

    /// One slot, for tests.
    #[cfg(test)]
    fn prunes(&self, slot: usize) -> bool {
        let mut out = Vec::new();
        self.keep(&[slot as u32], false, &mut out);
        out.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::l2_distance;

    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
    }

    /// A row is never pruned against its own exact distance, at any scale,
    /// for rows coded at build time and rows set later far outside the
    /// fitted range; and the bound does prune rows clearly farther away.
    #[test]
    fn never_prunes_a_row_that_could_place() {
        let mut rng = Rng(5);
        for &scale in &[1.0f32, 1e-3, 1e4, 1e-30, 1e30] {
            for dim in [1usize, 3, 16, 17, 128] {
                let n = 300;
                let mut data: Vec<f32> = (0..n * dim).map(|_| rng.unit() * scale).collect();
                let mut live = vec![true; n];
                live[7] = false;
                let mut sq = Sq8::build(dim, &data, &live);
                for s in (0..n).step_by(11) {
                    let v: Vec<f32> = (0..dim).map(|_| rng.unit() * scale * 50.0).collect();
                    data[s * dim..][..dim].copy_from_slice(&v);
                    sq.set(s as u32, &v);
                }
                let mut pruned = 0;
                for _ in 0..30 {
                    let q: Vec<f32> = (0..dim).map(|_| rng.unit() * scale * 2.0).collect();
                    for s in 0..n {
                        let d = l2_distance(&q, &data[s * dim..][..dim]);
                        let mut qq = sq.query(&q);
                        qq.set_kth(d);
                        assert!(!qq.prunes(s), "scale {scale} dim {dim} slot {s} pruned at its own distance {d}");
                        qq.set_kth(d * 0.5);
                        pruned += qq.prunes(s) as usize;
                    }
                }
                assert!(pruned > 0, "scale {scale} dim {dim}: the bound never pruned");
            }
        }
    }

    #[test]
    fn identical_rows_are_kept() {
        let v = [0.25f32, -1.0, 3.5];
        let sq = Sq8::build(3, &v, &[true]);
        let mut q = sq.query(&v);
        q.set_kth(0.0);
        assert!(!q.prunes(0));
        let zero = Sq8::build(2, &[0.0, 0.0], &[true]);
        let mut q = zero.query(&[0.0, 0.0]);
        q.set_kth(0.0);
        assert!(!q.prunes(0));
    }
}
