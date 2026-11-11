//! Bounded selection of the K nearest rows, ordered by (distance, pk).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::vector::{l2_distance, l2_sq_f32, F32Bound};

/// Distance ordered by `total_cmp`; distances are never NaN.
#[derive(PartialEq)]
struct Dist(f64);

impl Eq for Dist {}

impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Dist {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Keeps the `k` smallest (distance, pk) pairs in a max-heap; ties on
/// distance go to the smaller pk. Each row is first measured with the
/// float32 kernel, and its exact distance is computed only when the error
/// bound says it could still place, so the result is the same as computing
/// every exact distance.
pub struct TopK<'q> {
    query: &'q [f32],
    k: usize,
    heap: BinaryHeap<(Dist, i64)>,
    bound: F32Bound,
    /// Float32 squared-distance limit derived from the current K-th distance.
    cutoff: f64,
    /// Rows whose exact distance was computed.
    pub exact_evals: usize,
}

impl<'q> TopK<'q> {
    pub fn new(query: &'q [f32], k: usize) -> TopK<'q> {
        TopK {
            query,
            k,
            heap: BinaryHeap::with_capacity(k.min(4096) + 1),
            bound: F32Bound::new(query.len()),
            cutoff: f64::INFINITY,
            exact_evals: 0,
        }
    }

    #[inline]
    pub fn offer(&mut self, pk: i64, v: &[f32]) {
        if self.k == 0 || !F32Bound::may_reach(l2_sq_f32(v, self.query), self.cutoff) {
            return;
        }
        self.exact_evals += 1;
        self.push((Dist(l2_distance(v, self.query)), pk));
    }

    #[inline(always)]
    fn push(&mut self, key: (Dist, i64)) {
        if self.heap.len() < self.k {
            self.heap.push(key);
        } else if self.heap.peek().is_some_and(|top| key < *top) {
            self.heap.pop();
            self.heap.push(key);
        } else {
            return;
        }
        if self.heap.len() == self.k {
            self.cutoff = self.heap.peek().map_or(f64::INFINITY, |top| self.bound.cutoff(top.0 .0));
        }
    }

    pub fn query(&self) -> &'q [f32] {
        self.query
    }

    pub fn k(&self) -> usize {
        self.k
    }

    /// Takes in the rows another TopK for the same query and `k` holds, as
    /// if they had been offered here.
    pub fn merge(&mut self, other: TopK<'_>) {
        self.exact_evals += other.exact_evals;
        for key in other.heap {
            self.push(key);
        }
    }

    /// Distance of the K-th row so far, once K rows are held: rows farther
    /// away can no longer place.
    pub fn kth(&self) -> Option<f64> {
        (self.k > 0 && self.heap.len() == self.k).then(|| self.heap.peek().map_or(f64::INFINITY, |top| top.0 .0))
    }

    /// Primary keys, nearest first.
    pub fn into_sorted(self) -> Vec<i64> {
        self.heap.into_sorted_vec().into_iter().map(|(_, pk)| pk).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn unit(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn brute_force(rows: &[(i64, Vec<f32>)], q: &[f32], k: usize) -> Vec<i64> {
        let mut all: Vec<(f64, i64)> = rows.iter().map(|(pk, v)| (l2_distance(v, q), *pk)).collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        all.into_iter().take(k).map(|(_, pk)| pk).collect()
    }

    fn top_k(rows: &[(i64, Vec<f32>)], q: &[f32], k: usize) -> (Vec<i64>, usize) {
        let mut t = TopK::new(q, k);
        for (pk, v) in rows {
            t.offer(*pk, v);
        }
        let evals = t.exact_evals;
        (t.into_sorted(), evals)
    }

    #[test]
    fn random_data_matches_brute_force_and_skips_most_rows() {
        let mut rng = Rng(42);
        let rows: Vec<(i64, Vec<f32>)> = (0..3000).map(|i| (i, (0..128).map(|_| rng.unit()).collect())).collect();
        for k in [1, 10, 100, 5000] {
            let q: Vec<f32> = (0..128).map(|_| rng.unit()).collect();
            let (got, evals) = top_k(&rows, &q, k);
            assert_eq!(got, brute_force(&rows, &q, k), "k = {k}");
            if k == 10 {
                assert!(evals < 300, "exact distance computed for {evals} of 3000 rows");
            }
        }
    }

    /// Many rows tie exactly or differ only in the last bit of one
    /// coordinate, in shuffled pk order, so the K-th place is contested by
    /// rows whose float32 distances are rounded differently.
    #[test]
    fn near_ties_match_brute_force() {
        let mut rng = Rng(7);
        for round in 0..200 {
            let dim = [3, 8, 33, 128][round % 4];
            let bases: Vec<Vec<f32>> = (0..4).map(|_| (0..dim).map(|_| rng.unit() * 100.0).collect()).collect();
            let mut rows: Vec<(i64, Vec<f32>)> = Vec::new();
            for i in 0..300 {
                let mut v = bases[rng.below(bases.len())].clone();
                if rng.below(2) == 0 {
                    let j = rng.below(dim);
                    let bits = v[j].to_bits();
                    v[j] = f32::from_bits(if rng.below(2) == 0 { bits + 1 } else { bits - 1 });
                }
                rows.push((((i * 7919) % 300) as i64 - 150, v));
            }
            let q: Vec<f32> = bases[0].iter().map(|x| x + rng.unit() * 0.01).collect();
            for k in [1, 5, 50] {
                assert_eq!(top_k(&rows, &q, k).0, brute_force(&rows, &q, k), "round {round} dim {dim} k {k}");
            }
        }
    }
}
