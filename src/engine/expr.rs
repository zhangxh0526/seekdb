//! Name resolution, type checking and evaluation of expressions.

use std::cmp::Ordering;
use std::ops::RangeInclusive;

use super::result::{cmp_int_float, Cell};
use super::table::{RowView, ValRef};
use crate::error::DbResult;
use crate::sql::ast::{CmpOp, Expr, Literal};
use crate::storage::catalog::{Column, TableDef};
use crate::types::{char_len, parse_vector, DataType, Value};
use crate::vector::l2_distance;
use crate::{bail, db_err};

/// Resolved expression.
#[derive(Debug, Clone)]
pub enum CExpr {
    Col(usize),
    Int(i64),
    Float(f64),
    Str(String),
    /// `L2_DISTANCE(column, constant)`.
    Dist { col: usize, query: Vec<f32> },
    /// `L2_DISTANCE` of two constants.
    ConstDist(f64),
    Cmp(CmpOp, Box<CExpr>, Box<CExpr>),
    /// Holds when every condition holds; none of them is an `And`.
    And(Vec<CExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Num,
    Str,
    Vector,
    Bool,
}

enum Scalar<'a> {
    Int(i64),
    Float(f64),
    Str(&'a str),
}

/// The table an expression is resolved against.
pub struct Scope<'a> {
    pub def: &'a TableDef,
    pub alias: Option<&'a str>,
}

impl Scope<'_> {
    pub fn column(&self, table: Option<&str>, name: &str) -> DbResult<usize> {
        if let Some(t) = table {
            let known = t.eq_ignore_ascii_case(&self.def.name) || self.alias.is_some_and(|a| a.eq_ignore_ascii_case(t));
            if !known {
                bail!(NoSuchColumn, "unknown column '{}.{}'", t, name);
            }
        }
        self.def.column(name).ok_or_else(|| db_err!(NoSuchColumn, "unknown column '{}'", name))
    }
}

pub fn compile(e: &Expr, scope: &Scope<'_>) -> DbResult<CExpr> {
    Ok(match e {
        Expr::Column { table, name } => CExpr::Col(scope.column(table.as_deref(), name)?),
        Expr::Lit(Literal::Int(v)) => CExpr::Int(*v),
        Expr::Lit(Literal::Num(s)) => CExpr::Float(parse_number(s)?),
        Expr::Lit(Literal::Str(s)) => CExpr::Str(s.clone()),
        Expr::Func { name, args } => compile_func(name, args, scope)?,
        Expr::Cmp { op, left, right } => {
            let l = compile(left, scope)?;
            let r = compile(right, scope)?;
            match (l.kind(scope.def), r.kind(scope.def)) {
                (Kind::Num, Kind::Num) | (Kind::Str, Kind::Str) => {}
                (Kind::Bool, _) | (_, Kind::Bool) => bail!(Unsupported, "comparing conditions is not supported"),
                (Kind::Vector, _) | (_, Kind::Vector) => bail!(TypeMismatch, "vectors cannot be compared"),
                _ => bail!(TypeMismatch, "cannot compare a number with a string"),
            }
            CExpr::Cmp(*op, Box::new(l), Box::new(r))
        }
        Expr::And(terms) => {
            let mut all = Vec::with_capacity(terms.len());
            for t in terms {
                match compile(t, scope)? {
                    CExpr::And(inner) => all.extend(inner),
                    c if c.kind(scope.def) == Kind::Bool => all.push(c),
                    _ => bail!(Unsupported, "AND operands must be conditions"),
                }
            }
            CExpr::And(all)
        }
    })
}

/// Compiles a WHERE clause, which must be a condition.
pub fn compile_filter(e: Option<&Expr>, scope: &Scope<'_>) -> DbResult<Option<CExpr>> {
    let Some(e) = e else { return Ok(None) };
    let c = compile(e, scope)?;
    if c.kind(scope.def) != Kind::Bool {
        bail!(Unsupported, "WHERE must be a comparison");
    }
    Ok(Some(c))
}

fn parse_number(s: &str) -> DbResult<f64> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => bail!(OutOfRange, "number '{}' is out of range", s),
    }
}

enum VecArg {
    Col(usize, usize),
    Lit(Vec<f32>),
}

fn compile_func(name: &str, args: &[Expr], scope: &Scope<'_>) -> DbResult<CExpr> {
    if name != "L2_DISTANCE" {
        bail!(Unsupported, "function {} is not supported", name);
    }
    if args.len() != 2 {
        bail!(Syntax, "L2_DISTANCE takes 2 arguments, got {}", args.len());
    }
    let vec_arg = |e: &Expr| -> DbResult<VecArg> {
        match e {
            Expr::Column { table, name } => {
                let col = scope.column(table.as_deref(), name)?;
                match scope.def.columns[col].ty {
                    DataType::Vector(dim) => Ok(VecArg::Col(col, dim as usize)),
                    _ => bail!(TypeMismatch, "L2_DISTANCE argument '{}' is not a vector", name),
                }
            }
            Expr::Lit(Literal::Str(s)) => Ok(VecArg::Lit(parse_vector(s)?)),
            _ => bail!(TypeMismatch, "L2_DISTANCE arguments must be a vector column or a vector literal"),
        }
    };
    match (vec_arg(&args[0])?, vec_arg(&args[1])?) {
        (VecArg::Col(col, dim), VecArg::Lit(query)) | (VecArg::Lit(query), VecArg::Col(col, dim)) => {
            if query.len() != dim {
                bail!(VectorDim, "vector dimension mismatch: column has {}, query has {}", dim, query.len());
            }
            Ok(CExpr::Dist { col, query })
        }
        (VecArg::Lit(a), VecArg::Lit(b)) => {
            if a.len() != b.len() || a.is_empty() {
                bail!(VectorDim, "vector dimension mismatch: {} vs {}", a.len(), b.len());
            }
            Ok(CExpr::ConstDist(l2_distance(&a, &b)))
        }
        (VecArg::Col(..), VecArg::Col(..)) => bail!(Unsupported, "L2_DISTANCE between two columns is not supported"),
    }
}

impl CExpr {
    pub fn kind(&self, def: &TableDef) -> Kind {
        match self {
            CExpr::Col(i) => match def.columns[*i].ty {
                DataType::BigInt => Kind::Num,
                DataType::Varchar(_) => Kind::Str,
                DataType::Vector(_) => Kind::Vector,
            },
            CExpr::Int(_) | CExpr::Float(_) | CExpr::Dist { .. } | CExpr::ConstDist(_) => Kind::Num,
            CExpr::Str(_) => Kind::Str,
            CExpr::Cmp(..) | CExpr::And(..) => Kind::Bool,
        }
    }

    fn scalar<'a>(&'a self, row: &RowView<'a>) -> Scalar<'a> {
        match self {
            CExpr::Col(i) => match row.get(*i) {
                ValRef::Int(v) => Scalar::Int(v),
                ValRef::Str(s) => Scalar::Str(s),
                ValRef::Vector(_) => Scalar::Int(0),
            },
            CExpr::Int(v) => Scalar::Int(*v),
            CExpr::Float(f) => Scalar::Float(*f),
            CExpr::Str(s) => Scalar::Str(s),
            CExpr::Dist { col, query } => Scalar::Float(match row.get(*col) {
                ValRef::Vector(v) => l2_distance(v.floats(&mut Vec::new()), query),
                _ => 0.0,
            }),
            CExpr::ConstDist(d) => Scalar::Float(*d),
            CExpr::Cmp(..) | CExpr::And(..) => Scalar::Int(self.test(row) as i64),
        }
    }

    /// Evaluates a condition.
    pub fn test(&self, row: &RowView<'_>) -> bool {
        match self {
            CExpr::Cmp(op, l, r) => holds(*op, compare(&l.scalar(row), &r.scalar(row))),
            CExpr::And(terms) => terms.iter().all(|e| e.test(row)),
            _ => false,
        }
    }

    /// Evaluates to an output value.
    pub fn cell(&self, row: &RowView<'_>) -> Cell {
        if let CExpr::Col(i) = self {
            return match row.get(*i) {
                ValRef::Int(v) => Cell::Int(v),
                ValRef::Str(s) => Cell::Str(s.to_string()),
                ValRef::Vector(v) => Cell::Vector(v.to_vec()),
            };
        }
        match self.scalar(row) {
            Scalar::Int(v) => Cell::Int(v),
            Scalar::Float(f) => Cell::Float(f),
            Scalar::Str(s) => Cell::Str(s.to_string()),
        }
    }
}

/// Whether `a op b` holds, given how `a` compares to `b`.
#[inline]
pub fn holds(op: CmpOp, ord: Ordering) -> bool {
    match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
    }
}

/// `column <op> constant` where both sides have the same type, so a cached
/// table can evaluate it directly on its column arrays.
pub struct ColPred<'a> {
    pub col: usize,
    pub op: CmpOp,
    pub value: PredValue<'a>,
}

pub enum PredValue<'a> {
    Int(i64),
    Str(&'a str),
}

impl ColPred<'_> {
    /// Same result as the `CExpr` it came from.
    pub fn test(&self, row: &RowView<'_>) -> bool {
        match (row.get(self.col), &self.value) {
            (ValRef::Int(x), PredValue::Int(v)) => holds(self.op, x.cmp(v)),
            (ValRef::Str(x), PredValue::Str(v)) => holds(self.op, x.as_bytes().cmp(v.as_bytes())),
            _ => false,
        }
    }
}

/// Splits a filter's AND-ed conditions into column predicates and the rest.
/// The filter holds exactly when every predicate and every rest condition
/// holds.
pub fn split_filter(filter: Option<&CExpr>) -> (Vec<ColPred<'_>>, Vec<&CExpr>) {
    fn walk<'a>(e: &'a CExpr, preds: &mut Vec<ColPred<'a>>, rest: &mut Vec<&'a CExpr>) {
        match e {
            CExpr::And(terms) => {
                for e in terms {
                    walk(e, preds, rest);
                }
            }
            CExpr::Cmp(op, l, r) => {
                let (col, op, lit) = match (&**l, &**r) {
                    (CExpr::Col(c), lit) => (*c, *op, lit),
                    (lit, CExpr::Col(c)) => (*c, flip(*op), lit),
                    _ => return rest.push(e),
                };
                let value = match lit {
                    CExpr::Int(v) => PredValue::Int(*v),
                    CExpr::Str(s) => PredValue::Str(s),
                    _ => return rest.push(e),
                };
                preds.push(ColPred { col, op, value });
            }
            _ => rest.push(e),
        }
    }
    let (mut preds, mut rest) = (Vec::new(), Vec::new());
    if let Some(f) = filter {
        walk(f, &mut preds, &mut rest);
    }
    (preds, rest)
}

fn compare(a: &Scalar<'_>, b: &Scalar<'_>) -> Ordering {
    match (a, b) {
        (Scalar::Int(x), Scalar::Int(y)) => x.cmp(y),
        (Scalar::Float(x), Scalar::Float(y)) => x.total_cmp(y),
        (Scalar::Int(x), Scalar::Float(y)) => cmp_int_float(*x, *y),
        (Scalar::Float(x), Scalar::Int(y)) => cmp_int_float(*y, *x).reverse(),
        (Scalar::Str(x), Scalar::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => Ordering::Equal,
    }
}

/// Inclusive primary-key bounds implied by `pk <op> integer` conjuncts of a
/// filter, or `None` when they cannot all hold.
pub fn pk_range(filter: Option<&CExpr>, pk: usize) -> Option<RangeInclusive<i64>> {
    fn walk(e: &CExpr, pk: usize, lo: &mut i64, hi: &mut i64, empty: &mut bool) {
        match e {
            CExpr::And(terms) => {
                for e in terms {
                    walk(e, pk, lo, hi, empty);
                }
            }
            CExpr::Cmp(op, l, r) => {
                let (op, v) = match (&**l, &**r) {
                    (CExpr::Col(c), CExpr::Int(v)) if *c == pk => (*op, *v),
                    (CExpr::Int(v), CExpr::Col(c)) if *c == pk => (flip(*op), *v),
                    _ => return,
                };
                match op {
                    CmpOp::Eq => {
                        *lo = (*lo).max(v);
                        *hi = (*hi).min(v);
                    }
                    CmpOp::Lt => match v.checked_sub(1) {
                        Some(x) => *hi = (*hi).min(x),
                        None => *empty = true,
                    },
                    CmpOp::Le => *hi = (*hi).min(v),
                    CmpOp::Gt => match v.checked_add(1) {
                        Some(x) => *lo = (*lo).max(x),
                        None => *empty = true,
                    },
                    CmpOp::Ge => *lo = (*lo).max(v),
                    CmpOp::Ne => {}
                }
            }
            _ => {}
        }
    }
    let (mut lo, mut hi, mut empty) = (i64::MIN, i64::MAX, false);
    if let Some(f) = filter {
        walk(f, pk, &mut lo, &mut hi, &mut empty);
    }
    (!empty && lo <= hi).then_some(lo..=hi)
}

fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        o => o,
    }
}

/// Converts a literal in INSERT/UPDATE to a value of the column's type.
pub fn literal_value(e: &Expr, col: &Column) -> DbResult<Value> {
    let Expr::Lit(lit) = e else {
        bail!(Unsupported, "only literal values can be stored (column '{}')", col.name);
    };
    match (col.ty, lit) {
        (DataType::BigInt, Literal::Int(v)) => Ok(Value::Int(*v)),
        (DataType::BigInt, Literal::Num(s)) => {
            if !s.contains(['.', 'e', 'E']) {
                bail!(OutOfRange, "value {} is out of range for column '{}'", s, col.name);
            }
            let f = parse_number(s)?;
            if f.fract() != 0.0 {
                bail!(InvalidValue, "incorrect integer value '{}' for column '{}'", s, col.name);
            }
            if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
                bail!(OutOfRange, "value {} is out of range for column '{}'", s, col.name);
            }
            Ok(Value::Int(f as i64))
        }
        (DataType::Varchar(n), Literal::Str(s)) => {
            if char_len(s) > n as usize {
                bail!(DataTooLong, "data too long for column '{}'", col.name);
            }
            Ok(Value::Str(s.clone()))
        }
        (DataType::Vector(dim), Literal::Str(s)) => {
            let v = parse_vector(s)?;
            if v.len() != dim as usize {
                bail!(VectorDim, "vector dimension mismatch for column '{}': expected {}, got {}", col.name, dim, v.len());
            }
            Ok(Value::Vector(v))
        }
        (DataType::BigInt, Literal::Str(_)) => bail!(TypeMismatch, "incorrect integer value for column '{}'", col.name),
        (_, _) => bail!(TypeMismatch, "column '{}' expects a quoted string", col.name),
    }
}
