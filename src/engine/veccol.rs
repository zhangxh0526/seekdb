//! The row cache's vector columns, `dim` elements per slot. A column holds
//! the 24-bit codes of `types::millionths`, three bytes per element instead
//! of four, as long as every vector stored in it has them (a vector written
//! with six decimals and magnitudes under 8.39 does). The first vector that
//! does not turns the column into float32 for good, as too many distinct
//! strings end a string column's dictionary. Either way every vector reads
//! back bit for bit.

use crate::types::{extend_from_millionths, put_millionths, MILLIONTHS_BYTES};

pub struct VecCol {
    dim: usize,
    data: Data,
}

enum Data {
    Codes(Vec<u8>),
    F32(Vec<f32>),
}

/// One stored vector, of a cache slot or of a decoded row.
#[derive(Clone, Copy)]
pub enum VecRef<'a> {
    F32(&'a [f32]),
    Codes(&'a [u8]),
}

impl<'a> VecRef<'a> {
    /// The elements: in place, or decoded into `buf`.
    #[inline]
    pub fn floats<'b>(self, buf: &'b mut Vec<f32>) -> &'b [f32]
    where
        'a: 'b,
    {
        match self {
            VecRef::F32(v) => v,
            VecRef::Codes(c) => {
                buf.clear();
                extend_from_millionths(c, buf);
                buf
            }
        }
    }

    pub fn to_vec(self) -> Vec<f32> {
        match self {
            VecRef::F32(v) => v.to_vec(),
            VecRef::Codes(c) => {
                let mut v = Vec::with_capacity(c.len() / MILLIONTHS_BYTES);
                extend_from_millionths(c, &mut v);
                v
            }
        }
    }
}

impl VecCol {
    pub fn new(dim: usize) -> VecCol {
        VecCol { dim, data: Data::Codes(Vec::new()) }
    }

    /// A column holding `data`, `dim` floats per slot.
    #[cfg(test)]
    pub fn from_floats(dim: usize, data: &[f32]) -> VecCol {
        let mut col = VecCol::new(dim);
        for (slot, v) in data.chunks_exact(dim).enumerate() {
            col.grow();
            col.set(slot, v);
        }
        col
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Whether the column is still in codes.
    #[cfg(test)]
    pub fn is_coded(&self) -> bool {
        matches!(self.data, Data::Codes(_))
    }

    /// Adds a slot holding zeros.
    pub fn grow(&mut self) {
        match &mut self.data {
            Data::Codes(c) => c.resize(c.len() + self.dim * MILLIONTHS_BYTES, 0),
            Data::F32(f) => f.resize(f.len() + self.dim, 0.0),
        }
    }

    pub fn reserve_exact(&mut self, slots: usize) {
        match &mut self.data {
            Data::Codes(c) => c.reserve_exact(slots * self.dim * MILLIONTHS_BYTES),
            Data::F32(f) => f.reserve_exact(slots * self.dim),
        }
    }

    pub fn set(&mut self, slot: usize, v: &[f32]) {
        let dim = self.dim;
        if let Data::Codes(c) = &mut self.data {
            if put_millionths(v, &mut c[slot * dim * MILLIONTHS_BYTES..][..dim * MILLIONTHS_BYTES]) {
                return;
            }
            // The slot's bytes are overwritten below.
            let mut f = Vec::with_capacity(c.capacity() / MILLIONTHS_BYTES);
            extend_from_millionths(c, &mut f);
            self.data = Data::F32(f);
        }
        if let Data::F32(f) = &mut self.data {
            f[slot * dim..][..dim].copy_from_slice(v);
        }
    }

    #[inline]
    pub fn get(&self, slot: usize) -> VecRef<'_> {
        match &self.data {
            Data::Codes(c) => VecRef::Codes(&c[slot * self.dim * MILLIONTHS_BYTES..][..self.dim * MILLIONTHS_BYTES]),
            Data::F32(f) => VecRef::F32(&f[slot * self.dim..][..self.dim]),
        }
    }

    /// The slot's elements: in place, or decoded into `buf`.
    #[inline]
    pub fn row<'b>(&'b self, slot: usize, buf: &'b mut Vec<f32>) -> &'b [f32] {
        self.get(slot).floats(buf)
    }

    pub fn bytes(&self) -> usize {
        match &self.data {
            Data::Codes(c) => c.capacity(),
            Data::F32(f) => f.capacity() * 4,
        }
    }

    /// Moves the vector in slot `order[i]` to slot `i`; `order` is a
    /// permutation of all slots. In place, one cycle at a time, so the
    /// column is never copied whole.
    pub fn permute(&mut self, order: &[u32]) {
        match &mut self.data {
            Data::Codes(c) => permute_rows(c, self.dim * MILLIONTHS_BYTES, order),
            Data::F32(f) => permute_rows(f, self.dim, order),
        }
    }
}

fn permute_rows<T: Copy>(data: &mut [T], width: usize, order: &[u32]) {
    let Some(first) = data.get(..width) else {
        return;
    };
    let mut done = vec![false; order.len()];
    let mut tmp = first.to_vec();
    for start in 0..order.len() {
        if done[start] {
            continue;
        }
        tmp.copy_from_slice(&data[start * width..][..width]);
        let mut j = start;
        loop {
            done[j] = true;
            let k = order[j] as usize;
            if k == start {
                data[j * width..][..width].copy_from_slice(&tmp);
                break;
            }
            data.copy_within(k * width..(k + 1) * width, j * width);
            j = k;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(v: &[f32]) -> Vec<u32> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    /// Codes until a vector without them, float32 from then on, through
    /// overwrites and permutations; every slot reads back bit for bit.
    #[test]
    fn codes_then_floats() {
        let mut col = VecCol::new(3);
        let mut model: Vec<Vec<f32>> = Vec::new();
        let mut buf = Vec::new();
        let check = |col: &VecCol, model: &[Vec<f32>], buf: &mut Vec<f32>| {
            for (s, v) in model.iter().enumerate() {
                assert_eq!(bits(col.row(s, buf)), bits(v), "slot {s}");
                assert_eq!(bits(&col.get(s).to_vec()), bits(v), "slot {s}");
            }
        };
        for s in 0..40 {
            // What parsing -1.875000, -0.000000, 0.012345 * s gives.
            let v = vec![s as f32 / 8.0 - 2.0, -0.0, (s * 12_345) as f32 / 1e6];
            col.grow();
            col.set(s, &v);
            model.push(v);
        }
        assert!(col.is_coded());
        assert!(matches!(col.get(3), VecRef::Codes(c) if c.len() == 9));
        check(&col, &model, &mut buf);
        let order: Vec<u32> = (0..40).map(|i| (i * 17 % 40) as u32).collect();
        col.permute(&order);
        model = order.iter().map(|&s| model[s as usize].clone()).collect();
        check(&col, &model, &mut buf);
        model[5] = vec![1.0, 2.0, 3.0];
        col.set(5, &model[5]);
        assert!(col.is_coded());
        model[9] = vec![8.5, 0.0, -1e-7];
        col.set(9, &model[9]);
        assert!(!col.is_coded());
        assert!(matches!(col.get(3), VecRef::F32(f) if f.len() == 3));
        check(&col, &model, &mut buf);
        col.permute(&order);
        model = order.iter().map(|&s| model[s as usize].clone()).collect();
        check(&col, &model, &mut buf);
        col.grow();
        model.push(vec![0.0; 3]);
        check(&col, &model, &mut buf);
    }

    #[test]
    fn one_vector_without_codes_turns_the_column() {
        assert!(VecCol::from_floats(4, &[0.5; 40]).is_coded());
        assert!(VecCol::from_floats(4, &[0.1; 40]).is_coded());
        let mut v = [0.1f32; 40];
        v[37] = 0.1 + 1e-7;
        assert!(!VecCol::from_floats(4, &v).is_coded());
    }
}
