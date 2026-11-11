//! Row encoding inside log records. The primary key is stored in the record
//! header, so the row body holds the other columns in schema order:
//! BIGINT as 8 bytes LE, VARCHAR as varint length + UTF-8, VECTOR(n) as a
//! tag byte and then either n 24-bit codes LE (`types::millionths`), when
//! every element has one, or n float32 LE.

use super::catalog::TableDef;
use super::codec::{self, Reader};
use crate::error::DbResult;
use crate::types::{from_millionths, millionths, DataType, Value};
use crate::{bail, db_err};

const VECTOR_F32: u8 = 0;
const VECTOR_MILLIONTHS: u8 = 1;

pub fn encode_row(def: &TableDef, row: &[Value], out: &mut Vec<u8>) -> DbResult<()> {
    if row.len() != def.columns.len() {
        bail!(Internal, "row has {} values, table {} has {} columns", row.len(), def.name, def.columns.len());
    }
    for (i, (col, v)) in def.columns.iter().zip(row).enumerate() {
        if i == def.pk {
            continue;
        }
        match (col.ty, v) {
            (DataType::BigInt, Value::Int(x)) => codec::put_i64(out, *x),
            (DataType::Varchar(_), Value::Str(s)) => codec::put_str(out, s),
            (DataType::Vector(dim), Value::Vector(v)) if v.len() == dim as usize => put_vector(out, v),
            _ => bail!(Internal, "value for column {} does not match type {}", col.name, col.ty),
        }
    }
    Ok(())
}

pub fn decode_row(def: &TableDef, pk: i64, bytes: &[u8]) -> DbResult<Vec<Value>> {
    let mut r = Reader::new(bytes);
    let mut row = Vec::with_capacity(def.columns.len());
    for (i, col) in def.columns.iter().enumerate() {
        if i == def.pk {
            row.push(Value::Int(pk));
            continue;
        }
        row.push(match col.ty {
            DataType::BigInt => Value::Int(r.i64()?),
            DataType::Varchar(_) => Value::Str(r.str()?.to_string()),
            DataType::Vector(dim) => Value::Vector(match r.u8()? {
                VECTOR_MILLIONTHS => {
                    let raw = r.take(dim as usize * 3)?;
                    raw.as_chunks::<3>().0.iter().map(|&[a, b, c]| from_millionths(u32::from_le_bytes([a, b, c, 0]))).collect()
                }
                VECTOR_F32 => {
                    let raw = r.take(dim as usize * 4)?;
                    raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
                }
                tag => bail!(Corrupt, "vector tag {} in row of table {}", tag, def.name),
            }),
        });
    }
    if r.remaining() != 0 {
        return Err(db_err!(Corrupt, "{} trailing bytes in row of table {}", r.remaining(), def.name));
    }
    Ok(row)
}

/// The check pass has no stores, so it vectorizes; codes are written only
/// once every element is known to have one.
fn put_vector(out: &mut Vec<u8>, v: &[f32]) {
    if v.iter().fold(true, |ok, &x| ok & millionths(x).1) {
        let at = out.len();
        out.resize(at + 1 + v.len() * 3, 0);
        out[at] = VECTOR_MILLIONTHS;
        for (b, &x) in out[at + 1..].as_chunks_mut::<3>().0.iter_mut().zip(v) {
            b.copy_from_slice(&millionths(x).0.to_le_bytes()[..3]);
        }
    } else {
        out.reserve(1 + v.len() * 4);
        out.push(VECTOR_F32);
        for f in v {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::Column;

    fn docs() -> TableDef {
        TableDef {
            id: 1,
            name: "docs".into(),
            columns: vec![
                Column { name: "id".into(), ty: DataType::BigInt },
                Column { name: "category".into(), ty: DataType::Varchar(32) },
                Column { name: "embedding".into(), ty: DataType::Vector(3) },
                Column { name: "n".into(), ty: DataType::BigInt },
            ],
            pk: 0,
            vector_index: None,
        }
    }

    #[test]
    fn roundtrip() {
        let def = docs();
        let row = vec![
            Value::Int(42),
            Value::Str("向量索引".into()),
            Value::Vector(vec![1.0, -0.5, 3.25]),
            Value::Int(i64::MIN),
        ];
        let mut buf = Vec::new();
        encode_row(&def, &row, &mut buf).unwrap();
        assert_eq!(buf.len(), 1 + 12 + 1 + 9 + 8);
        assert_eq!(buf[13], VECTOR_MILLIONTHS);
        assert_eq!(decode_row(&def, 42, &buf).unwrap(), row);
        assert!(decode_row(&def, 42, &buf[..buf.len() - 1]).is_err());
        buf[13] = 2;
        assert!(decode_row(&def, 42, &buf).is_err());
    }

    /// One element without a 24-bit code stores the whole vector as float32,
    /// and every element, -0.0 included, comes back bit for bit.
    #[test]
    fn vector_falls_back_to_f32() {
        let def = docs();
        for (v, packed) in [
            (vec![-0.0, 0.123456, -8.388607], true),
            (vec![-0.0, 0.123456, 8.388608], false),
            (vec![0.1, 1e-7, -0.0], false),
            (vec![0.5, f32::MAX, 0.25], false),
            (vec![0.1234567, 0.5, 0.25], false),
        ] {
            let row = vec![Value::Int(7), Value::Str(String::new()), Value::Vector(v.clone()), Value::Int(-1)];
            let mut buf = Vec::new();
            encode_row(&def, &row, &mut buf).unwrap();
            assert_eq!(buf.len(), 1 + 1 + if packed { 9 } else { 12 } + 8, "{v:?}");
            assert_eq!(buf[1], if packed { VECTOR_MILLIONTHS } else { VECTOR_F32 }, "{v:?}");
            let Value::Vector(got) = &decode_row(&def, 7, &buf).unwrap()[2] else { panic!() };
            assert_eq!(got.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), v.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn rejects_mismatched_values() {
        let def = docs();
        let mut buf = Vec::new();
        let bad_dim = vec![Value::Int(1), Value::Str("a".into()), Value::Vector(vec![1.0]), Value::Int(0)];
        assert!(encode_row(&def, &bad_dim, &mut buf).is_err());
        assert!(encode_row(&def, &bad_dim[..2], &mut buf).is_err());
    }
}
