//! Exact search index over a cached vector column. Rows are grouped into
//! lists around k-means centroids, and each list keeps an upper bound on its
//! members' distance to the centroid. By the triangle inequality no member
//! of a list is nearer to a query q than |q - c| - radius, so a TopK scan can
//! skip every list whose bound exceeds the K-th distance found so far and
//! still return exactly what a full scan returns. On clustered data most
//! lists are skipped; on uniform data the bounds prune nothing and the scan
//! visits every row, paying only the distances to the centroids.

use super::veccol::VecCol;
use crate::vector::{l2_distance, l2_sq_f32, F32Bound};

const NONE: u32 = u32::MAX;
/// Relative padding on every bound: far above the < 1e-13 relative error of
/// the f64 distances involved, far below any gap that matters.
const PAD: f64 = 1e-9;
const KMEANS_ROUNDS: usize = 10;
/// Training rows per list.
const SAMPLE_PER_LIST: usize = 32;

pub struct Ivf {
    dim: usize,
    centroids: Vec<f32>,
    /// At least |x - centroid| for every member x, padded.
    radius: Vec<f64>,
    lists: Vec<Vec<u32>>,
    /// List of each slot, NONE when the slot is not indexed.
    list_of: Vec<u32>,
}

impl Ivf {
    /// About sqrt(n) lists over the live slots of a vector column.
    /// Deterministic for the same input.
    pub fn build(vectors: &VecCol, live: &[bool]) -> Ivf {
        let dim = vectors.dim();
        let slots: Vec<u32> = (0..live.len()).filter(|&s| live[s]).map(|s| s as u32).collect();
        let n = slots.len();
        let nlist = ((n as f64).sqrt().round() as usize).clamp(1, 1024);
        // Rows in codes are decoded one at a time, never the whole column.
        let mut buf = Vec::with_capacity(dim);
        let sample = spread(&slots, (nlist * SAMPLE_PER_LIST).min(n));
        let mut centroids: Vec<f32> =
            spread(&sample, nlist).iter().flat_map(|&s| vectors.row(s as usize, &mut buf).to_vec()).collect();
        let mut assign = vec![0u32; sample.len()];
        for _ in 0..KMEANS_ROUNDS {
            for (a, &s) in assign.iter_mut().zip(&sample) {
                *a = nearest(&centroids, dim, vectors.row(s as usize, &mut buf));
            }
            let mut sum = vec![0f64; nlist * dim];
            let mut count = vec![0usize; nlist];
            for (&a, &s) in assign.iter().zip(&sample) {
                count[a as usize] += 1;
                for (acc, &x) in sum[a as usize * dim..][..dim].iter_mut().zip(vectors.row(s as usize, &mut buf)) {
                    *acc += f64::from(x);
                }
            }
            // A list that lost all its rows keeps its centroid.
            for l in (0..nlist).filter(|&l| count[l] > 0) {
                for d in 0..dim {
                    centroids[l * dim + d] = (sum[l * dim + d] / count[l] as f64) as f32;
                }
            }
        }
        let mut ivf = Ivf {
            dim,
            centroids,
            radius: vec![0.0; nlist],
            lists: vec![Vec::new(); nlist],
            list_of: vec![NONE; live.len()],
        };
        for &s in &slots {
            ivf.insert(s, vectors.row(s as usize, &mut buf));
        }
        ivf
    }

    fn centroid(&self, l: usize) -> &[f32] {
        &self.centroids[l * self.dim..][..self.dim]
    }

    /// Adds a slot. Any list would keep results exact (the radius grows to
    /// cover the row); the nearest one keeps the bounds tight.
    pub fn insert(&mut self, slot: u32, v: &[f32]) {
        let l = nearest(&self.centroids, self.dim, v) as usize;
        let d = l2_distance(v, self.centroid(l)) * (1.0 + PAD);
        self.radius[l] = self.radius[l].max(d);
        let s = slot as usize;
        if self.list_of.len() <= s {
            self.list_of.resize(s + 1, NONE);
        }
        self.list_of[s] = l as u32;
        self.lists[l].push(slot);
    }

    /// Removes a slot. The radius stays: it is still an upper bound.
    pub fn remove(&mut self, slot: u32) {
        let Some(l) = self.list_of.get(slot as usize).copied().filter(|&l| l != NONE) else {
            return;
        };
        self.list_of[slot as usize] = NONE;
        let list = &mut self.lists[l as usize];
        if let Some(p) = list.iter().position(|&s| s == slot) {
            list.swap_remove(p);
        }
    }

    pub fn list(&self, l: usize) -> &[u32] {
        &self.lists[l]
    }

    /// Slots list by list, then every slot of `0..len` that is not indexed:
    /// the order to lay rows out in so that each list is contiguous.
    pub fn slot_order(&self, len: usize) -> Vec<u32> {
        let mut order: Vec<u32> = self.lists.iter().flatten().copied().collect();
        order.extend((0..len as u32).filter(|&s| self.list_of.get(s as usize).is_none_or(|&l| l == NONE)));
        order
    }

    /// Renames every slot `s` to `new_slot[s]` after the rows were moved.
    pub fn remap(&mut self, new_slot: &[u32]) {
        let mut list_of = vec![NONE; new_slot.len()];
        for (l, list) in self.lists.iter_mut().enumerate() {
            for s in list.iter_mut() {
                *s = new_slot[*s as usize];
                list_of[*s as usize] = l as u32;
            }
        }
        self.list_of = list_of;
    }

    /// Non-empty lists by increasing lower bound on their members'
    /// `l2_distance` to `q`: a list whose bound exceeds a distance `d` holds
    /// no row at distance `d` or less.
    pub fn order(&self, q: &[f32]) -> Vec<(f64, usize)> {
        let bound = F32Bound::new(self.dim);
        let mut out: Vec<(f64, usize)> = (0..self.lists.len())
            .filter(|&l| !self.lists[l].is_empty())
            .map(|l| {
                // Shrinking the centroid distance by PAD, not the difference,
                // keeps the margin when it nearly cancels against the radius.
                let to_centroid = bound.lower(l2_sq_f32(q, self.centroid(l))).sqrt();
                (to_centroid * (1.0 - PAD) - self.radius[l], l)
            })
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    pub fn bytes(&self) -> usize {
        self.centroids.capacity() * 4
            + self.radius.capacity() * 8
            + self.list_of.capacity() * 4
            + self.lists.iter().map(|l| l.capacity() * 4 + size_of::<Vec<u32>>()).sum::<usize>()
    }
}

/// `k` items spread evenly over `items`.
fn spread(items: &[u32], k: usize) -> Vec<u32> {
    (0..k).map(|i| items[i * items.len() / k]).collect()
}

fn nearest(centroids: &[f32], dim: usize, v: &[f32]) -> u32 {
    let mut best = (f32::INFINITY, 0u32);
    for (l, c) in centroids.chunks_exact(dim).enumerate() {
        let d = l2_sq_f32(v, c);
        if d < best.0 {
            best = (d, l as u32);
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
    }

    /// Every member of every list is at least its list's bound away from the
    /// query, through inserts and removals after the build.
    #[test]
    fn bounds_hold_for_every_member() {
        let (dim, n) = (16, 3000);
        let mut rng = Rng(3);
        let centers: Vec<Vec<f32>> = (0..20).map(|_| (0..dim).map(|_| rng.unit() * 4.0).collect()).collect();
        let mut data: Vec<f32> = Vec::new();
        for i in 0..n {
            data.extend(centers[i % 20].iter().map(|c| c + rng.unit() * 0.3));
        }
        let mut live = vec![true; n];
        for s in (0..n).step_by(7) {
            live[s] = false;
        }
        let mut ivf = Ivf::build(&VecCol::from_floats(dim, &data), &live);
        for s in (0..n).step_by(5) {
            if live[s] {
                ivf.remove(s as u32);
                live[s] = false;
            }
        }
        for s in (0..n).step_by(7) {
            // Far from every centroid: the radius has to grow.
            let v: Vec<f32> = (0..dim).map(|_| rng.unit() * 20.0).collect();
            data[s * dim..][..dim].copy_from_slice(&v);
            ivf.insert(s as u32, &v);
            live[s] = true;
        }
        let indexed: usize = (0..ivf.lists.len()).map(|l| ivf.list(l).len()).sum();
        assert_eq!(indexed, live.iter().filter(|&&l| l).count());
        for _ in 0..50 {
            let q: Vec<f32> = (0..dim).map(|_| rng.unit() * 6.0).collect();
            for (lb, l) in ivf.order(&q) {
                for &s in ivf.list(l) {
                    let d = l2_distance(&q, &data[s as usize * dim..][..dim]);
                    assert!(d >= lb, "slot {s} at {d} below bound {lb}");
                }
            }
        }
    }
}
